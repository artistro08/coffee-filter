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
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
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
}

impl Problems {
    pub fn record(&self, host: &str, kind: &'static str, detail: String) {
        let now     = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let mut map = self.map.lock().unwrap();

        // Keep memory bounded: drop the oldest host when full
        if map.len() >= MAX_HOSTS
            && !map.contains_key(host)
            && let Some(oldest) = map.values().min_by_key(|p| p.last).map(|p| p.host.clone())
        {
            map.remove(&oldest);
        }
        let entry = map.entry(host.to_string()).or_insert_with(|| Problem { host: host.to_string(), kind, detail: String::new(), count: 0, last: 0 });
        entry.kind   = kind;
        entry.detail = detail;
        entry.count += 1;
        entry.last   = now;
        self.dirty.store(true, Ordering::Relaxed);
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

    /// Most recent first. Hang-ups only count once they repeat; browsers drop the odd
    /// connection on their own.
    pub fn list(&self) -> Vec<Problem> {
        let mut list: Vec<Problem> = self.map.lock().unwrap().values().filter(|p| p.kind != "app_hung_up" || p.count >= 3).cloned().collect();
        list.sort_by_key(|p| Reverse(p.last));
        list
    }

    pub fn clear(&self) {
        self.map.lock().unwrap().clear();
        self.dirty.store(true, Ordering::Relaxed);
    }
}

/// Decide whether a hudsucker error is one Coffee Filter caused, and explain it.
pub fn classify(message: &str, error: &str) -> Option<(&'static str, String)> {
    let lower = error.to_ascii_lowercase();
    if message.contains("Failed to establish TLS connection") && lower.contains("alert") {
        return Some(("app_rejected_certificate", "This app doesn't accept Coffee Filter's certificate. It may check for its own. Exclude it to let it connect.".into()));
    }
    if message.contains("Failed to establish TLS connection") && lower.contains("handshake eof") {
        return Some(("app_hung_up", "An app keeps closing the connection when it sees Coffee Filter's certificate. Exclude it if something isn't working.".into()));
    }
    None
}

/// Host part of `host:port` or a full URL.
fn host_of(uri: &str) -> String {
    let rest = uri.split_once("://").map_or(uri, |(_, r)| r);
    let auth = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let auth = auth.rsplit_once('@').map_or(auth, |(_, a)| a);
    if let Some(v6) = auth.strip_prefix('[') {
        return v6.split(']').next().unwrap_or(v6).to_string();
    }
    auth.split(':').next().unwrap_or(auth).to_ascii_lowercase()
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
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_debug(field, &value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        // Only format the fields we read; spans carry several others per request
        match field.name() {
            "uri" => self.uri = Some(format!("{value:?}").trim_matches('"').to_string()),
            "message" => self.message = format!("{value:?}"),
            "error" => self.error = format!("{value:?}"),
            _ => {}
        }
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        if field.name() == "error" {
            // Include the whole cause chain; TLS alerts are usually a few levels down
            let mut text = value.to_string();
            let mut src  = value.source();
            while let Some(e) = src {
                text.push_str(": ");
                text.push_str(&e.to_string());
                src = e.source();
            }
            self.error = text;
        }
    }
}

/// hudsucker's per-request span (it carries the `uri` field) and the span its spawned
/// TLS handshake runs in (a child of the request span, so the host can be found from it).
const SPANS: [&str; 2] = ["proxy", "process_connect"];

fn wanted(metadata: &Metadata<'_>) -> bool {
    if metadata.is_span() { SPANS.contains(&metadata.name()) } else { *metadata.level() <= Level::WARN }
}

fn debug_enabled() -> bool {
    static DEBUG: OnceLock<bool> = OnceLock::new();
    *DEBUG.get_or_init(|| std::env::var_os("COFFEE_FILTER_DEBUG").is_some())
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
        let uri = ctx.event_scope(event).and_then(|scope| scope.from_root().find_map(|span| span.extensions().get::<SpanUri>().map(|u| u.0.clone())));

        // COFFEE_FILTER_DEBUG=1 prints every proxy error, for diagnosing what broke
        if debug_enabled() {
            eprintln!("[{}] {:?} {} | {}", event.metadata().level(), uri, fields.message, fields.error);
        }

        let Some((kind, detail)) = classify(&fields.message, &fields.error) else { return };
        if let Some(uri) = uri {
            self.0.record(&host_of(&uri), kind, detail);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_only_problems_we_cause() {
        assert_eq!(classify("Failed to establish TLS connection", "received fatal alert: UnknownCA").unwrap().0, "app_rejected_certificate");
        assert_eq!(classify("Failed to establish TLS connection", "tls handshake eof").unwrap().0, "app_hung_up");
        assert!(classify("Failed to forward request", "invalid peer certificate: UnknownIssuer").is_none(), "fails in the browser too");
        assert!(classify("Failed to establish TLS connection", "connection reset by peer").is_none(), "closed tab, not a rejection");
        assert!(classify("Failed to forward request", "dns error: no such host").is_none(), "site down is not ours");
    }

    #[test]
    fn host_parsing() {
        assert_eq!(host_of("api.example.com:443"), "api.example.com");
        assert_eq!(host_of("https://Example.com:8443/path?q"), "example.com");
        assert_eq!(host_of("[::1]:443"), "::1");
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
}
