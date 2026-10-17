//! Filter lists, the blocking engine, and the hudsucker request handler.

use crate::{
    apps,
    config::{Config, ExcludedApp, FilterList, data_dir, write_atomic},
    cosmetic, sysproxy, userscripts,
};
use adblock::{Engine, lists::{FilterFormat, FilterSet, ParseOptions}, request::Request as AdRequest, resources::PermissionMask};
use hyper_util::client::legacy::connect::dns::{GaiResolver, Name};
use hudsucker::{
    Body, HttpContext, HttpHandler, RequestOrResponse,
    rustls::server::ClientHello,
    futures::future::join_all,
    hyper::{Method, Request, Response, StatusCode},
};
use std::{
    fs, io,
    future::Future,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    task::{Context, Poll},
    path::PathBuf,
    collections::VecDeque,
    sync::{Arc, Mutex, RwLock, atomic::{AtomicBool, AtomicU64, Ordering}},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Lists bigger than this are refused, so a bad custom list can't exhaust memory.
const MAX_LIST_BYTES: usize = 64 * 1024 * 1024;

/// State shared by every proxied connection.
pub struct Shared {
    pub engine:      RwLock<Arc<Engine>>,
    /// The PAC script and the random path it's served on for this run (see sysproxy).
    pub pac:         RwLock<(String, String)>,
    pub exclusions:  RwLock<Vec<String>>,
    pub userscripts: RwLock<Arc<Vec<userscripts::Script>>>,
    pub blocked:     AtomicU64,
    pub allowed:     AtomicU64,
    pub log:         Log,
    /// Resolves short links (see SHORTENERS).
    pub resolver:    reqwest::Client,
    pub apps:        RwLock<Arc<Vec<ExcludedApp>>>,
    /// Per connection: does it come from an excluded app?
    pub app_cache:   apps::Cache,
}

/// Recently blocked requests for the app's live log. Recording is off (one atomic read per
/// blocked request) unless the log is open in the app.
#[derive(Default)]
pub struct Log {
    on:      AtomicBool,
    /// Last id handed out, and the newest entries.
    entries: Mutex<(u64, VecDeque<LogEntry>)>,
}

#[derive(Clone, serde::Serialize)]
pub struct LogEntry {
    id:   u64,
    /// Unix time in milliseconds.
    time: u64,
    url:  String,
    kind: &'static str,
    /// Site the request came from, if known.
    page: Option<String>,
}

impl Log {
    const MAX: usize = 300;

    fn record(&self, url: &str, kind: &'static str, source: &str) {
        if !self.on.load(Ordering::Relaxed) {
            return;
        }
        let time  = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let page  = source.parse::<hudsucker::hyper::Uri>().ok().and_then(|u| u.host().map(str::to_string));
        let url   = url.chars().take(500).collect();
        let mut g = self.entries.lock().unwrap();
        g.0 += 1;
        let id = g.0;
        if g.1.len() >= Self::MAX {
            g.1.pop_front();
        }
        g.1.push_back(LogEntry { id, time, url, kind, page });
    }

    /// Entries newer than `after`, oldest first.
    /// An `after` from a previous core run (larger than any id here) means "everything".
    pub fn since(&self, after: u64) -> Vec<LogEntry> {
        let g     = self.entries.lock().unwrap();
        let after = if after > g.0 { 0 } else { after };
        g.1.iter().filter(|e| e.id > after).cloned().collect()
    }

    /// Start recording, or stop and forget everything recorded.
    pub fn set(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
        if !on {
            self.entries.lock().unwrap().1.clear();
        }
    }
}

// =========================================================================
// FILTER LISTS
// =========================================================================

/// Cached copy of a list. The name keeps it readable; the URL hash keeps two lists whose
/// names differ only in punctuation from sharing a file.
fn cache_path(list: &FilterList) -> io::Result<PathBuf> {
    let name: String = list.name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let hash = sha1_smol::Sha1::from(&list.url).digest().to_string();
    let dir  = data_dir()?.join("lists");
    fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{name}-{}.txt", &hash[..8])))
}

/// Fetch one URL with a size cap.
pub async fn fetch(client: &reqwest::Client, url: &str, max: usize) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let mut response = client.get(url).send().await?.error_for_status()?;
    if response.content_length().is_some_and(|len| len as usize > max) {
        return Err("file is too large".into());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        body.extend_from_slice(&chunk);
        if body.len() > max {
            return Err("file is too large".into());
        }
    }
    Ok(body)
}

/// Download client with time limits, so one slow server can't hold up the rest. Uses a
/// direct connection so it never loops through our own proxy.
pub fn client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
}

/// Download every enabled list and the scriptlet library, all at once.
pub async fn download_lists(config: &Config) -> Vec<String> {
    let client = match client() {
        Ok(c) => c,
        Err(e) => return vec![e.to_string()],
    };

    let lists = config.lists.iter().filter(|l| l.enabled).map(|list| {
        let client = &client;
        async move {
            let body = fetch(client, &list.url, MAX_LIST_BYTES).await?;
            write_atomic(&cache_path(list)?, &body)?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }
    });
    let library = async {
        let body = fetch(&client, cosmetic::SCRIPTLETS_URL, MAX_LIST_BYTES).await?;
        cosmetic::verify_library(&body)?;
        write_atomic(&data_dir()?.join("scriptlets.json"), &body)?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    };

    let names = config.lists.iter().filter(|l| l.enabled).map(|l| l.name.clone());
    let (results, library) = tokio::join!(join_all(lists), library);

    let mut errors: Vec<String> = names.zip(results).filter_map(|(name, r)| r.err().map(|e| format!("{name}: {e}"))).collect();
    if let Err(e) = library {
        errors.push(format!("Scriptlets: {e}"));
    }
    errors
}

/// Build an engine from cached lists, downloading any that are missing. Compiling is
/// CPU-heavy, so it runs off the proxy's worker threads.
pub async fn build_engine(config: &Config) -> (Engine, usize) {
    let library = data_dir().map(|d| d.join("scriptlets.json"));
    let missing = config.lists.iter().any(|l| l.enabled && !cache_path(l).is_ok_and(|p| p.exists()))
        || !library.as_ref().is_ok_and(|p| p.exists());
    if missing {
        download_lists(config).await;
    }

    let config = config.clone();
    tokio::task::spawn_blocking(move || compile(&config, library.ok()))
        .await
        .unwrap_or_else(|_| (Engine::default(), 0))
}

fn compile(config: &Config, library: Option<PathBuf>) -> (Engine, usize) {
    // Reuse the compiled engine when the enabled lists haven't changed; parsing ~1M rules costs about a second of CPU
    let key = engine_key(config);
    let (mut engine, rules) = match load_cached_engine(&key) {
        Some(cached) => cached,
        None => {
            let mut set   = FilterSet::new(false);
            let mut rules = 0;
            for list in config.lists.iter().filter(|l| l.enabled) {
                if let Ok(text) = cache_path(list).and_then(fs::read_to_string) {
                    rules += text.lines().filter(|l| is_rule(l)).count();
                    add_list(&mut set, text, list.trusted);
                }
            }
            add_list(&mut set, BUILTIN_RULES.to_string(), false);
            let engine = Engine::new_with_filter_set(set);

            if let Ok(dir) = data_dir() {
                let _ = write_atomic(&dir.join("engine.dat"), &engine.serialize());
                let _ = write_atomic(&dir.join("engine.key"), format!("{rules}\n{key}").as_bytes());
            }
            (engine, rules)
        }
    };

    // Scriptlets aren't part of the saved engine; load them every start (only if the file is the pinned version)
    if let Some(json) = library.and_then(|p| fs::read(p).ok()).filter(|b| cosmetic::verify_library(b).is_ok()) {
        engine.use_resources(cosmetic::scriptlet_resources(&String::from_utf8_lossy(&json)));
    }
    (engine, rules)
}

/// Identifies the exact list files an engine was built from (name, size, modified time).
fn engine_key(config: &Config) -> String {
    config
        .lists
        .iter()
        .filter(|l| l.enabled)
        .map(|l| {
            let meta = cache_path(l).and_then(fs::metadata).ok();
            let size = meta.as_ref().map_or(0, |m| m.len());
            let time = meta.and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
            format!("{}|{}|{size}|{}", l.name, l.trusted, time.map_or(0, |t| t.as_nanos()))
        })
        .collect::<Vec<_>>()
        .join("\n")
        // Bump when the way lists are compiled changes, so old engine.dat files are rebuilt
        + "\nformat:4"
        + "\nbuiltin:" + &sha1_smol::Sha1::from(BUILTIN_RULES).digest().to_string()
}

fn load_cached_engine(key: &str) -> Option<(Engine, usize)> {
    let dir          = data_dir().ok()?;
    let saved        = fs::read_to_string(dir.join("engine.key")).ok()?;
    let (rules, old) = saved.split_once('\n')?;
    if old != key {
        return None;
    }
    let mut engine = Engine::default();
    engine.deserialize(&fs::read(dir.join("engine.dat")).ok()?).ok()?;
    Some((engine, rules.parse().ok()?))
}

/// Coffee Filter's own fixes, for ads the public lists don't catch yet. Always on.
///
/// X marks promoted posts with a small "Ad" label (the lists still look for "Promoted"), and
/// keeps ad slots in `placementTracking` cells.
const BUILTIN_RULES: &str = r#"x.com,twitter.com##div[data-testid="cellInnerDiv"]:has(div[data-testid="placementTracking"])
x.com,twitter.com##article[data-testid="tweet"] span:has-text(/^Ad$/):upward(div[data-testid="cellInnerDiv"])
"#;

/// Add one list, detecting hosts-file format (`0.0.0.0 domain`) from its first rule.
/// Network and cosmetic rules are both kept; AdGuard-only syntax is translated first.
fn add_list(set: &mut FilterSet, text: String, trusted: bool) {
    let first  = text.lines().map(|l| l.trim().trim_start_matches('\u{feff}')).find(|l| !l.is_empty() && !l.starts_with(['#', '!', '[']));
    let hosts  = first.is_some_and(|l| l.starts_with("0.0.0.0") || l.starts_with("127.0.0.1") || l.starts_with("::"));
    let format = if hosts { FilterFormat::Hosts } else { FilterFormat::Standard };
    let text   = if hosts { text } else { cosmetic::translate_adguard(&text) };
    let perms  = if trusted { cosmetic::TRUSTED } else { PermissionMask::default() };
    set.add_filter_list(text, ParseOptions { format, permissions: perms, ..Default::default() });
}

/// Rough count for the UI: skips blanks, comments, and headers.
fn is_rule(line: &str) -> bool {
    let line = line.trim();
    !line.is_empty() && !line.starts_with(['!', '[']) && !(line.starts_with('#') && !line.starts_with("##") && !line.starts_with("#@#"))
}

// =========================================================================
// REQUEST HANDLER
// =========================================================================

/// Map the browser's `Sec-Fetch-Dest` header to an adblock request type.
fn request_type(dest: Option<&str>) -> &'static str {
    match dest {
        Some("document") => "document",
        Some("iframe" | "frame") => "subdocument",
        Some("script" | "worker" | "sharedworker" | "serviceworker") => "script",
        Some("image") => "image",
        Some("style") => "stylesheet",
        Some("font") => "font",
        Some("audio" | "video" | "track") => "media",
        Some("empty") => "xmlhttprequest",
        Some("websocket") => "websocket",
        _ => "other",
    }
}

/// `host` is `ex` or a subdomain of it, in any letter case (exclusions are saved lowercase,
/// but apps may send `Bank.COM`). No allocation: it runs on every connection.
pub fn is_excluded<S: AsRef<str>>(exclusions: &[S], host: &str) -> bool {
    let host = host.trim_end_matches('.').as_bytes();
    exclusions.iter().any(|ex| {
        let ex = ex.as_ref().as_bytes();
        host.len() >= ex.len()
            && host[host.len() - ex.len()..].eq_ignore_ascii_case(ex)
            && (host.len() == ex.len() || host[host.len() - ex.len() - 1] == b'.')
    })
}

/// This PC and home-network devices (routers, docks, NAS, smart home, Tailscale). Their
/// certificates are usually self-made, and there are no ads there, so they're never decrypted.
pub fn is_private_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_private_ip(ip);
    }
    host == "localhost" || sysproxy::LOCAL_SUFFIXES.iter().any(|s| host.ends_with(s)) || !host.contains('.')
}

/// DNS for the proxy's own connections. A public site name that resolves to this PC or the
/// home network is refused: otherwise a web page could flip its DNS to 127.0.0.1 or the router
/// and read local services through the proxy, where the browser can't see the address to stop it.
/// Local names (`nas.local`, `router`, IP addresses) resolve as usual.
#[derive(Clone)]
pub struct PublicOnly(GaiResolver);

impl Default for PublicOnly {
    fn default() -> Self {
        Self(GaiResolver::new())
    }
}

impl tower_service::Service<Name> for PublicOnly {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error    = io::Error;
    type Future   = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let local   = is_private_host(name.as_str());
        let resolve = self.0.call(name);
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = resolve.await?.filter(|a| local || !is_private_ip(a.ip())).collect();
            if addrs.is_empty() {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "site name points at a local address"));
            }
            Ok(addrs.into_iter())
        })
    }
}

fn is_private_ip(ip: IpAddr) -> bool {
    // `::ffff:127.0.0.1` is the IPv4 address wearing IPv6 clothes
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified() || v6.is_unique_local() || v6.is_unicast_link_local(),
    }
}

/// The address as a browser shows it: HTTP/1.1 requests through a tunnel carry `:443`, HTTP/2
/// ones don't, so the default port is dropped and rules and userscripts see one form.
fn page_url(uri: &hudsucker::hyper::Uri) -> String {
    let url = uri.to_string();
    let default = match uri.scheme_str() {
        Some("https") => ":443",
        Some("http") => ":80",
        _ => return url,
    };
    match uri.host() {
        Some(host) if uri.port().is_some_and(|p| default == format!(":{}", p.as_str())) => url.replacen(&format!("{host}{default}"), host, 1),
        _ => url,
    }
}

/// What a public site name was found to point at (see `Blocker::resolve_public`).
enum Resolved {
    /// Only local addresses: refused.
    Local,
    /// A public address, the one to connect to.
    Public(SocketAddr),
    /// Local or excluded names, and lookups that failed: left as they are.
    Unchecked,
}

fn local_refused() -> RequestOrResponse {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .body(Body::from("This site name points at a local address."))
        .expect("static response")
        .into()
}

/// What a blocked page shows, instead of a blank tab (Windows 11 styled, light and dark), with
/// the app icon built in so the page needs nothing from the network.
static BLOCKED_PAGE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    use base64::{Engine as _, prelude::BASE64_STANDARD};
    let icon = BASE64_STANDARD.encode(include_bytes!("../../app/Assets/Square150x150Logo.png"));
    include_str!("blocked.html").replace("__CF_ICON__", &icon)
});

/// Your Windows accent color as Windows 11 buttons use it: a darker shade in light mode and a
/// lighter one in dark mode, from the accent palette Windows keeps in the registry. Browsers
/// report a stock blue instead of the real color, so the page gets it from here. Read on each
/// blocked page (one small registry read), so a color change shows up right away.
fn accent_shades() -> (String, String) {
    let palette = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent")
        .and_then(|k| k.get_raw_value("AccentPalette"))
        .map(|v| v.bytes)
        .unwrap_or_default();
    // Eight RGBA entries from lightest to darkest: Light3, Light2, Light1, Accent, Dark1, ...
    let shade = |i: usize| palette.get(i * 4..i * 4 + 3).map(|c| format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]));
    match (shade(4), shade(1)) {
        (Some(dark1), Some(light2)) => (dark1, light2),
        // Windows' default blue
        _ => ("#005fb8".into(), "#60cdff".into()),
    }
}

/// The page for one blocked address. Its Exclude button opens the app with the site name
/// (`coffee-filter://exclude?host=...`); the browser and the app each ask before anything changes.
fn blocked_page(url: &str, host: &str, reason: &str) -> String {
    let exclude       = format!("coffee-filter://exclude?host={}", userscripts::encode_component(host));
    let (light, dark) = accent_shades();
    // The address goes in last, so text inside it can never be taken for a placeholder
    BLOCKED_PAGE
        .replace("__CF_ACCENT_LIGHT__", &light)
        .replace("__CF_ACCENT_DARK__", &dark)
        .replace("__CF_EXCLUDE__", &userscripts::escape_html(&exclude))
        .replace("__CF_REASON__", &userscripts::escape_html(reason))
        .replace("__CF_URL__", &userscripts::escape_html(url))
}

/// Why a page was blocked, shown under the title.
const LISTED: &str = "This page is on a filter list for ads, tracking, or harmful content, so it wasn't loaded.";

// =========================================================================
// LINK SHORTENERS
// =========================================================================

/// Shorteners whose links are resolved by Coffee Filter instead of the browser: the browser
/// goes straight to the destination, so the shortener never sees its cookies, the page it came
/// from, or the click.
const SHORTENERS: [&str; 1] = ["t.co"];

/// Query parameters that only track where a click came from.
fn is_tracking_param(name: &str) -> bool {
    name.starts_with("utm_") || matches!(name, "fbclid" | "gclid" | "dclid" | "msclkid" | "mc_eid" | "igshid" | "ref_src" | "ref_url" | "twclid" | "si")
}

/// The destination without its tracking parameters.
fn clean_destination(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else { return url.to_string() };
    let (query, fragment) = query.split_once('#').map_or((query, None), |(q, f)| (q, Some(f)));
    let kept: Vec<&str> = query.split('&').filter(|p| !p.is_empty() && !is_tracking_param(p.split('=').next().unwrap_or(""))).collect();
    let mut out = base.to_string();
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    out
}

/// Ask the shortener where a link goes (no cookies, no referrer, redirects not followed),
/// then send the browser straight there. Anything unexpected shows the blocked page.
async fn resolve_short_link(client: &reqwest::Client, url: &str, host: &str) -> Response<Body> {
    let location = match client.get(url).send().await {
        Ok(r) if r.status().is_redirection() => r.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_string),
        _ => None,
    };
    let redirect = location.filter(|l| l.starts_with("https://") || l.starts_with("http://")).and_then(|dest| {
        Response::builder()
            .status(StatusCode::FOUND)
            .header("location", clean_destination(&dest))
            .header("referrer-policy", "no-referrer")
            .header("cache-control", "no-store")
            .body(Body::empty())
            .ok()
    });
    redirect.unwrap_or_else(|| {
        let page = blocked_page(url, host, "This short link couldn't be opened without its tracking, so it wasn't loaded.");
        Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header("content-type", "text/html; charset=utf-8")
            .body(Body::from(page))
            .expect("static response")
    })
}

/// Client for resolving short links: direct (never through our proxy), no redirects followed,
/// no cookies, short time limit.
pub fn resolver_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
}

/// The handler. hudsucker clones it for each request; `page` carries one request's
/// state over to its own response.
#[derive(Clone)]
pub struct Blocker {
    shared: Arc<Shared>,
    /// URL of the HTML page whose response is in flight, for cosmetic filtering.
    page:   Option<String>,
    /// URL of a userscript opened in the browser, shown as an install page.
    install: Option<String>,
}

impl Blocker {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self { shared, page: None, install: None }
    }

    /// Where a public site name (not excluded) points. One OS lookup, usually from its cache.
    async fn resolve_public(&self, host: &str, port: u16) -> Resolved {
        if is_private_host(host) || is_excluded(&self.shared.exclusions.read().unwrap(), host) {
            return Resolved::Unchecked;
        }
        let Ok(addrs) = tokio::net::lookup_host((host, port)).await else { return Resolved::Unchecked };
        let addrs: Vec<SocketAddr> = addrs.collect();
        match addrs.iter().find(|a| !is_private_ip(a.ip())) {
            Some(addr) => Resolved::Public(*addr),
            // Failures are left to the real connection
            None if addrs.is_empty() => Resolved::Unchecked,
            None => Resolved::Local,
        }
    }

    /// A public site name (not excluded) whose every address is on this PC or the home network.
    async fn points_local(&self, host: &str, port: u16) -> bool {
        matches!(self.resolve_public(host, port).await, Resolved::Local)
    }

    /// Does this connection come from an app you excluded? Free when none are excluded.
    async fn is_excluded_app(&self, ctx: &HttpContext) -> bool {
        let apps = {
            let apps = self.shared.apps.read().unwrap();
            if apps.is_empty() {
                return false;
            }
            apps.clone()
        };
        apps::is_excluded(&apps, &self.shared.app_cache, ctx.client_addr).await
    }

    /// Private hosts, Windows' update and Store servers, and your exclusions are only tunneled.
    fn decrypts(&self, host: &str) -> bool {
        !is_private_host(host) && !is_excluded(&sysproxy::SYSTEM_HOSTS, host) && !is_excluded(&self.shared.exclusions.read().unwrap(), host)
    }
}

impl HttpHandler for Blocker {
    async fn handle_request(&mut self, ctx: &HttpContext, mut req: Request<Body>) -> RequestOrResponse {
        // hudsucker clones the handler per request; reset anyway so state can never leak across
        self.page    = None;
        self.install = None;
        if req.method() == Method::CONNECT {
            // Tunnels that won't be decrypted connect on their own, past our DNS check
            // (PublicOnly). Check here that a public name doesn't point at a local address, then
            // connect to the address that was checked: looking the name up again could give a
            // different, local one (a name can change what it points to between lookups).
            if let Some(host) = req.uri().host().map(str::to_string)
                && req.uri().port_u16() != Some(443)
            {
                let port = req.uri().port_u16().unwrap_or(443);
                match self.resolve_public(&host, port).await {
                    Resolved::Local => return local_refused(),
                    Resolved::Public(addr) => {
                        if let Ok(uri) = addr.to_string().parse() {
                            *req.uri_mut() = uri;
                        }
                    }
                    Resolved::Unchecked => {}
                }
            }
            return req.into();
        }

        // Windows fetches the PAC script straight from this port (not as a proxy request)
        if req.uri().authority().is_none() {
            let pac = self.shared.pac.read().unwrap();
            let response = if req.method() == Method::GET && req.uri().path() == pac.0 {
                Response::builder()
                    .header("content-type", "application/x-ns-proxy-autoconfig")
                    .header("cache-control", "no-store")
                    .body(Body::from(pac.1.clone()))
            } else {
                Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty())
            };
            return response.expect("static response").into();
        }

        // We can't pass on compressed WebSocket frames (excluded or not), so don't let the server agree to them
        let host      = req.uri().host().unwrap_or("").to_string();
        let websocket = req.headers().get("upgrade").is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
        if websocket {
            req.headers_mut().remove("sec-websocket-extensions");
        }

        // Excluded sites and apps over plain HTTP are left alone too: no blocking, no page changes
        let excluded_site = is_excluded(&self.shared.exclusions.read().unwrap(), &host) || is_excluded(&sysproxy::SYSTEM_HOSTS, &host);
        if excluded_site || self.is_excluded_app(ctx).await {
            return req.into();
        }

        // WebSockets connect on their own too, past PublicOnly
        if websocket && self.points_local(&host, req.uri().port_u16().unwrap_or(80)).await {
            return local_refused();
        }

        let header = |name| req.headers().get(name).and_then(|v| v.to_str().ok());
        let url    = page_url(req.uri());
        let source = header("referer").or(header("origin")).unwrap_or(&url);
        let kind   = request_type(header("sec-fetch-dest"));

        // Short links open their destination directly; the browser never contacts the shortener
        if req.method() == Method::GET && kind == "document" && SHORTENERS.contains(&host.as_str()) {
            self.shared.allowed.fetch_add(1, Ordering::Relaxed);
            return resolve_short_link(&self.shared.resolver, &url, &host).await.into();
        }

        let engine = self.shared.engine.read().unwrap().clone();
        let block  = AdRequest::new(&url, source, kind, req.method().as_str())
            .is_ok_and(|r| engine.check_network_request(&r).should_block());

        if !block {
            // Page loads (and frames) get cosmetic filtering on the way back
            let accepts_html = header("accept").is_some_and(|a| a.contains("text/html"));
            let page_load    = matches!(kind, "document" | "subdocument") || (header("sec-fetch-dest").is_none() && accepts_html);
            if req.method() == Method::GET && kind == "document" && userscripts::is_install_link(req.uri()) {
                self.install = Some(url);
            } else if req.method() == Method::GET && page_load {
                self.page = Some(url);
            }
            self.shared.allowed.fetch_add(1, Ordering::Relaxed);
            return req.into();
        }

        self.shared.blocked.fetch_add(1, Ordering::Relaxed);
        self.shared.log.record(&url, kind, source);
        let response = Response::builder().status(StatusCode::FORBIDDEN);
        let response = if kind == "document" {
            response.header("content-type", "text/html; charset=utf-8").body(Body::from(blocked_page(&url, &host, LISTED)))
        } else {
            response.body(Body::empty())
        };
        response.expect("static response").into()
    }

    async fn handle_response(&mut self, _ctx: &HttpContext, res: Response<Body>) -> Response<Body> {
        if let Some(url) = self.install.take() {
            return userscripts::install_page(res, &url).await;
        }
        let Some(url) = self.page.take() else { return res };
        let engine  = self.shared.engine.read().unwrap().clone();
        let scripts = self.shared.userscripts.read().unwrap().clone();
        cosmetic::inject(res, &engine, &userscripts::for_page(&scripts, &url), &url)
    }

    /// Decrypt only HTTPS on 443 for hosts not excluded; everything else is tunneled untouched.
    async fn should_intercept_connect(&mut self, ctx: &HttpContext, req: &Request<Body>) -> bool {
        let Some(host) = req.uri().host() else { return false };
        req.uri().port_u16() == Some(443) && self.decrypts(host) && !self.is_excluded_app(ctx).await
    }

    /// Check again with the site name inside the TLS handshake, which can differ from the
    /// CONNECT address; a certificate is only ever made for a name that passes.
    async fn should_intercept_tls(&mut self, _ctx: &HttpContext, hello: ClientHello<'_>) -> bool {
        hello.server_name().is_some_and(|host| self.decrypts(host))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_listed_and_passes_clean() {
        let mut set = FilterSet::new(false);
        set.add_filter_list("||ads.example.com^\n@@||ads.example.com/ok.js".into(), ParseOptions::default());
        let engine = Engine::new_with_filter_set(set);
        let check  = |u: &str| engine.check_network_request(&AdRequest::new(u, "https://site.com", "script", "GET").unwrap()).should_block();

        assert!(check("https://ads.example.com/x.js"));
        assert!(!check("https://ads.example.com/ok.js"));
        assert!(!check("https://site.com/app.js"));
    }

    #[test]
    fn hosts_file_lists_block_their_domains() {
        let mut set = FilterSet::new(false);
        add_list(&mut set, "# comment\n0.0.0.0 tracker.example.net\n127.0.0.1 ads.example.org".into(), false);
        let engine = Engine::new_with_filter_set(set);
        let check  = |u: &str| engine.check_network_request(&AdRequest::new(u, "https://site.com", "image", "GET").unwrap()).should_block();

        assert!(check("https://tracker.example.net/pixel.gif"));
        assert!(check("https://ads.example.org/banner.png"));
        assert!(!check("https://example.net/"));
    }

    #[test]
    fn saved_engine_blocks_the_same() {
        let mut set = FilterSet::new(false);
        add_list(&mut set, "||ads.example.com^".into(), false);
        add_list(&mut set, "0.0.0.0 hosts.example.net".into(), false);
        let mut engine = Engine::default();
        engine.deserialize(&Engine::new_with_filter_set(set).serialize()).unwrap();
        let check = |u: &str| engine.check_network_request(&AdRequest::new(u, "https://site.com", "script", "GET").unwrap()).should_block();

        assert!(check("https://ads.example.com/a.js"));
        assert!(check("https://hosts.example.net/a.js"));
        assert!(!check("https://site.com/a.js"));
    }

    #[test]
    fn standard_lists_still_parse_as_standard() {
        let mut set = FilterSet::new(false);
        add_list(&mut set, "! Title: test\n||ads.example.com^$third-party".into(), false);
        let engine = Engine::new_with_filter_set(set);
        let req    = AdRequest::new("https://ads.example.com/a.js", "https://site.com", "script", "GET").unwrap();
        assert!(engine.check_network_request(&req).should_block());
    }

    #[test]
    fn resource_type_rules_respect_sec_fetch_dest() {
        let mut set = FilterSet::new(false);
        set.add_filter_list("||cdn.example.com^$script".into(), ParseOptions::default());
        let engine = Engine::new_with_filter_set(set);
        let check  = |dest| {
            let req = AdRequest::new("https://cdn.example.com/a", "https://site.com", request_type(dest), "GET").unwrap();
            engine.check_network_request(&req).should_block()
        };

        assert!(check(Some("script")));
        assert!(!check(Some("image")));
    }

    #[test]
    fn request_type_mapping() {
        assert_eq!(request_type(Some("document")), "document");
        assert_eq!(request_type(Some("iframe")), "subdocument");
        assert_eq!(request_type(Some("serviceworker")), "script");
        assert_eq!(request_type(Some("style")), "stylesheet");
        assert_eq!(request_type(Some("video")), "media");
        assert_eq!(request_type(Some("empty")), "xmlhttprequest");
        assert_eq!(request_type(None), "other");
    }

    #[test]
    fn rule_count_skips_comments() {
        let lines = ["! comment", "# hosts comment", "[Adblock Plus 2.0]", "", "example.com##.ad", "##.generic", "example.com#@#.ad", "||ads.com^", "0.0.0.0 x.com"];
        assert_eq!(lines.iter().filter(|l| is_rule(l)).count(), 5);
    }

    #[tokio::test]
    async fn public_names_cannot_resolve_to_local_addresses() {
        use std::str::FromStr;
        use tower_service::Service;

        let mut dns = PublicOnly::default();
        // A local name may point at this PC
        assert!(dns.call(Name::from_str("localhost").unwrap()).await.is_ok());
        // A public name that resolves to 127.0.0.1 (needs internet; skipped when DNS is unreachable)
        match dns.call(Name::from_str("127.0.0.1.nip.io").unwrap()).await {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {}
            Err(e) => eprintln!("skipped, DNS unavailable: {e}"),
            Ok(addrs) => panic!("resolved to {:?}", addrs.collect::<Vec<_>>()),
        }
    }

    #[test]
    fn home_network_hosts_are_never_decrypted() {
        for host in ["192.168.86.28", "10.0.0.5", "172.20.1.1", "169.254.1.1", "127.0.0.1", "[::1]", "[fd00::1]", "[::ffff:127.0.0.1]", "100.100.1.2", "0.0.0.0", "nas.local", "router", "dock.lan", "app.localhost"] {
            assert!(is_private_host(host), "{host}");
        }
        for host in ["reddit.com", "8.8.8.8", "172.32.0.1", "100.128.0.1", "[2606:4700::1]", "[::ffff:8.8.8.8]"] {
            assert!(!is_private_host(host), "{host}");
        }
    }

    #[test]
    fn short_link_destinations_lose_tracking() {
        assert_eq!(clean_destination("https://a.com/p?utm_source=x&id=7&fbclid=z#top"), "https://a.com/p?id=7#top");
        assert_eq!(clean_destination("https://a.com/p?utm_campaign=x"), "https://a.com/p");
        assert_eq!(clean_destination("https://a.com/p"), "https://a.com/p");
        assert_eq!(clean_destination("https://a.com/?q=utm_source"), "https://a.com/?q=utm_source", "only parameter names count");
    }

    #[test]
    fn blocked_page_shows_the_address_safely() {
        let page = blocked_page("https://evil.com/<script>alert(1)</script>__CF_EXCLUDE__", "evil.com", LISTED);
        assert!(page.contains("https://evil.com/&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!page.contains("<script>alert"));
        assert!(page.contains("&lt;/script&gt;__CF_EXCLUDE__</code>"), "text in the address is shown as is, never filled in");
        for placeholder in ["__CF_ICON__", "__CF_REASON__", "__CF_ACCENT_LIGHT__", "__CF_ACCENT_DARK__", "__CF_URL__"] {
            assert!(!page.contains(placeholder), "{placeholder} is filled");
        }
        assert!(page.contains(&userscripts::escape_html(LISTED)));
        let (light, dark) = accent_shades();
        assert!(page.contains(&format!("--accent: {light};")) && page.contains(&format!("--accent: {dark};")), "Windows accent color");
        assert!(page.contains("data:image/png;base64,iVBOR"), "icon is built in");
        assert!(page.contains("href=\"coffee-filter://exclude?host=evil.com\""), "Exclude opens the app with the site name");
    }

    #[test]
    fn builtin_rules_hide_x_promoted_posts() {
        let mut set = FilterSet::new(false);
        add_list(&mut set, BUILTIN_RULES.to_string(), false);
        let engine = Engine::new_with_filter_set(set);
        let found  = engine.url_cosmetic_resources("https://x.com/home");
        let all    = format!("{:?} {:?}", found.hide_selectors, found.procedural_actions);
        assert!(all.contains("placementTracking") && all.contains("cellInnerDiv"), "{all}");
    }

    #[test]
    fn urls_drop_the_default_port() {
        let url = |u: &str| page_url(&u.parse().unwrap());
        assert_eq!(url("https://www.facebook.com:443/home?x=1"), "https://www.facebook.com/home?x=1");
        assert_eq!(url("http://example.com:80/a"), "http://example.com/a");
        assert_eq!(url("https://example.com:8443/a"), "https://example.com:8443/a");
        assert_eq!(url("https://example.com/a"), "https://example.com/a");
    }

    #[test]
    fn log_ids_from_an_old_run_mean_everything() {
        let log = Log::default();
        log.set(true);
        log.record("https://ads.example.com/a", "image", "https://site.com/");
        assert_eq!(log.since(500).len(), 1, "after a core restart the app still holds the old run's last id");
    }

    #[test]
    fn log_records_only_while_open() {
        let log = Log::default();
        log.record("https://ads.example.com/a.js", "script", "https://site.com/");
        assert!(log.since(0).is_empty(), "off by default");

        log.set(true);
        for i in 0..Log::MAX + 5 {
            log.record(&format!("https://ads.example.com/{i}"), "image", "https://site.com/page");
        }
        let all = log.since(0);
        assert_eq!(all.len(), Log::MAX, "keeps only the newest");
        assert_eq!(all[0].page.as_deref(), Some("site.com"));
        assert_eq!(log.since(all[all.len() - 2].id).len(), 1);

        log.set(false);
        assert!(log.since(0).is_empty(), "closing the log forgets it");
    }

    #[test]
    fn exclusion_matches_subdomains_only() {
        let ex = vec!["bank.com".to_string()];
        assert!(is_excluded(&ex, "bank.com"));
        assert!(is_excluded(&ex, "www.bank.com"));
        assert!(!is_excluded(&ex, "notbank.com"));
        assert!(!is_excluded(&ex, "bank.com.evil.com"));
        assert!(is_excluded(&ex, "WWW.Bank.COM"), "apps may send any letter case");
        assert!(is_excluded(&ex, "bank.com."));
        assert!(!is_excluded(&ex, "ank.com"));

        // Windows Update and Store servers are never decrypted
        for host in ["fe3cr.delivery.mp.microsoft.com", "tlu.dl.delivery.mp.microsoft.com", "sls.update.microsoft.com", "download.windowsupdate.com", "login.live.com"] {
            assert!(is_excluded(&sysproxy::SYSTEM_HOSTS, host), "{host}");
        }
        assert!(!is_excluded(&sysproxy::SYSTEM_HOSTS, "www.microsoft.com"));
    }
}
