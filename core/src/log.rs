//! The app's live log of blocked requests, and the detailed log (debug.log).

use crate::config::data_dir;
use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

// =========================================================================
// LIVE LOG
// =========================================================================

/// Recently blocked requests for the app's live log. Recording is off (one atomic read per
/// blocked request) unless the log is open in the app.
#[derive(Default)]
pub struct Log {
    on:     AtomicBool,
    recent: Mutex<Recent>,
}

#[derive(Default)]
struct Recent {
    /// Last id handed out.
    last_id: u64,
    /// The newest entries, oldest first.
    entries: VecDeque<LogEntry>,
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

    pub fn record(&self, url: &str, kind: &'static str, source: &str) {
        if !self.on.load(Ordering::Relaxed) {
            return;
        }
        let time  = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let page  = source.parse::<hudsucker::hyper::Uri>().ok().and_then(|u| u.host().map(str::to_string));
        let url   = url.chars().take(500).collect();
        let mut recent = self.recent.lock().unwrap();
        recent.last_id += 1;
        let id = recent.last_id;
        if recent.entries.len() >= Self::MAX {
            recent.entries.pop_front();
        }
        recent.entries.push_back(LogEntry { id, time, url, kind, page });
    }

    /// Entries newer than `after`, oldest first.
    /// An `after` from a previous core run (larger than any id here) means "everything".
    pub fn since(&self, after: u64) -> Vec<LogEntry> {
        let recent = self.recent.lock().unwrap();
        let after  = if after > recent.last_id { 0 } else { after };
        recent.entries.iter().filter(|e| e.id > after).cloned().collect()
    }

    /// Forget what's recorded but keep recording (memory is low).
    pub fn trim(&self) {
        self.recent.lock().unwrap().entries.clear();
    }

    /// Start recording, or stop and forget everything recorded.
    pub fn set(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
        if !on {
            self.recent.lock().unwrap().entries.clear();
        }
    }
}

// =========================================================================
// DETAILED LOG
// =========================================================================

/// Detailed log for tracking down problems: every connection, request, and page, one line
/// each, in `debug.log`. Off costs one atomic read per event. Addresses are written without
/// their query string, which can hold tokens. Past `MAX_BYTES` it starts over, keeping the
/// previous file as `debug.old.log`.
#[derive(Default)]
pub struct Trace {
    on:   AtomicBool,
    /// The open file, from the first line on.
    file: Mutex<Option<TraceFile>>,
    /// Folder for the file; the data folder unless a test sets one.
    dir:  Option<PathBuf>,
}

struct TraceFile {
    dir:  PathBuf,
    file: fs::File,
    /// Bytes in the file, counted as lines are written instead of asking the disk each time.
    size: u64,
}

impl Trace {
    const MAX_BYTES: u64 = 10 * 1024 * 1024;

    pub fn set(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
        if !on {
            *self.file.lock().unwrap() = None;
        }
    }

    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// Write one line; `format_args!` keeps this free when the log is off.
    pub fn write(&self, line: std::fmt::Arguments) {
        if !self.is_on() {
            return;
        }
        use std::io::Write;
        let mut open = self.file.lock().unwrap();

        // Start over once it's big, keeping one previous file
        if open.as_ref().is_some_and(|t| t.size > Self::MAX_BYTES)
            && let Some(TraceFile { dir, .. }) = open.take()
        {
            let _ = fs::rename(dir.join("debug.log"), dir.join("debug.old.log"));
        }
        if open.is_none() {
            let Some(dir) = self.dir.clone().or_else(|| data_dir().ok()) else { return };
            let Ok(file)  = fs::OpenOptions::new().create(true).append(true).open(dir.join("debug.log")) else { return };
            let size      = file.metadata().map_or(0, |m| m.len());
            *open = Some(TraceFile { dir, file, size });
        }
        if let Some(trace) = open.as_mut() {
            let line = format!("{} {line}\n", time::OffsetDateTime::now_utc());
            if trace.file.write_all(line.as_bytes()).is_ok() {
                trace.size += line.len() as u64;
            }
        }
    }
}

/// An error with every cause under it, `outer: inner: ...`.
pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text   = error.to_string();
    let mut source = error.source();
    while let Some(inner) = source {
        text.push_str(": ");
        text.push_str(&inner.to_string());
        source = inner.source();
    }
    text
}

/// An address without its query string or fragment, for the detailed log.
pub fn without_query(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_writes_only_while_on_and_drops_queries() {
        let dir   = std::env::temp_dir().join(format!("cf-trace-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let trace = Trace { dir: Some(dir.clone()), ..Default::default() };
        let log   = dir.join("debug.log");

        trace.write(format_args!("before"));
        assert!(!log.exists(), "nothing is written while it's off");

        trace.set(true);
        trace.write(format_args!("GET {} allowed", without_query("https://a.com/p?token=secret#top")));
        trace.set(false);
        trace.write(format_args!("after"));

        let text = fs::read_to_string(&log).unwrap();
        assert!(text.contains("GET https://a.com/p allowed"), "{text}");
        assert!(!text.contains("secret") && !text.contains("before") && !text.contains("after"), "{text}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn trace_starts_over_past_its_limit() {
        let dir   = std::env::temp_dir().join(format!("cf-trace-roll-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let log   = dir.join("debug.log");
        let trace = Trace { dir: Some(dir.clone()), ..Default::default() };

        // A log already past the limit (sparse, so the test writes almost nothing)
        fs::File::create(&log).unwrap().set_len(Trace::MAX_BYTES + 1).unwrap();
        trace.set(true);
        trace.write(format_args!("first"));
        trace.write(format_args!("second"));
        trace.set(false);

        let new = fs::read_to_string(&log).unwrap();
        assert!(new.contains("second") && !new.contains("first"), "{new}");
        assert!(fs::metadata(dir.join("debug.old.log")).unwrap().len() > Trace::MAX_BYTES, "the big one is kept as debug.old.log");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn trace_counts_what_it_writes() {
        let dir   = std::env::temp_dir().join(format!("cf-trace-count-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("debug.log"), "earlier\n").unwrap();
        let trace = Trace { dir: Some(dir.clone()), ..Default::default() };

        trace.set(true);
        trace.write(format_args!("one"));
        trace.write(format_args!("two"));
        let counted = trace.file.lock().unwrap().as_ref().unwrap().size;
        assert_eq!(counted, fs::metadata(dir.join("debug.log")).unwrap().len(), "the count matches the file, starting from what was there");
        trace.set(false);
        let _ = fs::remove_dir_all(&dir);
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
}
