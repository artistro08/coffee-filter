//! Connection problem tracker.
//!
//! hudsucker reports failures through `tracing`. This layer picks out the ones Coffee
//! Filter itself can cause and remembers them per host, so the app can show what broke
//! and offer to exclude it:
//!
//! - an app refused Coffee Filter's certificate (apps that pin their own certificate)
//! - an app hung up during the handshake, repeatedly (strict apps that can't check
//!   revocation for Coffee Filter's certificates)
//!
//! Sites are verified with Windows' own check, so a site that fails through Coffee
//! Filter fails in the browser too (expired, DNS block pages, ...). Those, and ordinary
//! network noise, aren't reported.

use serde::Serialize;
use std::{
    cmp::Reverse,
    collections::HashMap,
    fmt::Debug,
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::{
    Event, Id, Level, Metadata, Subscriber,
    field::{Field, Visit},
    span::Attributes,
    subscriber::Interest,
};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

const MAX_HOSTS: usize = 100;

#[derive(Clone, Debug, Serialize)]
pub struct Problem {
    pub host:   String,
    pub kind:   &'static str,
    pub detail: String,
    pub count:  u64,
    /// Unix seconds of the latest occurrence.
    pub last:   u64,
}

#[derive(Default)]
pub struct Problems {
    map:   Mutex<HashMap<String, Problem>>,
    /// Changed since the last save to problems.json.
    dirty: AtomicBool,
    /// Where a host that just became a problem is sent, so the app can notify right away.
    found: OnceLock<UnboundedSender<Problem>>,
    /// Sites an app refused Coffee Filter's certificate for. Like Zen's "transparent hosts",
    /// they're tunneled untouched for every app until Coffee Filter restarts (or Clear). Only
    /// a refusal counts, never a hang-up: a web page can make the browser hang up on purpose
    /// (canceled requests), and must not be able to switch filtering off for a site. Each is
    /// also in `map`, so it's dropped with it and stays as bounded.
    transparent: RwLock<Vec<String>>,
}

/// Hang-ups only count once they repeat; browsers drop the odd connection on their own.
fn shown(p: &Problem) -> bool {
    p.kind != "app_hung_up" || p.count >= 3
}

impl Problems {
    /// Hosts that newly show up in the list, from now on.
    pub fn subscribe(&self) -> UnboundedReceiver<Problem> {
        let (tx, rx) = unbounded_channel();
        let _ = self.found.set(tx);
        rx
    }

    /// Whether an app refused Coffee Filter's certificate for `host` (or a site above it), so
    /// it's tunneled instead of decrypted.
    pub fn passes_through(&self, host: &str) -> bool {
        crate::net::on_domain_list(&self.transparent.read().unwrap(), host)
    }

    pub fn record(&self, host: &str, kind: &'static str, detail: String) {
        let now     = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let mut map = self.map.lock().unwrap();

        // Keep memory bounded: drop the oldest host when full
        if map.len() >= MAX_HOSTS
            && !map.contains_key(host)
            && let Some(oldest) = map.values().min_by_key(|p| p.last).map(|p| p.host.clone())
        {
            map.remove(&oldest);
            self.transparent.write().unwrap().retain(|h| *h != oldest);
        }
        let entry = map.entry(host.to_string()).or_insert_with(|| Problem { host: host.to_string(), kind, detail: String::new(), count: 0, last: 0 });
        let was   = entry.count > 0 && shown(entry);
        entry.kind   = kind;
        entry.detail = detail;
        entry.count += 1;
        entry.last   = now;
        self.dirty.store(true, Ordering::Relaxed);

        // Apps get through to this site untouched from their next connection on
        if kind == "app_rejected_certificate" {
            let mut transparent = self.transparent.write().unwrap();
            if !transparent.iter().any(|h| h == host) {
                transparent.push(host.to_string());
            }
        }

        // Only the first time a host shows up, not on every repeat
        if !was && shown(entry)
            && let Some(found) = self.found.get()
        {
            let _ = found.send(entry.clone());
        }
    }

    /// Keep a copy on disk for troubleshooting. Called from the command loop, not from the
    /// proxy's threads, and only writes when something changed.
    pub fn save_if_changed(&self) {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        if let (Ok(dir), Ok(json)) = (crate::config::data_dir(), serde_json::to_string_pretty(&self.list())) {
            let _ = crate::config::write_atomic(&dir.join("problems.json"), json.as_bytes());
        }
    }

    /// Most recent first, only the ones worth showing.
    pub fn list(&self) -> Vec<Problem> {
        let mut list: Vec<Problem> = self.map.lock().unwrap().values().filter(|p| shown(p)).cloned().collect();
        list.sort_by_key(|p| Reverse(p.last));
        list
    }

    /// Forget the problems, and decrypt their sites again (they're tunneled again if an app
    /// still refuses).
    pub fn clear(&self) {
        self.map.lock().unwrap().clear();
        self.transparent.write().unwrap().clear();
        self.dirty.store(true, Ordering::Relaxed);
    }
}

/// Decide whether a hudsucker error is one Coffee Filter caused, and explain it.
pub fn classify(message: &str, error: &str) -> Option<(&'static str, String)> {
    let lower = error.to_ascii_lowercase();
    // Only alerts about the certificate itself; an old client that can't speak TLS 1.2
    // (ProtocolVersion) or a protocol mismatch says nothing about our certificate
    const CERTIFICATE_ALERTS: [&str; 7] =
        ["unknownca", "badcertificate", "certificateunknown", "certificateexpired", "certificaterevoked", "unsupportedcertificate", "accessdenied"];
    if message.contains("Failed to establish TLS connection") && lower.contains("alert") && CERTIFICATE_ALERTS.iter().any(|a| lower.contains(a)) {
        return Some(("app_rejected_certificate", "An app doesn't accept Coffee Filter's certificate (it checks for its own), so this site now connects without filtering until Coffee Filter restarts.".into()));
    }
    if message.contains("Failed to establish TLS connection") && lower.contains("handshake eof") {
        return Some(("app_hung_up", "An app keeps closing the connection when it sees Coffee Filter's certificate. Exclude it if something isn't working.".into()));
    }
    None
}

// =========================================================================
// TRACING LAYER
// =========================================================================

pub struct ProblemLayer(pub Arc<Problems>);

/// The `uri` field hudsucker puts on each request span.
struct SpanUri(String);

#[derive(Default)]
struct Fields {
    uri:     Option<String>,
    message: String,
    error:   String,
}

impl Visit for Fields {
    /// Plain strings are taken as they are: formatting them for debugging would add quotes
    /// and escapes that would then have to come off again.
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "uri" => self.uri = Some(value.to_string()),
            "message" => self.message = value.to_string(),
            "error" => self.error = value.to_string(),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        // Only format the fields we read; spans carry several others per request
        match field.name() {
            "uri" => self.uri = Some(format!("{value:?}")),
            "message" => self.message = format!("{value:?}"),
            "error" => self.error = format!("{value:?}"),
            _ => {}
        }
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        // Include the whole cause chain; TLS alerts are usually a few levels down
        if field.name() == "error" {
            self.error = crate::log::error_chain(value);
        }
    }
}

/// hudsucker's per-request span (it carries the `uri` field) and the span its spawned
/// TLS handshake runs in (a child of the request span, so the host can be found from it).
const SPANS: [&str; 2] = ["proxy", "process_connect"];

fn wanted(metadata: &Metadata<'_>) -> bool {
    if metadata.is_span() { SPANS.contains(&metadata.name()) } else { *metadata.level() <= Level::WARN }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for ProblemLayer {
    /// Decided once per log statement: everything except request spans and warnings is
    /// switched off at the source, so routine logging costs nothing per request.
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        if wanted(metadata) { Interest::always() } else { Interest::never() }
    }

    fn enabled(&self, metadata: &Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        wanted(metadata)
    }

    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        if let (Some(uri), Some(span)) = (fields.uri, ctx.span(id)) {
            span.extensions_mut().insert(SpanUri(uri));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let Some((kind, detail)) = classify(&fields.message, &fields.error) else { return };
        let uri = ctx.event_scope(event).and_then(|scope| scope.from_root().find_map(|span| span.extensions().get::<SpanUri>().map(|u| u.0.clone())));
        if let Some(uri) = uri {
            self.0.record(&crate::net::host_of(&uri).to_ascii_lowercase(), kind, detail);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_only_problems_we_cause() {
        assert_eq!(classify("Failed to establish TLS connection", "received fatal alert: UnknownCA").unwrap().0, "app_rejected_certificate");
        assert_eq!(classify("Failed to establish TLS connection", "received fatal alert: BadCertificate").unwrap().0, "app_rejected_certificate");
        assert!(classify("Failed to establish TLS connection", "received fatal alert: ProtocolVersion").is_none(), "an old client, not our certificate");
        assert!(classify("Failed to establish TLS connection", "received fatal alert: NoApplicationProtocol").is_none());
        assert_eq!(classify("Failed to establish TLS connection", "tls handshake eof").unwrap().0, "app_hung_up");
        assert!(classify("Failed to forward request", "invalid peer certificate: UnknownIssuer").is_none(), "fails in the browser too");
        assert!(classify("Failed to establish TLS connection", "connection reset by peer").is_none(), "closed tab, not a rejection");
        assert!(classify("Failed to forward request", "dns error: no such host").is_none(), "site down is not ours");
    }

    #[test]
    fn records_count_and_caps_hosts() {
        let p = Problems::default();
        p.record("a.com", "app_rejected_certificate", "x".into());
        p.record("a.com", "app_rejected_certificate", "x".into());
        assert_eq!(p.list()[0].count, 2);
        p.record("b.com", "app_hung_up", "x".into());
        assert!(p.list().iter().all(|x| x.host != "b.com"), "single hang-up hidden");
        for i in 0..150 {
            p.record(&format!("h{i}.com"), "app_rejected_certificate", "x".into());
        }
        assert!(p.list().len() <= MAX_HOSTS);
    }

    #[test]
    fn a_refused_certificate_tunnels_that_site() {
        let problems = Problems::default();
        assert!(!problems.passes_through("discovery.elgato.com"));

        // Refusing the certificate counts right away, for the site and below it
        problems.record("Discovery.Elgato.com", "app_rejected_certificate", String::new());
        assert!(problems.passes_through("discovery.elgato.com"));
        assert!(problems.passes_through("eu.discovery.elgato.com"));
        assert!(!problems.passes_through("elgato.com"), "not the sites above it");
        assert!(!problems.passes_through("notdiscovery.elgato.com"));

        // Hanging up never does, however often: a web page can cancel its own requests
        for _ in 0..5 {
            problems.record("ads.example.com", "app_hung_up", String::new());
        }
        assert!(!problems.passes_through("ads.example.com"), "a page must not switch filtering off for a site");

        // Bounded: a site dropped from the problem list is decrypted again (all of these land in
        // the same second, so make the first one the oldest)
        problems.map.lock().unwrap().get_mut("Discovery.Elgato.com").unwrap().last = 0;
        for i in 0..MAX_HOSTS {
            problems.record(&format!("s{i}.example.net"), "app_rejected_certificate", String::new());
        }
        assert!(!problems.passes_through("discovery.elgato.com"), "the oldest was dropped");
        assert!(problems.transparent.read().unwrap().len() <= MAX_HOSTS);

        // Clear decrypts them again
        problems.clear();
        assert!(!problems.passes_through("discovery.elgato.com"));
    }

    #[test]
    fn announces_each_host_once_when_it_shows_up() {
        let p       = Problems::default();
        let mut rx  = p.subscribe();
        p.record("pinned.com", "app_rejected_certificate", "x".into());
        p.record("pinned.com", "app_rejected_certificate", "x".into());
        p.record("strict.com", "app_hung_up", "x".into());
        p.record("strict.com", "app_hung_up", "x".into());
        assert_eq!(rx.try_recv().unwrap().host, "pinned.com");
        assert!(rx.try_recv().is_err(), "repeats and single hang-ups stay quiet");
        p.record("strict.com", "app_hung_up", "x".into());
        assert_eq!(rx.try_recv().unwrap().host, "strict.com");
        p.clear();
        p.record("pinned.com", "app_rejected_certificate", "x".into());
        assert_eq!(rx.try_recv().unwrap().host, "pinned.com", "again after Clear");
    }

    #[test]
    fn layer_attributes_errors_to_the_request_host() {
        use tracing_subscriber::layer::SubscriberExt;
        let problems = Arc::new(Problems::default());
        let subscriber = tracing_subscriber::registry().with(ProblemLayer(problems.clone()));
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("proxy", uri = %"pinned.example.com:443");
            let _in  = span.enter();
            let err  = std::io::Error::other("received fatal alert: BadCertificate");
            tracing::error!(error = &err as &dyn std::error::Error, "Failed to establish TLS connection");
        });
        let list = problems.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].host, "pinned.example.com");
    }

    #[test]
    fn plain_string_fields_are_read_as_they_are() {
        use tracing_subscriber::layer::SubscriberExt;
        let problems   = Arc::new(Problems::default());
        let subscriber = tracing_subscriber::registry().with(ProblemLayer(problems.clone()));
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("proxy", uri = "Quoted.Example.com:443");
            let _in  = span.enter();
            tracing::error!(error = "received fatal alert: \"BadCertificate\"", "Failed to establish TLS connection");
        });
        let list = problems.list();
        assert_eq!(list.len(), 1, "a string error is read without debug escapes in the way");
        assert_eq!(list[0].host, "quoted.example.com");
    }
}
