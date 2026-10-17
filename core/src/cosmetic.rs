//! Cosmetic filtering: rules that act inside the page instead of blocking a request.
//!
//! Some ads (Reddit's promoted posts, for example) arrive inside the page's own HTML,
//! so there is no request to block. Browser extensions hide them with page CSS and
//! small scripts; a proxy does the same by adding one `<style>` and one `<script>`
//! right after `<head>` in every HTML page, streamed so pages don't wait on it.
//!
//! - Hide rules (`##`) become CSS.
//! - Style and advanced rules (`:style()`, `:has-text()`, `:upward()`, ...) run in a
//!   small script (procedural.js).
//! - Scriptlets (`##+js(...)` and AdGuard's `#%#//scriptlet(...)`) come from AdGuard's
//!   open-source scriptlet library, the same one AdGuard's desktop app uses.
//!
//! When a site's Content-Security-Policy would block inline code, a fresh nonce is
//! added for our two tags only; the site's own rules are otherwise untouched.

use adblock::{
    Engine,
    resources::{MimeType, PermissionMask, Resource, ResourceType},
};
use base64::{Engine as _, prelude::BASE64_STANDARD};
use hudsucker::{
    Body, decode_response,
    futures::{StreamExt, stream},
    hyper::{
        Response, StatusCode,
        body::Bytes,
        header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY_REPORT_ONLY, CONTENT_TYPE, HeaderValue},
    },
};
use http_body_util::BodyDataStream;
use serde::Deserialize;

/// Permission granted to trusted lists; required by AdGuard's `trusted-*` scriptlets.
pub const TRUSTED: PermissionMask = PermissionMask::from_bits(0b1);

/// AdGuard scriptlets, pinned to a version and its SHA-256, so neither an upstream change
/// nor a tampered download can alter the code that runs in pages.
pub const SCRIPTLETS_URL: &str = "https://cdn.jsdelivr.net/npm/@adguard/scriptlets@2.5.1/dist/scriptlets.corelibs.json";
const SCRIPTLETS_SHA256: &str = "5c76ecdb58ecaf975df54aa447e2cc5db762330eab3d334e271829c6b95c0184";

/// Accept the scriptlet library only if it's exactly the pinned file.
pub fn verify_library(bytes: &[u8]) -> Result<(), &'static str> {
    use sha2::{Digest, Sha256};
    let hash: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
    if hash == SCRIPTLETS_SHA256 { Ok(()) } else { Err("scriptlet library doesn't match the pinned version") }
}

const PROCEDURAL_JS: &str = include_str!("procedural.js");

// =========================================================================
// ADGUARD SYNTAX
// =========================================================================

/// Rewrite AdGuard-only rule syntax into the uBO syntax adblock-rust understands.
///
/// - `example.com#%#//scriptlet('name', 'a', 'b')` becomes `example.com##+js(name, a, b)`
/// - `example.com#$#.ad { display: none !important; }` becomes `example.com##.ad:style(display: none !important;)`
/// - Raw JavaScript rules (`#%#` without `//scriptlet`) are dropped: they're arbitrary code from a list.
pub fn translate_adguard(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if let Some(rule) = translate_line(line) {
            out.push_str(&rule);
            out.push('\n');
        }
    }
    out
}

fn translate_line(line: &str) -> Option<String> {
    let Some(at) = line.find('#') else { return Some(line.to_string()) };
    let (domains, rest) = line.split_at(at);

    // Scriptlets
    for (marker, sep) in [("#@%#", "#@#"), ("#%#", "##")] {
        if let Some(body) = rest.strip_prefix(marker) {
            let inner = body.trim().strip_prefix("//scriptlet(")?.strip_suffix(')')?;
            let args  = parse_quoted_args(inner)?;
            let args  = args.iter().map(|a| a.replace(',', "\\,")).collect::<Vec<_>>().join(", ");
            return Some(format!("{domains}{sep}+js({args})"));
        }
    }

    // CSS injection
    for (marker, sep) in [("#@$?#", "#@?#"), ("#$?#", "#?#"), ("#@$#", "#@#"), ("#$#", "##")] {
        if let Some(body) = rest.strip_prefix(marker) {
            let open     = body.rfind('{')?;
            let selector = body[..open].trim();
            let css      = body[open + 1..].trim().strip_suffix('}')?.trim();
            return Some(format!("{domains}{sep}{selector}:style({css})"));
        }
    }

    Some(line.to_string())
}

/// Parse `'a', "b", 'c\'d'` into its unquoted values.
fn parse_quoted_args(input: &str) -> Option<Vec<String>> {
    let mut args  = vec![];
    let mut chars = input.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace() || *c == ',') {
            chars.next();
        }
        let Some(quote) = chars.next() else { return Some(args) };
        if quote != '\'' && quote != '"' {
            return None;
        }
        let mut arg = String::new();
        loop {
            match chars.next()? {
                '\\' => arg.push(chars.next()?),
                c if c == quote => break,
                c => arg.push(c),
            }
        }
        args.push(arg);
    }
}

// =========================================================================
// SCRIPTLET LIBRARY
// =========================================================================

#[derive(Deserialize)]
struct Library {
    version:    String,
    scriptlets: Vec<LibraryScriptlet>,
}

#[derive(Deserialize)]
struct LibraryScriptlet {
    names:     Vec<String>,
    scriptlet: String,
}

fn with_js(name: &str) -> String {
    if name.ends_with(".js") { name.to_string() } else { format!("{name}.js") }
}

/// Turn AdGuard's library into adblock-rust resources. Each scriptlet is wrapped in a named
/// function so adblock-rust can call it with the rule's arguments; every AdGuard and uBO
/// alias maps to it. `trusted-*` scriptlets require the TRUSTED permission.
pub fn scriptlet_resources(json: &str) -> Vec<Resource> {
    let Ok(library) = serde_json::from_str::<Library>(json) else { return vec![] };

    library
        .scriptlets
        .into_iter()
        .filter(|s| !s.names.is_empty())
        .map(|s| {
            let primary  = s.names[0].clone();
            let ident    = format!("cf_{}", primary.replace(|c: char| !c.is_ascii_alphanumeric(), "_"));
            let source   = serde_json::json!({ "name": primary, "engine": "extension", "version": library.version, "verbose": false, "uniqueId": "" });
            let content  = format!("function {ident}(...args) {{ const source = Object.assign({source}, {{ args }}); ({})(source, args); }}", s.scriptlet);
            let name     = with_js(&primary);
            let aliases  = s.names.iter().map(|n| with_js(n)).filter(|n| *n != name).collect();
            let trusted  = primary.starts_with("trusted-");

            Resource {
                name,
                aliases,
                kind:         ResourceType::Mime(MimeType::ApplicationJavascript),
                content:      BASE64_STANDARD.encode(content),
                dependencies: vec![],
                permission:   if trusted { TRUSTED } else { PermissionMask::default() },
            }
        })
        .collect()
}

// =========================================================================
// PAGE INJECTION
// =========================================================================

/// Keep code inside its `<script>` tag. The HTML parser ends a script at `</script` in any
/// letter case, and `<!--` can make it swallow the rest of the page, so both get their `<`
/// written as `\x3C`, which means the same thing inside JS strings, templates, and regexes.
/// Without this, a scriptlet argument from any filter list could close our tag and add its own.
pub fn escape_script(js: &str) -> String {
    // ASCII lowercasing keeps every byte offset, so positions found in `lower` fit `js`
    let lower   = js.to_ascii_lowercase();
    let mut out = String::with_capacity(js.len());
    let mut last = 0;
    for (at, _) in lower.match_indices('<') {
        if lower[at..].starts_with("</script") || lower[at..].starts_with("<!--") {
            out.push_str(&js[last..at]);
            out.push_str("\\x3C");
            last = at + 1;
        }
    }
    out.push_str(&js[last..]);
    out
}

fn script_tag(js: &str, nonce: &str) -> String {
    format!("<script nonce=\"{nonce}\">{}</script>", escape_script(js))
}

/// The `<style>` and `<script>` for one page, plus one `<script>` per userscript (so a
/// broken one can't stop the rest), or None when nothing applies.
fn page_tags(engine: &Engine, userscripts: &[&str], url: &str, nonce: &str) -> Option<Bytes> {
    let resources = engine.url_cosmetic_resources(url);

    // One CSS rule per selector, so one bad selector can't void the rest. Anything that
    // could close the <style> tag is dropped.
    let css: String = resources
        .hide_selectors
        .iter()
        .filter(|s| !s.contains(['<', '{', '}']))
        .map(|s| format!("{s}{{display:none!important}}\n"))
        .collect();

    let procedural: Vec<serde_json::Value> = resources
        .procedural_actions
        .iter()
        .filter_map(|p| serde_json::from_str(p).ok())
        .collect();

    let mut js = resources.injected_script;
    if !procedural.is_empty() {
        let rules = serde_json::to_string(&procedural).ok()?.replace('<', "\\u003c");
        js.push_str(&PROCEDURAL_JS.replace("__CF_RULES__", &rules));
    }

    if css.is_empty() && js.is_empty() && userscripts.is_empty() {
        return None;
    }

    let mut tags = String::new();
    if !css.is_empty() {
        tags.push_str(&format!("<style nonce=\"{nonce}\">{css}</style>"));
    }
    if !js.is_empty() {
        tags.push_str(&script_tag(&js, nonce));
    }
    // Userscripts were escaped once when loaded; only the tag is added per page
    for script in userscripts {
        tags.push_str(&format!("<script nonce=\"{nonce}\">{script}</script>"));
    }
    Some(Bytes::from(tags))
}

fn new_nonce() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("OS random number generator");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Add cosmetic filtering to an HTML page response. Anything that isn't a plain,
/// decodable HTML page passes through untouched.
pub fn inject(res: Response<Body>, engine: &Engine, userscripts: &[&str], url: &str) -> Response<Body> {
    let headers = res.headers();
    let is_html = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        // XHTML is parsed as XML; injected CSS/JS with `&` or `<` would break the page
        .is_some_and(|t| t.len() >= 9 && t[..9].eq_ignore_ascii_case("text/html"));
    let decodable = headers.get_all(CONTENT_ENCODING).iter().all(|v| {
        v.to_str().is_ok_and(|v| {
            v.split(',').map(str::trim).all(|e| matches!(e, "gzip" | "x-gzip" | "deflate" | "br" | "zstd" | "identity"))
        })
    });
    if res.status() != StatusCode::OK || !is_html || !decodable {
        return res;
    }

    let nonce = new_nonce();
    let Some(tags) = page_tags(engine, userscripts, url, &nonce) else { return res };

    // Encodings were checked above, so decoding only fails on a broken upstream body
    let Ok(res) = decode_response(res) else {
        return Response::builder().status(StatusCode::BAD_GATEWAY).body(Body::empty()).expect("static response");
    };
    let (mut parts, body) = res.into_parts();

    // A policy we can't read or rewrite is kept exactly as sent; dropping it would remove the
    // site's protection. Report-only policies get the nonce too, so the site isn't sent reports
    // that name our tags.
    for name in [CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY_REPORT_ONLY] {
        let policies: Vec<HeaderValue> = parts
            .headers
            .get_all(&name)
            .iter()
            .map(|v| v.to_str().ok().and_then(|s| HeaderValue::from_str(&allow_nonce(s, &nonce)).ok()).unwrap_or_else(|| v.clone()))
            .collect();
        parts.headers.remove(&name);
        for policy in policies {
            parts.headers.append(&name, policy);
        }
    }
    parts.headers.remove(CONTENT_LENGTH);

    Response::from_parts(parts, inject_after_head(body, tags))
}

// =========================================================================
// CONTENT SECURITY POLICY
// =========================================================================

/// Let our nonce'd `<style>` and `<script>` run under a site's CSP header.
///
/// Only touches a directive that would block inline code. A directive that already
/// allows inline code ('unsafe-inline' with no nonce or hash) is left alone, because
/// adding a nonce there would switch 'unsafe-inline' off for the site's own code.
pub fn allow_nonce(header: &str, nonce: &str) -> String {
    header
        .split(',')
        .map(|policy| {
            let mut directives: Vec<(String, String)> = policy
                .split(';')
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(|d| match d.split_once(char::is_whitespace) {
                    Some((name, sources)) => (name.to_ascii_lowercase(), sources.trim().to_string()),
                    None => (d.to_ascii_lowercase(), String::new()),
                })
                .collect();

            for (elem, base) in [("style-src-elem", "style-src"), ("script-src-elem", "script-src")] {
                let source = format!("'nonce-{nonce}'");
                let find   = |name: &str, d: &[(String, String)]| d.iter().position(|(n, _)| n == name);

                if let Some(i) = find(elem, &directives).or_else(|| find(base, &directives)) {
                    if blocks_inline(&directives[i].1) {
                        directives[i].1 = format!("{} {source}", directives[i].1).trim().to_string();
                    }
                } else if let Some(i) = find("default-src", &directives)
                    && blocks_inline(&directives[i].1)
                {
                    let sources = format!("{} {source}", directives[i].1).trim().to_string();
                    directives.push((base.to_string(), sources));
                }
            }

            directives
                .iter()
                .map(|(n, s)| if s.is_empty() { n.clone() } else { format!("{n} {s}") })
                .collect::<Vec<_>>()
                .join("; ")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn blocks_inline(sources: &str) -> bool {
    let lower = sources.to_ascii_lowercase();
    let pinned = ["'nonce-", "'sha256-", "'sha384-", "'sha512-", "'strict-dynamic'"].iter().any(|p| lower.contains(p));
    !lower.contains("'unsafe-inline'") || pinned
}

// =========================================================================
// STREAMING
// =========================================================================

/// Where to insert our tags: just past the opening `<head ...>` tag, or right before
/// `<body ...>` on pages that leave out `<head>` (so they aren't held back waiting for one).
/// None until either has fully arrived. Searching starts at `from`, so a growing buffer
/// isn't rescanned from the top on every chunk.
fn head_end(html: &[u8], from: usize) -> Option<usize> {
    let mut from = from.min(html.len());
    while let Some(i) = html[from..].iter().position(|&b| b == b'<') {
        let start = from + i;
        let tag   = html.get(start + 1..start + 5)?;
        let head  = tag.eq_ignore_ascii_case(b"head");
        let body  = tag.eq_ignore_ascii_case(b"body");
        if head || body {
            match html.get(start + 5) {
                Some(b'>' | b' ' | b'\t' | b'\n' | b'\r' | b'/') if head => {
                    return html[start..].iter().position(|&b| b == b'>').map(|j| start + j + 1);
                }
                Some(b'>' | b' ' | b'\t' | b'\n' | b'\r' | b'/') => return Some(start),
                Some(_) => {} // <header>, <heading>, <bodyx>, ...
                None => return None,
            }
        }
        from = start + 1;
    }
    None
}

/// Insert `tags` right after `<head>` while streaming. Holds back only the bytes before
/// `<head>` (usually a few hundred); gives up and passes through after 256 KB.
fn inject_after_head(body: Body, tags: Bytes) -> Body {
    const LIMIT: usize = 256 * 1024;

    // How far back to rescan when a chunk arrives: a `<head ...>` tag split across chunks
    const OVERLAP: usize = 4096;

    let stream = stream::unfold((BodyDataStream::new(body), Some(Vec::new())), move |(mut body, mut held)| {
        let tags = tags.clone();
        async move {
            loop {
                let Some(mut buf) = held.take() else {
                    return body.next().await.map(|chunk| (chunk, (body, None)));
                };
                match body.next().await {
                    Some(Ok(chunk)) => {
                        let from = buf.len().saturating_sub(OVERLAP);
                        buf.extend_from_slice(&chunk);
                        if let Some(at) = head_end(&buf, from) {
                            let mut out = Vec::with_capacity(buf.len() + tags.len());
                            out.extend_from_slice(&buf[..at]);
                            out.extend_from_slice(&tags);
                            out.extend_from_slice(&buf[at..]);
                            return Some((Ok(Bytes::from(out)), (body, None)));
                        }
                        if buf.len() > LIMIT {
                            return Some((Ok(Bytes::from(buf)), (body, None)));
                        }
                        held = Some(buf);
                    }
                    Some(Err(e)) => return Some((Err(e), (body, None))),
                    None if buf.is_empty() => return None,
                    None => return Some((Ok(Bytes::from(buf)), (body, None))),
                }
            }
        }
    });
    Body::from_stream(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use adblock::lists::{FilterSet, ParseOptions};
    use http_body_util::BodyExt;

    #[test]
    fn translates_adguard_scriptlets_and_css() {
        assert_eq!(
            translate_line("a.com#%#//scriptlet('set-constant', 'ads.on', 'false')").unwrap(),
            "a.com##+js(set-constant, ads.on, false)"
        );
        assert_eq!(
            translate_line(r#"a.com#%#//scriptlet('trusted-click-element', 'div > button, .x', "it's")"#).unwrap(),
            r"a.com##+js(trusted-click-element, div > button\, .x, it's)"
        );
        assert_eq!(translate_line("a.com#@%#//scriptlet('abort-on-property-read', 'x')").unwrap(), "a.com#@#+js(abort-on-property-read, x)");
        assert_eq!(translate_line("a.com#$#.ad { display: none !important; }").unwrap(), "a.com##.ad:style(display: none !important;)");
        assert_eq!(translate_line("a.com#%#window.x = 1;"), None, "raw JS is dropped");
        assert_eq!(translate_line("||ads.com^").unwrap(), "||ads.com^");
        assert_eq!(translate_line("a.com##.ad").unwrap(), "a.com##.ad");
    }

    #[test]
    fn library_must_match_pinned_hash() {
        assert!(verify_library(b"{\"version\":\"x\",\"scriptlets\":[]}").is_err());
    }

    #[tokio::test]
    async fn unreadable_csp_header_is_kept() {
        let engine = engine_with("a.com##.ad", false);
        let raw    = HeaderValue::from_bytes(b"default-src 'self'; report-uri /r\xe9").unwrap();
        assert!(raw.to_str().is_err(), "test value must be non-UTF-8");
        let res = Response::builder()
            .header(CONTENT_TYPE, "text/html")
            .header(CONTENT_SECURITY_POLICY, raw.clone())
            .header(CONTENT_SECURITY_POLICY, "script-src 'self'")
            .body(Body::from("<html><head></head></html>"))
            .unwrap();

        let out      = inject(res, &engine, &[], "https://a.com/");
        let policies = out.headers().get_all(CONTENT_SECURITY_POLICY).iter().cloned().collect::<Vec<_>>();
        assert!(policies.contains(&raw), "unreadable policy passed through untouched");
        assert!(policies.iter().any(|p| p.to_str().is_ok_and(|s| s.contains("'nonce-"))), "readable policy got our nonce");
        let html = out.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&html).contains(".ad{display:none!important}"));
    }

    #[tokio::test]
    async fn xhtml_is_left_alone() {
        let engine = engine_with("a.com##.ad", false);
        let page   = "<html><head></head></html>";
        let res    = Response::builder().header(CONTENT_TYPE, "application/xhtml+xml").body(Body::from(page)).unwrap();
        let html   = inject(res, &engine, &[], "https://a.com/").into_body().collect().await.unwrap().to_bytes();
        assert_eq!(html, Bytes::from(page));
    }

    #[test]
    fn csp_gets_nonce_only_where_inline_is_blocked() {
        // Reddit-style: scripts pinned by nonce, styles allow inline
        let p = allow_nonce("default-src 'none'; script-src 'nonce-abc'; style-src 'unsafe-inline'", "N");
        assert!(p.contains("script-src 'nonce-abc' 'nonce-N'"));
        assert!(p.contains("style-src 'unsafe-inline'") && !p.contains("style-src 'unsafe-inline' 'nonce-N'"));

        // default-src only: new directives copy its sources
        let p = allow_nonce("default-src 'self'", "N");
        assert!(p.contains("style-src 'self' 'nonce-N'") && p.contains("script-src 'self' 'nonce-N'"));

        // style-src-elem wins over style-src
        let p = allow_nonce("style-src 'self'; style-src-elem 'self'", "N");
        assert!(p.contains("style-src-elem 'self' 'nonce-N'") && p.contains("style-src 'self';"));

        // No restrictions: unchanged
        assert_eq!(allow_nonce("frame-ancestors 'none'", "N"), "frame-ancestors 'none'");
    }

    #[test]
    fn finds_head_but_not_header() {
        assert_eq!(head_end(b"<html><head><title>", 0), Some(12));
        assert_eq!(head_end(b"<HTML><HEAD lang=en>x", 0), Some(20));
        assert_eq!(head_end(b"<header><head>", 0), Some(14));
        assert_eq!(head_end(b"<html><hea", 0), None);
        assert_eq!(head_end(b"<html><head>", 99), None, "start past the end is safe");
        assert_eq!(head_end(b"<html><BODY class=x>", 0), Some(6), "no <head>: before <body>");
        assert_eq!(head_end(b"<html><bodyx><head>", 0), Some(19));
        assert_eq!(head_end(b"<html><bo", 0), None);
    }

    #[tokio::test]
    async fn injects_after_head_across_chunks() {
        let chunks = vec![Ok::<_, std::io::Error>(Bytes::from("<!DOCTYPE html><html><he")), Ok(Bytes::from("ad><title>t</title>"))];
        let body   = inject_after_head(Body::from_stream(stream::iter(chunks)), Bytes::from("<style>X</style>"));
        let html   = body.collect().await.unwrap().to_bytes();
        assert_eq!(html, Bytes::from("<!DOCTYPE html><html><head><style>X</style><title>t</title>"));
    }

    fn engine_with(rules: &str, trusted: bool) -> Engine {
        let mut set = FilterSet::new(false);
        let opts    = ParseOptions { permissions: if trusted { TRUSTED } else { PermissionMask::default() }, ..Default::default() };
        set.add_filter_list(translate_adguard(rules), opts);
        let mut engine = Engine::new_with_filter_set(set);
        let library = r#"{"version":"1","scriptlets":[
            {"names":["set-constant","set.js","ubo-set.js"],"scriptlet":"function setConstant(source,args){window.__cf=[source.name].concat(args)}"},
            {"names":["trusted-click-element"],"scriptlet":"function t(source,args){window.__clicked=args}"}]}"#;
        engine.use_resources(scriptlet_resources(library));
        engine
    }

    #[test]
    fn reddit_promoted_posts_are_hidden() {
        let engine = engine_with("reddit.com##shreddit-ad-post\nreddit.com##[data-faceplate-tracking-context*=\"\\\"promoted\\\":true\"]", false);
        let tags   = String::from_utf8(page_tags(&engine, &[], "https://www.reddit.com/", "N").unwrap().to_vec()).unwrap();
        assert!(tags.starts_with("<style nonce=\"N\">"));
        assert!(tags.contains("shreddit-ad-post{display:none!important}"));
        assert!(page_tags(&engine, &[], "https://example.com/", "N").is_none(), "rules are site-specific");
    }

    #[test]
    fn scriptlets_run_with_both_syntaxes() {
        let engine = engine_with("a.com##+js(set, ads, false)\nb.com#%#//scriptlet('set-constant', 'x', '1')", false);
        let a = String::from_utf8(page_tags(&engine, &[], "https://a.com/", "N").unwrap().to_vec()).unwrap();
        let b = String::from_utf8(page_tags(&engine, &[], "https://b.com/", "N").unwrap().to_vec()).unwrap();
        assert!(a.contains("cf_set_constant(\"ads\", \"false\")"), "{a}");
        assert!(b.contains("cf_set_constant(\"x\", \"1\")"), "{b}");
        assert!(a.contains("<script nonce=\"N\">"));
    }

    #[test]
    fn trusted_scriptlets_need_a_trusted_list() {
        let rule = "a.com#%#//scriptlet('trusted-click-element', 'button')";
        assert!(page_tags(&engine_with(rule, false), &[], "https://a.com/", "N").is_none());
        assert!(page_tags(&engine_with(rule, true), &[], "https://a.com/", "N").is_some_and(|t| String::from_utf8_lossy(&t).contains("cf_trusted_click_element")));
    }

    #[test]
    fn procedural_rules_go_to_the_script() {
        let engine = engine_with("a.com##div:has-text(Sponsored):upward(2)\na.com##.x:remove()", false);
        let tags   = String::from_utf8(page_tags(&engine, &[], "https://a.com/", "N").unwrap().to_vec()).unwrap();
        assert!(tags.contains("\"has-text\"") && tags.contains("\"upward\"") && tags.contains("\"remove\""), "{tags}");
        assert!(!tags.contains("__CF_RULES__"));
    }

    #[test]
    fn selectors_cannot_break_out_of_style() {
        let engine = engine_with("a.com##div[x=\"</style><script>alert(1)</script>\"]", false);
        assert!(page_tags(&engine, &[], "https://a.com/", "N").is_none_or(|t| !String::from_utf8_lossy(&t).contains("alert(1)")));
    }

    #[test]
    fn each_userscript_gets_its_own_nonced_tag() {
        let engine = engine_with("", false);
        // Userscripts arrive already escaped (userscripts::build); only the tag is added here
        let tags   = String::from_utf8(page_tags(&engine, &["one()", "two()"], "https://a.com/", "N").unwrap().to_vec()).unwrap();
        assert_eq!(tags, "<script nonce=\"N\">one()</script><script nonce=\"N\">two()</script>");
    }

    #[test]
    fn scriptlet_arguments_cannot_close_the_script_tag() {
        let engine = engine_with("a.com##+js(set-constant, x, </sCRipt><script src=//evil/x.js></script>)\nb.com##+js(set-constant, y, <!--)", false);
        for url in ["https://a.com/", "https://b.com/"] {
            let tags  = String::from_utf8_lossy(&page_tags(&engine, &[], url, "N").unwrap()).to_ascii_lowercase();
            assert!(tags.contains("cf_set_constant"), "{tags}");
            let inner = tags.trim_start_matches("<script nonce=\"n\">").trim_end_matches("</script>");
            assert!(!inner.contains("</script") && !inner.contains("<!--"), "{url}: {tags}");
        }
        assert_eq!(escape_script("a</ScRiPt>b<!--c</x"), "a\\x3C/ScRiPt>b\\x3C!--c</x");
    }
}
