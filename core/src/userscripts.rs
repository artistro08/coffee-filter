//! Userscripts: small scripts you add that change how sites look or work, in the same
//! `.user.js` format as Tampermonkey, Greasemonkey, and AdGuard (Greasy Fork, OpenUserJS, ...).
//!
//! Each script is downloaded once (with its `@require` files) and cached. On every HTML
//! page whose address matches its `@match` / `@include` lines (and none of its `@exclude`
//! lines), it runs in its own `<script>` tag next to the cosmetic one, so one broken
//! script can't stop the others. Scripts run as the page itself, like `@grant none`,
//! with the common `GM_*` helpers provided (see userscripts.js).

use crate::{
    config::{Config, Userscript, data_dir, write_atomic},
    filter,
};
use http_body_util::{BodyExt, Limited};
use hudsucker::{
    Body, decode_response,
    hyper::{Response, StatusCode, Uri, header::{CONTENT_SECURITY_POLICY, CONTENT_TYPE}},
};
use regex::Regex;
use std::{fs, io, path::PathBuf};

/// Scripts (and their `@require` files together) bigger than this are refused.
const MAX_SCRIPT_BYTES: usize = 5 * 1024 * 1024;

const RUNNER_JS: &str = include_str!("userscripts.js");

/// One address rule from a script's header.
enum Rule {
    /// `@match`: Chrome-style `scheme://host/path` pattern
    Match(String),
    /// `@include` / `@exclude`: `*` wildcard over the whole address
    Glob(String),
    /// `@include /regex/`
    Regex(Regex),
}

/// A loaded script, ready to inject.
pub struct Script {
    includes: Vec<Rule>,
    excludes: Vec<Rule>,
    /// The whole `<script>` body: runner, helpers, `@require` files, and the script itself.
    js:       String,
}

/// What a script's `// ==UserScript==` header says.
#[derive(Default, Debug)]
pub struct Header {
    pub name:     String,
    pub version:  String,
    matches:      Vec<String>,
    includes:     Vec<String>,
    excludes:     Vec<String>,
    excl_matches: Vec<String>,
    requires:     Vec<String>,
    start:        bool,
    noframes:     bool,
}

// =========================================================================
// HEADER
// =========================================================================

/// Read the `// ==UserScript== ... // ==/UserScript==` block. None if it's missing or has no `@name`.
pub fn parse_header(code: &str) -> Option<Header> {
    let start = code.find("==UserScript==")?;
    let end   = code[start..].find("==/UserScript==")? + start;

    let mut header = Header::default();
    for line in code[start..end].lines() {
        let Some(line) = line.trim().strip_prefix("//").map(str::trim) else { continue };
        let Some(line) = line.strip_prefix('@') else { continue };
        let (key, value) = line.split_once(char::is_whitespace).map_or((line, ""), |(k, v)| (k, v.trim()));
        match key {
            "name" => header.name = value.to_string(),
            "version" => header.version = value.to_string(),
            "match" => header.matches.push(value.to_string()),
            "include" => header.includes.push(value.to_string()),
            "exclude" => header.excludes.push(value.to_string()),
            "exclude-match" => header.excl_matches.push(value.to_string()),
            "require" => header.requires.push(value.to_string()),
            "run-at" => header.start = value == "document-start",
            "noframes" => header.noframes = true,
            _ => {}
        }
    }
    (!header.name.is_empty()).then_some(header)
}

fn rule(value: &str, is_match: bool) -> Option<Rule> {
    if is_match {
        return Some(Rule::Match(value.to_string()));
    }
    match value.strip_prefix('/').and_then(|v| v.strip_suffix('/')) {
        // Case-insensitive, like Tampermonkey
        Some(re) => Regex::new(&format!("(?i){re}")).ok().map(Rule::Regex),
        None => Some(Rule::Glob(value.to_string())),
    }
}

// =========================================================================
// MATCHING
// =========================================================================

/// `*` matches any run of characters, everything else matches itself.
fn glob(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi  += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
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
    p[pi..].iter().all(|&c| c == b'*')
}

/// Chrome match pattern: `*://*.example.com/path*`, or `<all_urls>`.
fn match_pattern(pattern: &str, uri: &Uri) -> bool {
    let scheme = uri.scheme_str().unwrap_or("");
    if pattern == "<all_urls>" {
        return matches!(scheme, "http" | "https");
    }
    let Some((want_scheme, rest)) = pattern.split_once("://") else { return false };
    let (want_host, want_path) = rest.split_once('/').map_or((rest, "/".to_string()), |(h, p)| (h, format!("/{p}")));

    let host       = uri.host().unwrap_or("").to_ascii_lowercase();
    let want_host  = want_host.to_ascii_lowercase();
    let want_host  = want_host.split(':').next().unwrap_or("");
    let scheme_ok  = if want_scheme == "*" { matches!(scheme, "http" | "https") } else { want_scheme == scheme };
    let host_ok    = want_host == "*"
        || want_host == host
        || want_host.strip_prefix("*.").is_some_and(|base| host == base || host.strip_suffix(base).is_some_and(|sub| sub.ends_with('.')));
    let path       = uri.path_and_query().map_or("/", |p| p.as_str());

    scheme_ok && host_ok && glob(&want_path, path)
}

fn rule_matches(rule: &Rule, url: &str, uri: &Uri) -> bool {
    match rule {
        Rule::Match(p) => match_pattern(p, uri),
        Rule::Glob(p) => glob(p, url),
        Rule::Regex(re) => re.is_match(url),
    }
}

impl Script {
    fn runs_on(&self, url: &str, uri: &Uri) -> bool {
        // No @match or @include means every page, like Greasemonkey
        let included = self.includes.is_empty() || self.includes.iter().any(|r| rule_matches(r, url, uri));
        included && !self.excludes.iter().any(|r| rule_matches(r, url, uri))
    }
}

/// The script bodies (one per `<script>` tag) that run on this page.
pub fn for_page<'a>(scripts: &'a [Script], url: &str) -> Vec<&'a str> {
    let Ok(uri) = url.parse::<Uri>() else { return vec![] };
    scripts.iter().filter(|s| s.runs_on(url, &uri)).map(|s| s.js.as_str()).collect()
}

// =========================================================================
// LOADING
// =========================================================================

/// Cached files for a script: the script and its `@require` files, named by the URL's hash.
fn cache_paths(url: &str) -> io::Result<(PathBuf, PathBuf)> {
    let hash = sha1_smol::Sha1::from(url).digest().to_string();
    let dir  = data_dir()?.join("userscripts");
    fs::create_dir_all(&dir)?;
    Ok((dir.join(format!("{}.user.js", &hash[..16])), dir.join(format!("{}.require.js", &hash[..16]))))
}

/// Wrap a script for the page: its own copy of the helpers, run timing, frame check, and
/// a try/catch so its errors are logged instead of breaking the page.
fn build(header: &Header, requires: &str, code: &str) -> Script {
    let name    = serde_json::to_string(&header.name).unwrap_or_default();
    let version = serde_json::to_string(&header.version).unwrap_or_default();
    let frames  = if header.noframes { "if (window.top !== window) { return; }" } else { "" };
    let snippet = format!(
        "(() => {{ {frames}
const {{ GM, GM_info, GM_getValue, GM_setValue, GM_deleteValue, GM_listValues, GM_addStyle, GM_addElement, GM_openInTab, GM_setClipboard, GM_registerMenuCommand, GM_unregisterMenuCommand, GM_log, GM_xmlhttpRequest }} = helpers({name}, {version});
const unsafeWindow = window;
when({start}, () => {{ try {{
{requires}
{code}
}} catch (e) {{ console.error(\"[userscript] \" + {name} + \" failed:\", e); }} }});
}})();",
        start = header.start,
    );

    let includes = header.matches.iter().filter_map(|m| rule(m, true)).chain(header.includes.iter().filter_map(|i| rule(i, false))).collect();
    let excludes = header.excl_matches.iter().filter_map(|m| rule(m, true)).chain(header.excludes.iter().filter_map(|e| rule(e, false))).collect();
    // Escaped here once, not on every page that runs it (scripts can be megabytes)
    Script { includes, excludes, js: crate::cosmetic::escape_script(&RUNNER_JS.replace("__CF_SCRIPTS__", &snippet)) }
}

/// Download a script and its `@require` files and cache them. Returns its header.
/// Only `https://` is accepted for both, so nothing can be swapped in on the way.
pub async fn download(client: &reqwest::Client, url: &str) -> Result<Header, String> {
    let bytes  = filter::fetch(client, url, MAX_SCRIPT_BYTES).await.map_err(|e| e.to_string())?;
    let code   = String::from_utf8(bytes).map_err(|_| "That file isn't a userscript.")?;
    let header = parse_header(&code).ok_or("That file isn't a userscript (no ==UserScript== header with a @name).")?;

    let mut requires = String::new();
    for req in &header.requires {
        if !req.starts_with("https://") {
            return Err(format!("{}: @require {req} must use https://", header.name));
        }
        let bytes = filter::fetch(client, req, MAX_SCRIPT_BYTES).await.map_err(|e| format!("{}: @require {req}: {e}", header.name))?;
        requires.push_str(&String::from_utf8_lossy(&bytes));
        requires.push_str(";\n");
        if requires.len() > MAX_SCRIPT_BYTES {
            return Err(format!("{}: @require files are too large", header.name));
        }
    }

    let (script_path, require_path) = cache_paths(url).map_err(|e| e.to_string())?;
    write_atomic(&require_path, requires.as_bytes()).map_err(|e| e.to_string())?;
    write_atomic(&script_path, code.as_bytes()).map_err(|e| e.to_string())?;
    Ok(header)
}

/// Re-download every userscript. Returns one message per failure.
pub async fn download_all(scripts: &[Userscript]) -> Vec<String> {
    let client = match filter::client() {
        Ok(c) => c,
        Err(e) => return vec![e.to_string()],
    };
    let mut errors = vec![];
    for script in scripts {
        if let Err(e) = download(&client, &script.url).await {
            errors.push(format!("{}: {e}", script.name));
        }
    }
    errors
}

/// Remove a script's cached files.
pub fn forget(url: &str) {
    if let Ok((script, require)) = cache_paths(url) {
        let _ = fs::remove_file(script);
        let _ = fs::remove_file(require);
    }
}

/// Load every enabled script, downloading any that aren't cached yet.
pub async fn load(config: &Config) -> Vec<Script> {
    let enabled: Vec<&Userscript> = config.userscripts.iter().filter(|s| s.enabled).collect();
    let missing: Vec<Userscript>  = enabled.iter().filter(|s| !cache_paths(&s.url).is_ok_and(|(p, _)| p.exists())).map(|s| (*s).clone()).collect();
    if !missing.is_empty() {
        download_all(&missing).await;
    }

    enabled
        .iter()
        .filter_map(|s| {
            let (script, require) = cache_paths(&s.url).ok()?;
            let code     = fs::read_to_string(script).ok()?;
            let requires = fs::read_to_string(require).unwrap_or_default();
            Some(build(&parse_header(&code)?, &requires, &code))
        })
        .collect()
}

// =========================================================================
// INSTALL PAGE
// =========================================================================

/// Opening a `.user.js` link in the browser.
pub fn is_install_link(uri: &Uri) -> bool {
    uri.scheme_str() == Some("https") && uri.path().to_ascii_lowercase().ends_with(".user.js")
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// Percent-encode everything but unreserved characters, for the `coffee-filter://` link.
fn encode_component(text: &str) -> String {
    text.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// The page for a userscript opened in the browser: its name and code, and an Install
/// button that hands the address to the app (`coffee-filter://install?url=...`). The
/// browser asks before opening the app, and the app asks again, so a site can't add a
/// script on its own. The page runs no script of its own (its CSP forbids it).
fn install_html(header: &Header, url: &str, code: &str) -> String {
    let install = format!("coffee-filter://install?url={}", encode_component(url));
    format!(
        "<!DOCTYPE html><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>Install {name}</title>\
<style>body{{font:15px system-ui,sans-serif;margin:2em auto;max-width:960px;padding:0 16px;color:#222;background:#fff}}\
a.b{{display:inline-block;background:#6b4226;color:#fff;padding:.6em 1.4em;border-radius:6px;text-decoration:none;font-weight:600}}\
pre{{background:#f4f4f4;padding:1em;overflow:auto;border-radius:6px;font-size:13px}}\
@media (prefers-color-scheme:dark){{body{{color:#ddd;background:#1e1e1e}}pre{{background:#2a2a2a}}}}</style>\
<h1>{name} <small>{version}</small></h1>\
<p>This is a userscript. Install it in Coffee Filter to run it on the sites it lists. Only install scripts you trust: they can see and change those pages.</p>\
<p><a class=\"b\" href=\"{install}\">Install in Coffee Filter</a></p>\
<p><small>{shown_url}</small></p><pre>{code}</pre>",
        name      = escape_html(&header.name),
        version   = escape_html(&header.version),
        shown_url = escape_html(url),
        code      = escape_html(code),
    )
}

/// Replace a userscript's raw code with the install page. Anything that isn't a readable
/// userscript is passed on as it came (decoded).
pub async fn install_page(res: Response<Body>, url: &str) -> Response<Body> {
    if res.status() != StatusCode::OK {
        return res;
    }
    let Ok(res) = decode_response(res) else { return res_error() };
    let (mut parts, body) = res.into_parts();
    let Ok(bytes) = Limited::new(body, MAX_SCRIPT_BYTES).collect().await.map(|c| c.to_bytes()) else { return res_error() };

    let Some((header, code)) = std::str::from_utf8(&bytes).ok().and_then(|code| Some((parse_header(code)?, code))) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    let html = install_html(&header, url, code);

    // Fresh headers: none of the site's (Refresh, Set-Cookie, ...) carry over to our page
    parts.headers = Default::default();
    parts.headers.insert(CONTENT_TYPE, "text/html; charset=utf-8".parse().expect("static header"));
    parts.headers.insert(CONTENT_SECURITY_POLICY, "default-src 'none'; style-src 'unsafe-inline'".parse().expect("static header"));
    parts.headers.insert("cache-control", "no-store".parse().expect("static header"));
    Response::from_parts(parts, Body::from(html))
}

fn res_error() -> Response<Body> {
    Response::builder().status(StatusCode::BAD_GATEWAY).body(Body::empty()).expect("static response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_page_escapes_and_links_to_app() {
        let code   = "// ==UserScript==\n// @name <b>x</b>\n// ==/UserScript==\nif (a < b) {}";
        let html   = install_html(&parse_header(code).unwrap(), "https://g.org/a b.user.js?x=1&y=2", code);
        assert!(html.contains("coffee-filter://install?url=https%3A%2F%2Fg.org%2Fa%20b.user.js%3Fx%3D1%26y%3D2"));
        assert!(html.contains("&lt;b&gt;x&lt;/b&gt;") && html.contains("if (a &lt; b)"));
        assert!(!html.contains("<b>") && !html.contains("<script"));
        assert!(is_install_link(&"https://greasyfork.org/scripts/1/code/X.user.js".parse().unwrap()));
        assert!(!is_install_link(&"https://greasyfork.org/scripts/1/code/X.js".parse().unwrap()));
    }

    const SCRIPT: &str = "// ==UserScript==
// @name        Test Script
// @version     1.2
// @match       https://*.example.com/watch*
// @include     /^https://regex\\.test/
// @exclude     https://www.example.com/watch?v=skip*
// @run-at      document-start
// @noframes
// ==/UserScript==
document.title = 'hi';";

    fn script() -> Script {
        build(&parse_header(SCRIPT).unwrap(), "", SCRIPT)
    }

    #[test]
    fn reads_header() {
        let header = parse_header(SCRIPT).unwrap();
        assert_eq!(header.name, "Test Script");
        assert_eq!(header.version, "1.2");
        assert!(header.start && header.noframes);
        assert!(parse_header("console.log(1)").is_none());
        assert!(parse_header("// ==UserScript==\n// @match *://*/*\n// ==/UserScript==").is_none(), "needs a @name");
    }

    #[test]
    fn matches_like_tampermonkey() {
        let s = script();
        assert_eq!(for_page(std::slice::from_ref(&s), "https://www.example.com/watch?v=1").len(), 1);
        assert_eq!(for_page(std::slice::from_ref(&s), "https://example.com/watch").len(), 1);
        assert_eq!(for_page(std::slice::from_ref(&s), "https://REGEX.test/page").len(), 1);
        assert!(for_page(std::slice::from_ref(&s), "https://www.example.com/watch?v=skip1").is_empty(), "excluded");
        assert!(for_page(std::slice::from_ref(&s), "https://www.example.com/other").is_empty());
        assert!(for_page(std::slice::from_ref(&s), "https://notexample.com/watch").is_empty());
        assert!(for_page(std::slice::from_ref(&s), "http://www.example.com/watch").is_empty(), "scheme is https only");
    }

    #[test]
    fn no_rules_means_every_page() {
        let code = "// ==UserScript==\n// @name All\n// ==/UserScript==\n";
        let s    = build(&parse_header(code).unwrap(), "", code);
        assert_eq!(for_page(std::slice::from_ref(&s), "https://anything.com/").len(), 1);
    }

    #[test]
    fn glob_wildcards() {
        assert!(glob("*", ""));
        assert!(glob("https://*.com/*", "https://a.com/b"));
        assert!(glob("a*b*c", "aXXbYYc"));
        assert!(!glob("a*b*c", "aXXbYY"));
        assert!(!glob("abc", "abcd"));
    }

    #[test]
    fn script_is_wrapped_and_isolated() {
        let js = &script().js;
        assert!(js.contains("document.title = 'hi';"));
        assert!(js.contains("helpers(\"Test Script\", \"1.2\")"));
        assert!(js.contains("when(true,"), "document-start runs right away");
        assert!(js.contains("window.top !== window"), "@noframes");
        assert!(!js.contains("__CF_SCRIPTS__"));

        // Escaped once at load, so a closing tag in the code can't end its <script>
        let code = "// ==UserScript==\n// @name X\n// ==/UserScript==\nx = '</SCRIPT>';";
        let js   = build(&parse_header(code).unwrap(), "", code).js;
        assert!(!js.to_ascii_lowercase().contains("</script") && js.contains("x = '\\x3C/SCRIPT>';"));
    }
}
