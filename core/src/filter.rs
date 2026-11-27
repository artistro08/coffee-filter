//! The hudsucker request handler: what's decrypted, blocked, answered, or changed on the way back.

use crate::{
    apps, ca,
    config::ExcludedApp,
    cosmetic,
    log::{Log, Trace, without_query},
    net::{is_private_host, is_private_ip, on_domain_list},
    problems, prune, sysproxy, userscripts,
};
use adblock::{Engine, request::Request as AdRequest};
use hudsucker::{
    Body, HttpContext, HttpHandler, RequestOrResponse,
    rustls::server::ClientHello,
    hyper::{Method, Request, Response, StatusCode},
};
use std::{
    net::SocketAddr,
    sync::{Arc, RwLock, atomic::{AtomicU64, Ordering}},
    time::Duration,
};

/// State shared by every proxied connection.
pub struct Shared {
    pub engine:       RwLock<Arc<Engine>>,
    /// The lists' `$jsonprune` rules, swapped with the engine.
    pub prune:        RwLock<Arc<prune::Rules>>,
    /// The PAC script and the random path it's served on for this run (see sysproxy).
    pub pac:          RwLock<Pac>,
    /// Same-site path the page script asks for generic rules on, random per run.
    pub generic_path: String,
    pub exclusions:   RwLock<Vec<String>>,
    pub userscripts:  RwLock<Arc<Vec<userscripts::Script>>>,
    pub blocked:      AtomicU64,
    pub allowed:      AtomicU64,
    pub log:          Log,
    /// Detailed log (debug.log), on only while turned on in the app.
    pub trace:        Trace,
    /// Resolves short links (see SHORTENERS).
    pub resolver:     reqwest::Client,
    pub apps:         RwLock<Arc<Vec<ExcludedApp>>>,
    /// Per connection: does it come from an excluded app?
    pub app_cache:    apps::Cache,
    /// Connection problems, and the sites tunneled because an app refused the certificate.
    pub problems:     Arc<problems::Problems>,
    /// Site certificates and TLS sessions, dropped when memory is low.
    pub leaves:       Arc<ca::LeafCache>,
}

#[cfg(test)]
impl Shared {
    /// Empty state for tests: no rules, nothing excluded, logs off.
    pub fn for_tests() -> Self {
        Self {
            engine:       Default::default(),
            prune:        Default::default(),
            pac:          Default::default(),
            generic_path: "/.coffee-filter-test/generic".into(),
            exclusions:   Default::default(),
            userscripts:  Default::default(),
            blocked:      Default::default(),
            allowed:      Default::default(),
            log:          Default::default(),
            trace:        Default::default(),
            resolver:     reqwest::Client::new(),
            apps:         Default::default(),
            app_cache:    Default::default(),
            problems:     Default::default(),
            leaves:       ca::LeafCache::new(),
        }
    }
}

/// The PAC script Windows is pointed at, and the random path it's served on.
#[derive(Default)]
pub struct Pac {
    pub path:   String,
    pub script: String,
}

// =========================================================================
// REQUESTS
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

/// Drop "send it only if it changed" headers, so the site answers with the whole page.
fn remove_conditionals(headers: &mut hudsucker::hyper::HeaderMap) {
    for name in ["if-none-match", "if-modified-since"] {
        headers.remove(name);
    }
}

/// The address as a browser shows it: HTTP/1.1 requests through a tunnel carry `:443`, HTTP/2
/// ones don't, so the default port is dropped and rules and userscripts see one form.
fn page_url(uri: &hudsucker::hyper::Uri) -> String {
    let url     = uri.to_string();
    let default = match uri.scheme_str() {
        Some("https") => 443,
        Some("http") => 80,
        _ => return url,
    };
    match uri.host() {
        Some(host) if uri.port_u16() == Some(default) => url.replacen(&format!("{host}:{default}"), host, 1),
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

// =========================================================================
// BLOCKED PAGE
// =========================================================================

/// A blank in the blocked page, filled per page.
#[derive(Clone, Copy)]
enum Blank {
    AccentLight,
    AccentDark,
    Exclude,
    Reason,
    Url,
}

/// What a blocked page shows, instead of a blank tab (Windows 11 styled, light and dark), with
/// the app icon built in so the page needs nothing from the network. Cut once at its blanks,
/// so each page is put together in one pass instead of copying the whole page per blank.
static BLOCKED_PAGE: std::sync::LazyLock<Vec<(String, Option<Blank>)>> = std::sync::LazyLock::new(|| {
    use base64::{Engine as _, prelude::BASE64_STANDARD};
    const BLANKS: [(&str, Blank); 5] = [
        ("__CF_ACCENT_LIGHT__", Blank::AccentLight),
        ("__CF_ACCENT_DARK__", Blank::AccentDark),
        ("__CF_EXCLUDE__", Blank::Exclude),
        ("__CF_REASON__", Blank::Reason),
        ("__CF_URL__", Blank::Url),
    ];
    let icon = BASE64_STANDARD.encode(include_bytes!("../../app/Assets/Square150x150Logo.png"));
    let page = include_str!("blocked.html").replace("__CF_ICON__", &icon);

    let mut parts = vec![];
    let mut rest  = page.as_str();
    while let Some((at, name, blank)) = BLANKS.iter().filter_map(|(name, blank)| rest.find(name).map(|at| (at, *name, *blank))).min_by_key(|(at, ..)| *at) {
        parts.push((rest[..at].to_string(), Some(blank)));
        rest = &rest[at + name.len()..];
    }
    parts.push((rest.to_string(), None));
    parts
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
    let exclude       = userscripts::escape_html(&format!("coffee-filter://exclude?host={}", userscripts::encode_component(host)));
    let reason        = userscripts::escape_html(reason);
    let url           = userscripts::escape_html(url);
    let (light, dark) = accent_shades();

    // One pass over the cut page, so text inside a filled blank is never taken for another
    let mut page = String::with_capacity(BLOCKED_PAGE.iter().map(|(text, _)| text.len()).sum::<usize>() + url.len() + 512);
    for (text, blank) in BLOCKED_PAGE.iter() {
        page.push_str(text);
        page.push_str(match blank {
            Some(Blank::AccentLight) => &light,
            Some(Blank::AccentDark) => &dark,
            Some(Blank::Exclude) => &exclude,
            Some(Blank::Reason) => &reason,
            Some(Blank::Url) => &url,
            None => "",
        });
    }
    page
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

// =========================================================================
// HANDLER
// =========================================================================

/// What a request's response gets on its way back.
#[derive(Clone, Default)]
enum Pending {
    /// Passed on as it came.
    #[default]
    Nothing,
    /// Cosmetic filtering, if it's an HTML page: its address, and whether it was a page load
    /// or frame (a fetch only counts when the body is a whole page).
    Page { url: String, navigation: bool },
    /// A userscript opened in the browser, shown as an install page.
    Install(String),
}

/// The handler. hudsucker clones it for each request; `pending` and `prune` carry one
/// request's state over to its own response.
#[derive(Clone)]
pub struct Blocker {
    shared:  Arc<Shared>,
    pending: Pending,
    /// `$jsonprune` rules for the response, when its request matched any (see prune.rs).
    prune:   Option<Arc<prune::Matched>>,
}

impl Blocker {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self { shared, pending: Pending::Nothing, prune: None }
    }

    /// Where a public site name (not excluded) points. One OS lookup, usually from its cache.
    async fn resolve_public(&self, host: &str, port: u16) -> Resolved {
        if is_private_host(host) || on_domain_list(&self.shared.exclusions.read().unwrap(), host) {
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

    /// Why `host` is only tunneled, never decrypted (for the detailed log), or None when it's
    /// decrypted: private hosts, Windows' update and Store servers, Zen's never-decrypted sites,
    /// sites an app refused the certificate for, and your exclusions.
    fn tunnel_reason(&self, host: &str) -> Option<&'static str> {
        if is_private_host(host) {
            Some("private address")
        } else if on_domain_list(&sysproxy::SYSTEM_HOSTS, host) {
            Some("Windows Update or Store")
        } else if on_domain_list(sysproxy::never_decrypted(), host) {
            Some("on Zen's never-decrypted list")
        } else if self.shared.problems.passes_through(host) {
            Some("an app refused Coffee Filter's certificate for it")
        } else if on_domain_list(&self.shared.exclusions.read().unwrap(), host) {
            Some("excluded site")
        } else {
            None
        }
    }

    /// A tunnel request. Tunnels that won't be decrypted connect on their own, past our DNS
    /// check (PublicOnly). Check here that a public name doesn't point at a local address, then
    /// connect to the address that was checked: looking the name up again could give a
    /// different, local one (a name can change what it points to between lookups). Sites
    /// tunneled because an app refused our certificate are checked too: that list is learned at
    /// run time, not fixed.
    async fn connect(&self, ctx: &HttpContext, mut req: Request<Body>) -> RequestOrResponse {
        let port = req.uri().port_u16().unwrap_or(443);
        if let Some(host) = req.uri().host().map(str::to_string)
            && (port != 443 || self.shared.problems.passes_through(&host))
            && !self.is_excluded_app(ctx).await
        {
            match self.resolve_public(&host, port).await {
                Resolved::Local => return local_refused(),
                // On 443 the name stays: the decision to tunnel is made by name
                Resolved::Public(addr) if port != 443 => {
                    if let Ok(uri) = addr.to_string().parse() {
                        *req.uri_mut() = uri;
                    }
                }
                _ => {}
            }
        }
        req.into()
    }

    /// The PAC script, which Windows fetches straight from this port (not as a proxy request).
    fn serve_pac(&self, req: &Request<Body>) -> RequestOrResponse {
        let pac      = self.shared.pac.read().unwrap();
        let response = if req.method() == Method::GET && req.uri().path() == pac.path {
            Response::builder()
                .header("content-type", "application/x-ns-proxy-autoconfig")
                .header("cache-control", "no-store")
                .body(Body::from(pac.script.clone()))
        } else {
            Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty())
        };
        response.expect("static response").into()
    }

    /// The page script asking which generic rules match its class names and ids (generic.js).
    /// Answered here and never sent on to the site; all it ever returns is selectors.
    async fn generic_rules(&self, host: &str, req: Request<Body>) -> Response<Body> {
        use http_body_util::BodyExt;
        const MAX_BODY: usize = 256 * 1024;
        let body      = http_body_util::Limited::new(req.into_body(), MAX_BODY).collect().await.map(|b| b.to_bytes()).unwrap_or_default();
        let engine    = self.shared.engine.read().unwrap().clone();
        let selectors = cosmetic::generic_selectors(&engine, host, &body);
        Response::builder()
            .header("content-type", "application/json")
            .header("cache-control", "no-store")
            .header("x-content-type-options", "nosniff")
            .body(Body::from(serde_json::to_string(&selectors).unwrap_or_else(|_| "[]".into())))
            .expect("static response")
    }

    /// An allowed request: note what its response gets on the way back.
    fn allow(&mut self, req: &mut Request<Body>, url: String, kind: &'static str) {
        // Anything that may come back as a page gets cosmetic filtering on the way back (only
        // HTML with a <head> is changed): page loads and frames, and fetches, like a site's
        // service worker saving a page that a normal reload then shows
        let navigation = matches!(kind, "document" | "subdocument");
        let get        = req.method() == Method::GET;
        let install    = get && kind == "document" && userscripts::is_install_link(req.uri());
        let page       = !install && get && (navigation || matches!(kind, "xmlhttprequest" | "other"));

        // A page load always gets the whole page, never "not modified", so the browser can't
        // show a copy it saved before Coffee Filter was on (only Ctrl+Shift+R fixed that)
        if navigation {
            remove_conditionals(req.headers_mut());
        }
        self.shared.trace.write(format_args!("{} {} allowed dest={kind}{}", req.method(), without_query(&url), if page { " page" } else { "" }));
        self.shared.allowed.fetch_add(1, Ordering::Relaxed);

        self.pending = match (install, page) {
            (true, _) => Pending::Install(url),
            (_, true) => Pending::Page { url, navigation },
            _ => Pending::Nothing,
        };
    }

    /// A blocked request: the blocked page for page loads, an empty 403 for everything else.
    fn blocked(&self, req: &Request<Body>, url: &str, host: &str, kind: &'static str, source: &str) -> RequestOrResponse {
        self.shared.blocked.fetch_add(1, Ordering::Relaxed);
        self.shared.trace.write(format_args!("{} {} blocked dest={kind}", req.method(), without_query(url)));
        self.shared.log.record(url, kind, source);
        let response = Response::builder().status(StatusCode::FORBIDDEN);
        let response = if kind == "document" {
            response.header("content-type", "text/html; charset=utf-8").body(Body::from(blocked_page(url, host, LISTED)))
        } else {
            response.body(Body::empty())
        };
        response.expect("static response").into()
    }

    /// A request to a site that's filtered: answered here (generic rules, short links,
    /// blocked), or sent on with a note of what its response gets.
    async fn filter_request(&mut self, mut req: Request<Body>) -> RequestOrResponse {
        let host = req.uri().host().unwrap_or("").to_string();

        // The page script asking for generic rules; answered here, never sent on to the site
        if req.method() == Method::POST && req.uri().path() == self.shared.generic_path {
            return self.generic_rules(&host, req).await.into();
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

        if block {
            return self.blocked(&req, &url, &host, kind, source);
        }

        // JSON whose ad entries a list removes (any method: X loads its timeline with POST)
        let rules  = self.shared.prune.read().unwrap().clone();
        self.prune = prune::Matched::find(&rules, &url, source, kind).map(Arc::new);

        self.allow(&mut req, url, kind);
        req.into()
    }

    /// A response on its way back: pruned if it's JSON a rule is for, otherwise changed as its
    /// request said (cosmetic filtering, userscript install page) or passed on.
    async fn respond(&mut self, res: Response<Body>) -> Response<Body> {
        if let Some(matched) = self.prune.take()
            && prune::is_json(&res)
        {
            return prune::prune_response(res, &matched, &self.shared.trace).await;
        }

        let (url, navigation) = match std::mem::take(&mut self.pending) {
            Pending::Install(url) => return userscripts::install_page(res, &url).await,
            Pending::Page { url, navigation } => (url, navigation),
            Pending::Nothing => {
                // A page that arrives without its request marked as one gets no filtering; worth seeing
                if self.shared.trace.is_on() && res.headers().get("content-type").is_some_and(|t| t.as_bytes().starts_with(b"text/html")) {
                    self.shared.trace.write(format_args!("html response {} not filtered: its request wasn't a page load", res.status()));
                }
                return res;
            }
        };
        let engine  = self.shared.engine.read().unwrap().clone();
        let scripts = self.shared.userscripts.read().unwrap().clone();
        let page    = cosmetic::Page { url: &url, navigation, endpoint: Some(&self.shared.generic_path) };
        // Userscripts are matched only once the response turns out to be a page
        cosmetic::inject(res, &engine, || userscripts::for_page(&scripts, &url), &page, &self.shared.trace)
    }
}

impl HttpHandler for Blocker {
    async fn handle_request(&mut self, ctx: &HttpContext, mut req: Request<Body>) -> RequestOrResponse {
        // hudsucker clones the handler per request; reset anyway so state can never leak across
        self.pending = Pending::Nothing;
        self.prune   = None;
        if req.method() == Method::CONNECT {
            return self.connect(ctx, req).await;
        }
        if req.uri().authority().is_none() {
            return self.serve_pac(&req);
        }

        // We can't pass on compressed WebSocket frames (excluded or not), so don't let the server agree to them
        let host      = req.uri().host().unwrap_or("").to_string();
        let websocket = req.headers().get("upgrade").is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
        if websocket {
            req.headers_mut().remove("sec-websocket-extensions");
        }

        // What's never decrypted, and excluded apps, are left alone over plain HTTP too: no
        // blocking, no page changes
        let untouched = match self.tunnel_reason(&host) {
            None if self.is_excluded_app(ctx).await => Some("excluded app"),
            why => why,
        };
        if let Some(why) = untouched {
            if self.shared.trace.is_on() {
                self.shared.trace.write(format_args!("{} {} untouched ({why})", req.method(), without_query(&req.uri().to_string())));
            }
            return req.into();
        }

        // WebSockets connect on their own too, past PublicOnly
        if websocket && self.points_local(&host, req.uri().port_u16().unwrap_or(80)).await {
            return local_refused();
        }
        self.filter_request(req).await
    }

    async fn handle_response(&mut self, _ctx: &HttpContext, res: Response<Body>) -> Response<Body> {
        self.respond(res).await
    }

    /// The site couldn't be reached (or broke off); the browser gets a 502 either way, and the
    /// detailed log gets the reason.
    async fn handle_error(&mut self, _ctx: &HttpContext, err: hyper_util::client::legacy::Error) -> Response<Body> {
        if self.shared.trace.is_on() {
            let mut why = err.to_string();
            let mut source = std::error::Error::source(&err);
            while let Some(inner) = source {
                why.push_str(": ");
                why.push_str(&inner.to_string());
                source = inner.source();
            }
            let url = match &self.pending {
                Pending::Page { url, .. } | Pending::Install(url) => without_query(url),
                Pending::Nothing => "",
            };
            self.shared.trace.write(format_args!("upstream error {url}: {why}"));
        }
        Response::builder().status(StatusCode::BAD_GATEWAY).body(Body::empty()).expect("static response")
    }

    /// Decrypt only HTTPS on 443 for hosts not excluded; everything else is tunneled untouched.
    async fn should_intercept_connect(&mut self, ctx: &HttpContext, req: &Request<Body>) -> bool {
        let Some(host) = req.uri().host() else { return false };
        let tunnel = if req.uri().port_u16() != Some(443) {
            Some("not port 443")
        } else if let Some(why) = self.tunnel_reason(host) {
            Some(why)
        } else if self.is_excluded_app(ctx).await {
            Some("excluded app")
        } else {
            None
        };
        if self.shared.trace.is_on() {
            let what = tunnel.map_or("decrypted".into(), |why| format!("tunneled ({why})"));
            self.shared.trace.write(format_args!("connect {} from {} {what}", req.uri(), ctx.client_addr));
        }
        tunnel.is_none()
    }

    /// Check again with the site name inside the TLS handshake, which can differ from the
    /// CONNECT address; a certificate is only ever made for a name that passes.
    async fn should_intercept_tls(&mut self, _ctx: &HttpContext, hello: ClientHello<'_>) -> bool {
        let Some(host) = hello.server_name() else { return false };
        let Some(why) = self.tunnel_reason(host) else { return true };
        self.shared.trace.write(format_args!("tls name {host} tunneled ({why})"));
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adblock::lists::{FilterSet, ParseOptions};

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
    fn page_loads_always_ask_for_the_whole_page() {
        let mut headers = hudsucker::hyper::HeaderMap::new();
        headers.insert("if-none-match", "\"abc\"".parse().unwrap());
        headers.insert("if-modified-since", "Sat, 17 Oct 2026 10:00:00 GMT".parse().unwrap());
        headers.insert("accept", "text/html".parse().unwrap());
        remove_conditionals(&mut headers);
        assert!(!headers.contains_key("if-none-match") && !headers.contains_key("if-modified-since"));
        assert!(headers.contains_key("accept"), "other headers stay");
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
        assert_eq!(BLOCKED_PAGE.iter().filter(|(_, blank)| blank.is_some()).count(), 5, "the page is cut once at each of its blanks");
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

    /// A blocker with X's `$jsonprune` rules, and a timeline request from x.com sent as X sends it.
    fn x_blocker() -> Blocker {
        let shared = Shared::for_tests();
        *shared.prune.write().unwrap() = Arc::new(prune::Rules::parse(prune::tests::X_RULES));
        Blocker::new(Arc::new(shared))
    }

    fn timeline_request(path: &str) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri(format!("https://x.com/i/api/graphql/abc123/{path}?variables=%7B%7D"))
            .header("referer", "https://x.com/home")
            .header("sec-fetch-dest", "empty")
            .body(Body::empty())
            .unwrap()
    }

    fn json_response(body: &[u8]) -> Response<Body> {
        Response::builder().header("content-type", "application/json").body(Body::from(body.to_vec())).unwrap()
    }

    #[tokio::test]
    async fn matching_post_responses_are_pruned_and_others_pass_byte_for_byte() {
        use http_body_util::BodyExt;
        let timeline = serde_json::to_vec(&prune::tests::home_timeline()).unwrap();

        // HomeTimeline, sent with POST: the promoted post is gone
        let mut blocker = x_blocker();
        assert!(matches!(blocker.filter_request(timeline_request("HomeTimeline")).await, RequestOrResponse::Request(_)));
        let pruned = blocker.respond(json_response(&timeline)).await.into_body().collect().await.unwrap().to_bytes();
        assert!(!String::from_utf8_lossy(&pruned).contains("promoted-tweet-") && String::from_utf8_lossy(&pruned).contains("tweet-1871"));

        // Another endpoint with the same JSON: exactly as it came
        let mut blocker = x_blocker();
        let _ = blocker.filter_request(timeline_request("UserTweets")).await;
        assert!(blocker.prune.is_none(), "nothing matched, nothing to read");
        let same = blocker.respond(json_response(&timeline)).await.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(same, timeline.as_slice());
    }

    #[test]
    fn urls_drop_the_default_port() {
        let url = |u: &str| page_url(&u.parse().unwrap());
        assert_eq!(url("https://www.facebook.com:443/home?x=1"), "https://www.facebook.com/home?x=1");
        assert_eq!(url("http://example.com:80/a"), "http://example.com/a");
        assert_eq!(url("https://example.com:8443/a"), "https://example.com:8443/a");
        assert_eq!(url("https://example.com/a"), "https://example.com/a");
        assert_eq!(url("http://example.com:443/a"), "http://example.com:443/a", "only the scheme's own default port");
    }
}
