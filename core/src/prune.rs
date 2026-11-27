//! AdGuard's `$jsonprune`: rules that take ad entries out of a site's JSON responses (X's
//! promoted posts, YouTube's ad slots) before the page's own code ever sees them.
//!
//! adblock-rust doesn't know the modifier and drops these rules, so lists.rs pulls them out
//! of the list text while lists compile and keeps them next to the saved engine
//! (`engine.prune`). A rule is applied exactly as written or not at all: an address pattern,
//! modifier, or part of a path this file can't evaluate drops the whole rule (listed in the
//! detailed log), so nothing is ever pruned more broadly than the list asked for.
//!
//! Paths are the JSONPath subset AdGuard documents: `$`, `.key`, `['key']`, `[a, b]`, `*`,
//! `..` (any depth), and the filters `[?(has k)]`, `[?(key-eq k v)]`, `[?(key-substr k v)]`,
//! where `k` is a key or a nested path (`"$.a.b"`). `[?(@.a.b)]` (standard JSONPath, used by
//! Zen's lists) means the same as `[?(has "$.a.b")]`. A filter tests the children of the
//! node before it, and the children that pass are removed.
//!
//! <https://adguard.com/kb/general/ad-filtering/create-own-filters/#jsonprune-modifier>

use crate::{
    log::{Trace, without_query},
    net::on_domain_list,
};
use hudsucker::{
    Body, decode_response,
    futures::{StreamExt, stream},
    hyper::{
        Response, StatusCode,
        body::Bytes,
        header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue},
    },
};
use http_body_util::BodyDataStream;
use serde_json::Value;
use std::sync::Arc;

/// Responses bigger than this pass through untouched, like AdGuard's 10 MB limit.
const MAX_JSON_BYTES: usize = 10 * 1024 * 1024;

// =========================================================================
// RULES
// =========================================================================

/// Every `$jsonprune` rule and exception from the enabled lists, plus the ones that couldn't
/// be used and why.
#[derive(Default)]
pub struct Rules {
    rules:      Vec<Rule>,
    exceptions: Vec<Exception>,
    dropped:    Vec<String>,
}

/// One `$jsonprune` rule.
struct Rule {
    when:       When,
    /// The path as written (escapes undone): rules run in this order, and exceptions name it.
    expression: String,
    path:       Path,
}

/// `@@...$jsonprune` (every rule for those addresses) or `@@...$jsonprune=path` (that one).
struct Exception {
    when:       When,
    expression: Option<String>,
}

/// Which requests a rule or exception is for.
struct When {
    pattern:  Pattern,
    domains:  Domains,
    /// `$xmlhttprequest`: only fetches made by page code.
    xhr_only: bool,
}

impl When {
    fn applies(&self, url: &str, site: &str, kind: &str) -> bool {
        (!self.xhr_only || kind == "xmlhttprequest") && self.domains.allows(site) && self.pattern.matches(url)
    }
}

/// What one line of a list is, as far as `$jsonprune` goes.
enum Line {
    Rule(Rule),
    Exception(Exception),
}

impl Rules {
    /// Read the `$jsonprune` lines of the enabled lists (other lines are skipped).
    pub fn parse(text: &str) -> Self {
        let mut rules = Self::default();
        for line in text.lines().map(str::trim) {
            match parse_line(line) {
                Ok(Some(Line::Rule(rule))) => rules.rules.push(rule),
                Ok(Some(Line::Exception(exception))) => rules.exceptions.push(exception),
                Ok(None) => {}
                Err(why) => rules.dropped.push(format!("{why}: {line}")),
            }
        }
        rules
    }

    /// How many rules are in use.
    pub fn count(&self) -> usize {
        self.rules.len()
    }

    /// Put how many rules are in use, and each one that couldn't be used, in the detailed log.
    pub fn log_dropped(&self, trace: &Trace) {
        trace.write(format_args!("jsonprune: {} rules and {} exceptions in use, {} dropped", self.count(), self.exceptions.len(), self.dropped.len()));
        for why in &self.dropped {
            trace.write(format_args!("jsonprune rule dropped ({why})"));
        }
    }
}

/// The rules one request's response gets, in AdGuard's order (sorted by path).
pub struct Matched {
    rules: Arc<Rules>,
    which: Vec<usize>,
    /// The request's address, for the detailed log.
    url:   String,
}

impl Matched {
    /// The rules for a request to `url`, made by a page on `source` (its referer or origin),
    /// of adblock request type `kind`. None for nearly every request, which then costs a
    /// few short comparisons and nothing on its response.
    pub fn find(rules: &Arc<Rules>, url: &str, source: &str, kind: &str) -> Option<Self> {
        if rules.rules.is_empty() {
            return None;
        }
        let site = host_of(source);
        let mut which: Vec<usize> = (0..rules.rules.len())
            .filter(|&i| rules.rules[i].when.applies(url, site, kind))
            .filter(|&i| {
                let expression = &rules.rules[i].expression;
                !rules.exceptions.iter().any(|e| e.expression.as_ref().is_none_or(|x| x == expression) && e.when.applies(url, site, kind))
            })
            .collect();
        if which.is_empty() {
            return None;
        }
        which.sort_by(|a, b| rules.rules[*a].expression.cmp(&rules.rules[*b].expression));
        which.dedup_by(|a, b| rules.rules[*a].expression == rules.rules[*b].expression);
        Some(Self { rules: rules.clone(), which, url: url.to_string() })
    }

    fn paths(&self) -> impl Iterator<Item = &Path> {
        self.which.iter().map(|&i| &self.rules.rules[i].path)
    }
}

/// The host of an address, without copying it.
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let auth = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let auth = auth.rsplit_once('@').map_or(auth, |(_, host)| host);
    auth.split(':').next().unwrap_or(auth)
}

/// Undo the list escapes: `\$` and `\,` (needed because `$` and `,` separate modifiers).
fn unescape(value: &str) -> String {
    value.replace("\\$", "$").replace("\\,", ",")
}

/// Split at `sep` where it isn't escaped with `\`.
fn split_unescaped(text: &str, sep: char) -> Vec<&str> {
    let mut parts = vec![];
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if c == '\\' {
            chars.next();
        } else if c == sep {
            parts.push(&text[start..at]);
            start = at + c.len_utf8();
        }
    }
    parts.push(&text[start..]);
    parts
}

/// One list line: Ok(None) when it isn't a `$jsonprune` rule, Err with the reason when it is
/// one that can't be applied exactly as written.
fn parse_line(line: &str) -> Result<Option<Line>, String> {
    if line.is_empty() || line.starts_with(['!', '#', '[']) || !line.contains("jsonprune") || line.contains("##") || line.contains("#@#") {
        return Ok(None);
    }
    let (exception, line) = line.strip_prefix("@@").map_or((false, line), |rest| (true, rest));

    // Modifiers start at the first `$` that isn't escaped: paths inside them start with `$`
    // too (some lists don't escape it), so the last one can't be used
    let parts = split_unescaped(line, '$');
    if parts.len() < 2 {
        return Ok(None);
    }
    let pattern = parts[0];
    let options = &line[pattern.len() + 1..];

    let mut expression = None;
    let mut prune      = false;
    let mut domains    = Domains::default();
    let mut xhr_only   = false;
    let mut match_case = false;
    for option in split_unescaped(options, ',') {
        let (name, value) = option.split_once('=').map_or((option.trim(), None), |(n, v)| (n.trim(), Some(v)));
        match name {
            "jsonprune" => {
                prune      = true;
                expression = value.map(unescape);
            }
            "domain" => domains = Domains::parse(value.unwrap_or(""))?,
            "xmlhttprequest" | "xhr" => xhr_only = true,
            // Every request type: no limit to apply
            "all" => {}
            "match-case" => match_case = true,
            other => return Err(format!("the ${other} modifier isn't supported with $jsonprune")),
        }
    }
    if !prune {
        return Ok(None);
    }

    let when = When { pattern: Pattern::parse(pattern, match_case)?, domains, xhr_only };
    if exception {
        return Ok(Some(Line::Exception(Exception { when, expression })));
    }
    let expression = expression.ok_or("$jsonprune needs a path")?;
    let path       = Path::parse(&expression)?;
    if path.0.is_empty() {
        return Err("the path is the whole response".into());
    }
    Ok(Some(Line::Rule(Rule { when, expression, path })))
}

// =========================================================================
// ADDRESSES AND SITES
// =========================================================================

/// An Adblock Plus address pattern: `||` (the site or a subdomain), `|` (start or end of the
/// address), `*` (anything), `^` (a separator or the end); everything else is itself. Letter
/// case is ignored unless the rule says `$match-case`.
struct Pattern {
    /// With the implied `*` at either end written out.
    glob:       Vec<u8>,
    /// `||`: the glob is tried from the start of the host and after each `.` in it.
    host:       bool,
    match_case: bool,
}

impl Pattern {
    fn parse(pattern: &str, match_case: bool) -> Result<Self, String> {
        if pattern.len() > 1 && pattern.starts_with('/') && pattern.ends_with('/') {
            return Err("regular expression addresses aren't supported".into());
        }
        let (host, rest)  = pattern.strip_prefix("||").map_or((false, pattern), |rest| (true, rest));
        let (start, rest) = if host { (true, rest) } else { rest.strip_prefix('|').map_or((false, rest), |rest| (true, rest)) };
        let (end, rest)   = rest.strip_suffix('|').map_or((false, rest), |rest| (true, rest));

        let mut glob = Vec::with_capacity(rest.len() + 2);
        if !start {
            glob.push(b'*');
        }
        glob.extend_from_slice(rest.as_bytes());
        if !end {
            glob.push(b'*');
        }
        Ok(Self { glob, host, match_case })
    }

    fn matches(&self, url: &str) -> bool {
        let url = url.as_bytes();
        if !self.host {
            return glob(&self.glob, url, self.match_case);
        }
        // Where a site name can begin: the host's start, and right after each of its dots
        let Some(start) = url.windows(3).position(|w| w == b"://").map(|at| at + 3) else { return false };
        let end = url[start..].iter().position(|b| matches!(b, b'/' | b'?' | b'#' | b':')).map_or(url.len(), |at| start + at);
        std::iter::once(start)
            .chain((start..end).filter(|&at| url[at] == b'.').map(|at| at + 1))
            .any(|at| glob(&self.glob, &url[at..], self.match_case))
    }
}

/// A character `^` stands for: anything but a letter, a digit, or `_ - . %`.
fn is_separator(b: u8) -> bool {
    !(b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'%'))
}

/// Match a whole address against a glob with `*` and `^`. On a mismatch it goes back to the
/// last `*` and lets it take one more character, so it never backtracks further than that.
fn glob(pattern: &[u8], text: &[u8], match_case: bool) -> bool {
    let same = |p: u8, t: u8| if match_case { p == t } else { p.eq_ignore_ascii_case(&t) };
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < text.len() {
        if pi < pattern.len() && pattern[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi  += 1;
        } else if pi < pattern.len() && (same(pattern[pi], text[ti]) || (pattern[pi] == b'^' && is_separator(text[ti]))) {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi    = s + 1;
            mark += 1;
            ti    = mark;
        } else {
            return false;
        }
    }
    // What's left can only be stars, or a `^` standing for the end of the address
    pattern[pi..].iter().all(|&c| c == b'*' || c == b'^')
}

/// `$domain=a.com|~b.a.com`: the sites the page making the request must (or mustn't) be on.
#[derive(Default)]
struct Domains {
    include: Vec<String>,
    exclude: Vec<String>,
}

impl Domains {
    fn parse(value: &str) -> Result<Self, String> {
        let mut domains = Self::default();
        for entry in value.split('|').map(str::trim).filter(|e| !e.is_empty()) {
            let (negated, site) = entry.strip_prefix('~').map_or((false, entry), |site| (true, site));
            // `example.*` and regular expressions can't be checked by name
            if !site.contains('.') || !site.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
                return Err(format!("the domain {entry} can't be checked"));
            }
            let site = site.to_ascii_lowercase();
            if negated { domains.exclude.push(site) } else { domains.include.push(site) }
        }
        Ok(domains)
    }

    fn allows(&self, site: &str) -> bool {
        (self.include.is_empty() || on_domain_list(&self.include, site)) && !on_domain_list(&self.exclude, site)
    }
}

// =========================================================================
// PATHS
// =========================================================================

/// A parsed path: steps taken from the top of the response.
struct Path(Vec<Step>);

enum Step {
    /// `.key`, `['key']`, `[a, b]`: these children, by name.
    Keys(Vec<String>),
    /// `*`: every child.
    All,
    /// `..`: the node and everything inside it, at any depth (the next step applies to each).
    Descend,
    /// `[?(...)]`: the children that pass the test.
    Filter(Test),
}

enum Test {
    Has(Key),
    Equals(Key, Value),
    Contains(Key, String),
}

/// What a test looks at in a child: one of its keys, or a nested path from the child.
enum Key {
    Name(String),
    Path(Path),
}

/// Reads a path a character at a time.
struct Reader<'a> {
    rest: &'a str,
}

impl<'a> Reader<'a> {
    fn eat(&mut self, text: &str) -> bool {
        match self.rest.strip_prefix(text) {
            Some(rest) => {
                self.rest = rest;
                true
            }
            None => false,
        }
    }

    fn expect(&mut self, text: &str) -> Result<(), String> {
        if self.eat(text) { Ok(()) } else { Err(format!("expected {text} in the path")) }
    }

    fn skip_space(&mut self) {
        self.rest = self.rest.trim_start();
    }

    fn peek(&self) -> Option<char> {
        self.rest.chars().next()
    }

    /// A bare key: letters, digits, `_` and `-`.
    fn name(&mut self) -> Option<&'a str> {
        let len = self.rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).unwrap_or(self.rest.len());
        let (name, rest) = self.rest.split_at(len);
        self.rest = rest;
        (!name.is_empty()).then_some(name)
    }

    /// A string in single or double quotes. Quotes inside one can't be escaped (no list does).
    fn quoted(&mut self) -> Result<Option<&'a str>, String> {
        let Some(quote) = self.peek().filter(|c| matches!(c, '\'' | '"')) else { return Ok(None) };
        let body = &self.rest[1..];
        let end  = body.find(quote).ok_or("a quoted string isn't closed")?;
        if body[..end].contains('\\') {
            return Err("escapes inside quoted strings aren't supported".into());
        }
        self.rest = &body[end + 1..];
        Ok(Some(&body[..end]))
    }
}

impl Path {
    fn parse(text: &str) -> Result<Self, String> {
        let mut reader = Reader { rest: text.trim() };
        reader.expect("$")?;
        let path = Self::steps(&mut reader, false)?;
        if !reader.rest.is_empty() {
            return Err(format!("unexpected {} in the path", reader.rest));
        }
        Ok(path)
    }

    /// Steps until the end, or (`nested`, inside a filter) until a space or `)`.
    fn steps(reader: &mut Reader, nested: bool) -> Result<Self, String> {
        let mut steps = vec![];
        loop {
            if reader.rest.is_empty() || (nested && reader.peek().is_some_and(|c| c == ')' || c.is_whitespace())) {
                break;
            }
            if reader.eat("..") {
                if reader.rest.is_empty() || nested && reader.peek() == Some(')') {
                    return Err("a path can't end with ..".into());
                }
                steps.push(Step::Descend);
                steps.push(Self::selector(reader)?);
            } else if reader.eat(".") {
                steps.push(Self::selector(reader)?);
            } else if reader.peek() == Some('[') {
                steps.push(Self::bracket(reader)?);
            } else {
                return Err(format!("unexpected {} in the path", reader.rest));
            }
        }
        Ok(Self(steps))
    }

    /// What follows a `.` or `..`: a key, `*`, or brackets.
    fn selector(reader: &mut Reader) -> Result<Step, String> {
        if reader.peek() == Some('[') {
            return Self::bracket(reader);
        }
        if reader.eat("*") {
            return Ok(Step::All);
        }
        let name = reader.name().ok_or_else(|| format!("unexpected {} in the path", reader.rest))?;
        Ok(Step::Keys(vec![name.to_string()]))
    }

    /// `[?(test)]`, `[*]`, or `[key, 'key', "key"]`. Array positions and slices aren't supported.
    fn bracket(reader: &mut Reader) -> Result<Step, String> {
        reader.expect("[")?;
        reader.skip_space();
        if reader.eat("?(") {
            let test = Self::test(reader)?;
            reader.skip_space();
            reader.expect(")")?;
            reader.skip_space();
            reader.expect("]")?;
            return Ok(Step::Filter(test));
        }
        if reader.eat("*") {
            reader.skip_space();
            reader.expect("]")?;
            return Ok(Step::All);
        }

        let mut keys = vec![];
        loop {
            reader.skip_space();
            let key = match reader.quoted()? {
                Some(key) => key,
                None => reader.name().ok_or_else(|| format!("unexpected {} in brackets", reader.rest))?,
            };
            if key.bytes().all(|b| b.is_ascii_digit() || b == b':' || b == b'-') {
                return Err("array positions aren't supported".into());
            }
            keys.push(key.to_string());
            reader.skip_space();
            if !reader.eat(",") {
                break;
            }
        }
        reader.expect("]")?;
        Ok(Step::Keys(keys))
    }

    /// `has k`, `key-eq k v`, `key-substr k v`, or `@.a.b` (has `$.a.b`).
    fn test(reader: &mut Reader) -> Result<Test, String> {
        reader.skip_space();
        if reader.eat("@") {
            return Ok(Test::Has(Key::Path(Self::steps(reader, true)?)));
        }
        let test = reader.name().ok_or("a filter needs has, key-eq, or key-substr")?;
        reader.skip_space();
        let key = Self::key(reader)?;
        reader.skip_space();
        match test {
            "has" => Ok(Test::Has(key)),
            "key-eq" => {
                let value = match reader.quoted()? {
                    Some(text) => Value::String(text.to_string()),
                    // Bare values are JSON: true, false, null, or a number
                    None => {
                        let len  = reader.rest.find(|c: char| c == ')' || c.is_whitespace()).unwrap_or(reader.rest.len());
                        let word = &reader.rest[..len];
                        reader.rest = &reader.rest[len..];
                        match serde_json::from_str::<Value>(word) {
                            Ok(value @ (Value::Bool(_) | Value::Null | Value::Number(_))) => value,
                            _ => return Err(format!("the value {word} can't be compared")),
                        }
                    }
                };
                Ok(Test::Equals(key, value))
            }
            "key-substr" => {
                let text = match reader.quoted()? {
                    Some(text) => text,
                    None => reader.name().ok_or("key-substr needs a value")?,
                };
                Ok(Test::Contains(key, text.to_string()))
            }
            other => Err(format!("the filter {other} isn't supported")),
        }
    }

    /// A filter's key: a name (bare or quoted), or a quoted nested path starting with `$`.
    fn key(reader: &mut Reader) -> Result<Key, String> {
        if let Some(text) = reader.quoted()? {
            return Ok(if text.starts_with('$') { Key::Path(Self::parse(text)?) } else { Key::Name(text.to_string()) });
        }
        let name = reader.name().ok_or_else(|| format!("unexpected {} in a filter", reader.rest))?;
        Ok(Key::Name(name.to_string()))
    }
}

// =========================================================================
// PRUNING
// =========================================================================

/// One step from a node to a child. Borrowed while the response is searched, owned while it's
/// changed.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Seg<K> {
    Key(K),
    Index(usize),
}

type Found<'a> = Vec<(Vec<Seg<&'a str>>, &'a Value)>;

/// A node's children, with the step to each.
fn children(value: &Value) -> Box<dyn Iterator<Item = (Seg<&str>, &Value)> + '_> {
    match value {
        Value::Object(map) => Box::new(map.iter().map(|(k, v)| (Seg::Key(k.as_str()), v))),
        Value::Array(items) => Box::new(items.iter().enumerate().map(|(i, v)| (Seg::Index(i), v))),
        _ => Box::new(std::iter::empty()),
    }
}

impl Path {
    /// Every node the path selects from `root`, with the steps to it.
    fn select<'a>(&self, root: &'a Value) -> Found<'a> {
        let mut found: Found<'a> = vec![(vec![], root)];
        for step in &self.0 {
            let mut next = vec![];
            for (at, value) in found {
                let mut push = |seg, child| {
                    let mut to = at.clone();
                    to.push(seg);
                    next.push((to, child));
                };
                match step {
                    Step::Keys(keys) => {
                        if let Value::Object(map) = value {
                            for key in keys {
                                if let Some((key, child)) = map.get_key_value(key) {
                                    push(Seg::Key(key.as_str()), child);
                                }
                            }
                        }
                    }
                    Step::All => children(value).for_each(|(seg, child)| push(seg, child)),
                    Step::Filter(test) => children(value).filter(|(_, child)| test.passes(child)).for_each(|(seg, child)| push(seg, child)),
                    Step::Descend => {
                        // The node itself, then everything inside it. A selector always follows
                        // `..` and looks at children, so plain values (most of a response) are
                        // left out instead of each getting a copy of its path
                        let mut stack = vec![(at.clone(), value)];
                        while let Some((to, node)) = stack.pop() {
                            for (seg, child) in children(node).filter(|(_, child)| child.is_object() || child.is_array()) {
                                let mut deeper = to.clone();
                                deeper.push(seg);
                                stack.push((deeper, child));
                            }
                            next.push((to, node));
                        }
                    }
                }
            }
            found = next;
        }
        found
    }
}

impl Test {
    fn passes(&self, child: &Value) -> bool {
        let (Self::Has(key) | Self::Equals(key, _) | Self::Contains(key, _)) = self;
        let values: Vec<&Value> = match key {
            Key::Name(name) => child.get(name.as_str()).into_iter().collect(),
            Key::Path(path) => path.select(child).into_iter().map(|(_, v)| v).collect(),
        };
        match self {
            Self::Has(_) => !values.is_empty(),
            Self::Equals(_, want) => values.contains(&want),
            Self::Contains(_, text) => values.iter().any(|v| v.as_str().is_some_and(|s| s.contains(text.as_str()))),
        }
    }
}

/// Remove the node at `path`; false if it's no longer there.
fn remove(root: &mut Value, path: &[Seg<String>]) -> bool {
    let Some((last, parents)) = path.split_last() else { return false };
    let mut at = root;
    for seg in parents {
        let next = match (seg, at) {
            (Seg::Key(key), Value::Object(map)) => map.get_mut(key),
            (Seg::Index(i), Value::Array(items)) => items.get_mut(*i),
            _ => None,
        };
        let Some(next) = next else { return false };
        at = next;
    }
    match (last, at) {
        // shift_remove keeps the other keys in the site's order
        (Seg::Key(key), Value::Object(map)) => map.shift_remove(key).is_some(),
        (Seg::Index(i), Value::Array(items)) if *i < items.len() => {
            items.remove(*i);
            true
        }
        _ => false,
    }
}

/// Apply the paths, in order, to a JSON body. None when it isn't JSON or nothing was removed,
/// so the body goes on exactly as it came.
fn prune_json<'a>(body: &[u8], paths: impl Iterator<Item = &'a Path>) -> Option<(Vec<u8>, usize)> {
    let mut json: Value = serde_json::from_slice(body).ok()?;
    let mut removed     = 0;
    for path in paths {
        let mut found: Vec<Vec<Seg<String>>> = path
            .select(&json)
            .into_iter()
            .map(|(at, _)| at.into_iter().map(|seg| match seg { Seg::Key(k) => Seg::Key(k.to_string()), Seg::Index(i) => Seg::Index(i) }).collect())
            .collect();
        found.sort();
        found.dedup();
        // From the end: children before their parents, and later array items before earlier
        // ones, so removing one never moves another that's still to go
        for at in found.iter().rev() {
            removed += usize::from(remove(&mut json, at));
        }
    }
    if removed == 0 {
        return None;
    }
    Some((serde_json::to_vec(&json).ok()?, removed))
}

// =========================================================================
// RESPONSES
// =========================================================================

/// Whether a response is worth reading for pruning: a successful JSON reply in an encoding
/// that can be read. Anything else passes through without being touched.
pub fn is_json(res: &Response<Body>) -> bool {
    res.status() == StatusCode::OK
        && res.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.to_ascii_lowercase().contains("json"))
        && crate::cosmetic::decodable(res.headers())
        && !res.headers().get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<usize>().ok()).is_some_and(|len| len > MAX_JSON_BYTES)
}

/// Prune a JSON response (call only when `is_json`). It's read whole (decompressed) up to
/// `MAX_JSON_BYTES`; a bigger one, one that isn't JSON, or one nothing matched goes on with
/// the same content.
pub async fn prune_response(res: Response<Body>, matched: &Matched, trace: &Trace) -> Response<Body> {
    let Ok(res) = decode_response(res) else {
        return Response::builder().status(StatusCode::BAD_GATEWAY).body(Body::empty()).expect("static response");
    };
    let (mut parts, body) = res.into_parts();

    // Read it all, unless it turns out too big or breaks off: then pass on what came, and the rest
    let mut stream = BodyDataStream::new(body);
    let mut chunks = vec![];
    let mut size   = 0;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                size += chunk.len();
                chunks.push(chunk);
                if size > MAX_JSON_BYTES {
                    let came = stream::iter(chunks.into_iter().map(Ok));
                    return Response::from_parts(parts, Body::from_stream(came.chain(stream)));
                }
            }
            Err(e) => {
                let came = stream::iter(chunks.into_iter().map(Ok)).chain(stream::once(async { Err(e) }));
                return Response::from_parts(parts, Body::from_stream(came));
            }
        }
    }
    let body = chunks.concat();

    let body = match prune_json(&body, matched.paths()) {
        Some((pruned, removed)) => {
            trace.write(format_args!("json {} pruned ({removed} removed by {} rules)", without_query(&matched.url), matched.which.len()));
            pruned
        }
        None => body,
    };
    parts.headers.remove(CONTENT_ENCODING);
    parts.headers.insert(CONTENT_LENGTH, HeaderValue::from(body.len()));
    Response::from_parts(parts, Body::from(Bytes::from(body)))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use serde_json::json;

    /// The rules from AdGuard Base filter and Zen's ads list for X, as they appear there.
    pub const X_RULES: &str = r#"/i/api/graphql/*/HomeLatestTimeline$jsonprune=\$.data.home.home_timeline_urt.instructions..entries..[?(key-substr 'entryId' 'promoted-tweet-')],domain=twitter.com|x.com
/i/api/graphql/*/HomeTimeline$jsonprune=\$.data.home.home_timeline_urt.instructions..entries..[?(key-substr 'entryId' 'promoted-tweet-')],domain=twitter.com|x.com
/i/api/graphql/*/TweetDetail$jsonprune=\$.data.threaded_conversation_with_injections_v2.instructions..entries.[?(has "\$.content.items..item.itemContent.promotedMetadata.advertiser_results")],domain=twitter.com|x.com
/i/api/graphql/*/ImmersiveMedia$jsonprune=\$.data.immersiveMedia.timeline.instructions..entries..[?(key-substr 'entryId' 'promoted-tweet-')],domain=twitter.com|x.com
||x.com/i/api/graphql/*/HomeTimeline$jsonprune=$.data.home.home_timeline_urt.instructions..entries[?(@.content.itemContent.promotedMetadata)]"#;

    /// A trimmed X home timeline: one tweet and one promoted tweet.
    pub fn home_timeline() -> Value {
        json!({ "data": { "home": { "home_timeline_urt": { "instructions": [
            { "type": "TimelineClearCache" },
            { "type": "TimelineAddEntries", "entries": [
                { "entryId": "tweet-1871", "sortIndex": "9", "content": { "itemContent": { "tweet_results": { "result": { "rest_id": "1871" } } } } },
                { "entryId": "promoted-tweet-1872-abc", "sortIndex": "8", "content": { "itemContent": {
                    "tweet_results": { "result": { "rest_id": "1872" } },
                    "promotedMetadata": { "advertiser_results": { "result": { "rest_id": "99" } } } } } },
                { "entryId": "cursor-top-1873", "sortIndex": "7", "content": { "value": "x" } }
            ] }
        ] } } } })
    }

    fn rules(text: &str) -> Arc<Rules> {
        Arc::new(Rules::parse(text))
    }

    fn prune(rules: &Arc<Rules>, url: &str, source: &str, kind: &str, body: &Value) -> Option<Value> {
        let matched = Matched::find(rules, url, source, kind)?;
        prune_json(&serde_json::to_vec(body).unwrap(), matched.paths()).map(|(b, _)| serde_json::from_slice(&b).unwrap())
    }

    const HOME: &str = "https://x.com/i/api/graphql/abc123/HomeTimeline?variables=%7B%7D";

    #[test]
    fn x_promoted_posts_are_removed_and_tweets_stay() {
        let rules  = rules(X_RULES);
        assert_eq!(rules.count(), 5, "{:?}", rules.dropped);
        let pruned = prune(&rules, HOME, "https://x.com/home", "xmlhttprequest", &home_timeline()).expect("pruned");

        let entries = &pruned["data"]["home"]["home_timeline_urt"]["instructions"][1]["entries"];
        let ids: Vec<&str> = entries.as_array().unwrap().iter().map(|e| e["entryId"].as_str().unwrap()).collect();
        assert_eq!(ids, ["tweet-1871", "cursor-top-1873"], "only the ad is gone");
        assert_eq!(pruned["data"]["home"]["home_timeline_urt"]["instructions"][0], json!({ "type": "TimelineClearCache" }));
    }

    #[test]
    fn rules_apply_only_where_written() {
        let rules = rules(X_RULES);
        assert!(Matched::find(&rules, HOME, "https://x.com/home", "xmlhttprequest").is_some());
        assert!(Matched::find(&rules, HOME, "https://twitter.com/home", "other").is_some(), "no request type was named");
        let twitter = "https://twitter.com/i/api/graphql/abc/HomeTimeline";
        assert!(Matched::find(&rules, twitter, "https://twitter.com/home", "xmlhttprequest").is_some());
        assert!(Matched::find(&rules, twitter, "https://news.example/", "xmlhttprequest").is_none(), "domain= is the page's site");
        assert_eq!(Matched::find(&rules, HOME, "https://news.example/", "xmlhttprequest").unwrap().which.len(), 1, "only Zen's ||x.com rule, which names no domain");
        assert!(Matched::find(&rules, "https://x.com/i/api/graphql/abc/UserTweets", "https://x.com/", "xmlhttprequest").is_none());
        assert!(Matched::find(&rules, "https://x.com/home", "https://x.com/", "document").is_none());
    }

    #[test]
    fn tweet_details_lose_promoted_replies() {
        let rules = rules(X_RULES);
        let body  = json!({ "data": { "threaded_conversation_with_injections_v2": { "instructions": [ { "entries": [
            { "entryId": "tweet-1", "content": { "itemContent": {} } },
            { "entryId": "conversationthread-2", "content": { "items": [ { "item": { "itemContent": { "promotedMetadata": { "advertiser_results": {} } } } } ] } },
            { "entryId": "conversationthread-3", "content": { "items": [ { "item": { "itemContent": { "tweet_results": {} } } } ] } }
        ] } ] } } });
        let url    = "https://x.com/i/api/graphql/xyz/TweetDetail?variables=1";
        let pruned = prune(&rules, url, "https://x.com/a/status/1", "xmlhttprequest", &body).expect("pruned");
        let ids: Vec<&str> = pruned["data"]["threaded_conversation_with_injections_v2"]["instructions"][0]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["entryId"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["tweet-1", "conversationthread-3"]);
    }

    #[test]
    fn nothing_removed_means_nothing_rewritten() {
        let rules = rules(X_RULES);
        let body  = json!({ "data": { "home": { "home_timeline_urt": { "instructions": [] } } } });
        assert!(prune(&rules, HOME, "https://x.com/", "xmlhttprequest", &body).is_none());
        let matched = Matched::find(&rules, HOME, "https://x.com/", "xmlhttprequest").unwrap();
        assert!(prune_json(b"not json", matched.paths()).is_none());
    }

    #[test]
    fn rule_lines_are_read_with_their_escapes_and_modifiers() {
        let line = r"||example.org^$xmlhttprequest,jsonprune=\$..[one\, 'two three'],domain=a.com|~b.a.com";
        let Ok(Some(Line::Rule(rule))) = parse_line(line) else { panic!("parsed") };
        assert_eq!(rule.expression, "$..[one, 'two three']");
        assert!(rule.when.xhr_only);
        assert!(rule.when.applies("https://www.example.org/x", "a.com", "xmlhttprequest"));
        assert!(!rule.when.applies("https://www.example.org/x", "b.a.com", "xmlhttprequest"), "~ excludes a site");
        assert!(!rule.when.applies("https://www.example.org/x", "a.com", "script"));
        assert!(!rule.when.applies("https://notexample.org/x", "a.com", "xmlhttprequest"), "|| is the site or a subdomain");

        // Not $jsonprune rules at all
        for line in ["||ads.com^", "! jsonprune comment", "example.com##+js(json-prune, ads)", "a.com##.jsonprune"] {
            assert!(matches!(parse_line(line), Ok(None)), "{line}");
        }
    }

    #[test]
    fn rules_that_cant_be_applied_exactly_are_dropped() {
        for line in [
            r"||a.com^$jsonprune=\$.a,third-party",
            r"/ads\d+/$jsonprune=\$.a",
            r"||a.com^$jsonprune=\$.a,domain=a.*",
            r"||a.com^$jsonprune=\$.a[0]",
            r"||a.com^$jsonprune=\$.a..",
            r"||a.com^$jsonprune=\$.a[?(key-gt 'x' 1)]",
            r"||a.com^$jsonprune=\$",
            r"||a.com^$jsonprune",
            r"||a.com^$jsonprune=.a",
        ] {
            assert!(parse_line(line).is_err(), "{line}");
        }
        let rules = Rules::parse("||a.com^$jsonprune=\\$.a,third-party\n||a.com^$jsonprune=\\$.b");
        assert_eq!((rules.count(), rules.dropped.len()), (1, 1));
        assert!(rules.dropped[0].contains("third-party"), "{:?}", rules.dropped);
    }

    #[test]
    fn exceptions_turn_rules_off() {
        let text = "||a.com^$jsonprune=\\$.ad\n||a.com^$jsonprune=\\$.banner\n@@||a.com/keep^$jsonprune\n@@||a.com/one^$jsonprune=\\$.ad";
        let rules = rules(text);
        assert_eq!(Matched::find(&rules, "https://a.com/x", "https://a.com/", "other").unwrap().which.len(), 2);
        assert!(Matched::find(&rules, "https://a.com/keep", "https://a.com/", "other").is_none(), "all of them");
        let one = Matched::find(&rules, "https://a.com/one", "https://a.com/", "other").unwrap();
        assert_eq!(one.which.len(), 1, "only the one it names");
        assert_eq!(rules.rules[one.which[0]].expression, "$.banner");
    }

    #[test]
    fn paths_select_like_adguard() {
        let pick = |path: &str, body: Value| {
            let path = Path::parse(path).unwrap();
            prune_json(&serde_json::to_vec(&body).unwrap(), std::iter::once(&path)).map_or(body, |(b, _)| serde_json::from_slice(&b).unwrap())
        };

        // AdGuard's own examples
        assert_eq!(pick("$..[one, \"two three\"]", json!({ "one": 1, "two": { "two three": 23, "four five": 45 } })), json!({ "two": { "four five": 45 } }));
        assert_eq!(pick("$.a[?(has ad_origin)]", json!({ "a": [{ "ad_origin": "a", "b": 42 }, { "b": 1234 }] })), json!({ "a": [{ "b": 1234 }] }));
        assert_eq!(
            pick("$.*.*[?(key-eq 'Some key' 'Some value')]", json!({ "a": { "b": { "c": { "Some key": "Some value" }, "d": { "Some key": "Other value" } } }, "e": { "f": [{ "Some key": "Some value" }, { "Some key": "Other value" }] } })),
            json!({ "a": { "b": { "d": { "Some key": "Other value" } } }, "e": { "f": [{ "Some key": "Other value" }] } })
        );
        assert_eq!(pick("$.elems[?(has \"$.a.b.c\")]", json!({ "elems": [{ "a": { "b": { "c": 123 } } }, { "a": { "b": {} } }] })), json!({ "elems": [{ "a": { "b": {} } }] }));
        assert_eq!(pick("$.elems[?(key-eq \"$.a.b.c\" \"abc\")]", json!({ "elems": [{ "a": { "b": { "c": 123 } } }, { "a": { "b": { "c": "abc" } } }] })), json!({ "elems": [{ "a": { "b": { "c": 123 } } }] }));

        // Bare JSON values, the node itself as the key, `*` as the last step
        assert_eq!(pick("$.feed.*..[?(key-eq \"$.isAd\" true)]", json!({ "feed": { "x": [{ "isAd": true }, { "isAd": false }] } })), json!({ "feed": { "x": [{ "isAd": false }] } }));
        assert_eq!(pick("$.ref[?(key-substr '$' 'NativeAd')]", json!({ "ref": ["MainNativeAd", "Story"] })), json!({ "ref": ["Story"] }));
        assert_eq!(pick("$.adBreaks.*", json!({ "adBreaks": [1, 2], "keep": 3 })), json!({ "adBreaks": [], "keep": 3 }));
        assert_eq!(pick("$.configs['Gallery/ntp'].ads", json!({ "configs": { "Gallery/ntp": { "ads": 1, "b": 2 } } })), json!({ "configs": { "Gallery/ntp": { "b": 2 } } }));

        // Several removals from one array, and keys keep their order
        assert_eq!(pick("$.a[?(has x)]", json!({ "a": [{ "x": 1 }, { "y": 2 }, { "x": 3 }, { "y": 4 }] })), json!({ "a": [{ "y": 2 }, { "y": 4 }] }));
        let kept = pick("$.b", json!({ "z": 1, "b": 2, "a": 3 }));
        assert_eq!(serde_json::to_string(&kept).unwrap(), r#"{"z":1,"a":3}"#);
    }

    #[test]
    fn addresses_match_like_adblock_plus() {
        let matches = |pattern: &str, url: &str| Pattern::parse(pattern, false).unwrap().matches(url);
        assert!(matches("||api.pixai.art/graphql", "https://API.pixai.art/graphql?x"));
        assert!(matches("||pluto.tv/*/session.json", "https://boot.pluto.tv/v4/session.json?a"));
        assert!(!matches("||pluto.tv/*/session.json", "https://notpluto.tv/v4/session.json"));
        assert!(matches("/GetPlaybackResources?", "https://atv-ps.amazon.com/cdp/catalog/GetPlaybackResources?asin=1"));
        assert!(matches("||example.org^", "https://example.org"));
        assert!(matches("||example.org^", "https://example.org:8443/a"));
        assert!(!matches("||example.org^", "https://example.organic/"));
        assert!(matches("|https://a.com/x|", "https://a.com/x"));
        assert!(!matches("|https://a.com/x|", "https://a.com/xy"));
        assert!(!Pattern::parse("/Path", true).unwrap().matches("https://a.com/path"), "$match-case");
        assert_eq!(host_of("https://user@X.com:443/home?q"), "X.com");
    }

    #[tokio::test]
    async fn responses_are_pruned_whole_or_passed_on() {
        let rules   = rules(X_RULES);
        let matched = Matched::find(&rules, HOME, "https://x.com/home", "xmlhttprequest").unwrap();
        let body    = serde_json::to_vec(&home_timeline()).unwrap();
        let res     = Response::builder().header(CONTENT_TYPE, "application/json; charset=utf-8").header(CONTENT_LENGTH, body.len()).body(Body::from(body.clone())).unwrap();
        assert!(is_json(&res));

        let out    = prune_response(res, &matched, &Default::default()).await;
        let length = out.headers().get(CONTENT_LENGTH).unwrap().to_str().unwrap().parse::<usize>().unwrap();
        let pruned = out.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(length, pruned.len(), "content-length is the new length");
        assert!(pruned.len() < body.len() && !String::from_utf8_lossy(&pruned).contains("promoted-tweet-"));

        // Not JSON, too big, or not a success: never read
        let html = Response::builder().header(CONTENT_TYPE, "text/html").body(Body::empty()).unwrap();
        let huge = Response::builder().header(CONTENT_TYPE, "application/json").header(CONTENT_LENGTH, MAX_JSON_BYTES + 1).body(Body::empty()).unwrap();
        let fail = Response::builder().status(500).header(CONTENT_TYPE, "application/json").body(Body::empty()).unwrap();
        assert!(!is_json(&html) && !is_json(&huge) && !is_json(&fail));

        // Bigger than the limit without saying so: everything comes through unchanged
        let big   = vec![b' '; MAX_JSON_BYTES + 10];
        let res   = Response::builder().header(CONTENT_TYPE, "application/json").body(Body::from(big.clone())).unwrap();
        let out   = prune_response(res, &matched, &Default::default()).await.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(out.len(), big.len());
    }
}
