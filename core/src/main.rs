//! Coffee Filter core: a local HTTPS ad-blocking proxy.
//!
//! Controlled over stdin/stdout with one JSON object per line, so there is no
//! control port for other programs to poke at. Closing stdin (the GUI exiting
//! or crashing), Windows shutting down, or the user signing out stops the proxy
//! and restores the system proxy settings.

mod ca;
mod config;
mod cosmetic;
mod filter;
mod problems;
mod sysproxy;
mod userscripts;

use config::Config;
use filter::{Blocker, Shared};
use hudsucker::{
    Proxy,
    rustls::{ClientConfig, crypto::aws_lc_rs},
    tokio_tungstenite::Connector,
};
use hyper_util::client::legacy::connect::HttpConnector;
use rustls_platform_verifier::BuilderVerifierExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io,
    sync::{Arc, RwLock, atomic::Ordering},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown},
    sync::oneshot,
    task::{JoinHandle, spawn_blocking},
};
use windows::{
    Win32::{
        Foundation::{ERROR_ALREADY_EXISTS, GetLastError},
        System::Threading::CreateMutexW,
    },
    core::{HSTRING, PCWSTR},
};

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Command {
    Status,
    Start,
    Stop,
    InstallCert,
    UninstallCert,
    RenewCert,
    UpdateLists,
    SetExclusions { hosts: Vec<String> },
    ClearProblems,
    SetList { name: String, enabled: bool },
    AddList { name: String, url: String },
    RemoveList { name: String },
    AddUserscript { url: String },
    SetUserscript { name: String, enabled: bool },
    RemoveUserscript { name: String },
    /// Turn on the live log and get entries newer than `after`.
    Log { after: u64 },
    LogStop,
}

struct App {
    config:         Config,
    shared:         Arc<Shared>,
    problems:       Arc<problems::Problems>,
    rules:          usize,
    running:        Option<(oneshot::Sender<()>, JoinHandle<()>)>,
    /// Cached: checking the trust store on every status reply would cost a lookup every 2s.
    cert_installed: bool,
    cert_renewal:   Option<&'static str>,
    /// Log entries to send with this reply (the `log` command only).
    reply_log:      Option<Value>,
}

/// Lowercase, strip `*.` and dots, and reject anything that isn't a plain hostname.
fn normalize_host(host: &str) -> Option<String> {
    let host  = host.trim().trim_start_matches("*.").trim_matches('.').to_ascii_lowercase();
    let valid = !host.is_empty() && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    valid.then_some(host)
}

/// Validate a user-added list: needs a name, an https address (a plain http list could be
/// rewritten in transit to inject rules into every page), and no duplicate.
fn new_list(lists: &[config::FilterList], name: &str, url: &str) -> Result<config::FilterList, String> {
    let name = name.trim().to_string();
    let url  = url.trim().to_string();
    if name.is_empty() || !url.starts_with("https://") {
        return Err("Enter a name and a web address starting with https://".into());
    }
    if lists.iter().any(|l| l.name == name || l.url == url) {
        return Err("That list is already added.".into());
    }
    Ok(config::FilterList { name, url, enabled: true, kind: "custom".into(), trusted: false })
}

/// Listen on 127.0.0.1 with SO_EXCLUSIVEADDRUSE, so no other program can bind the same
/// port alongside us and answer in our place (Windows otherwise allows that with SO_REUSEADDR).
fn bind_exclusive(port: u16) -> io::Result<TcpListener> {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{SO_EXCLUSIVEADDRUSE, SOCKET, SOL_SOCKET, setsockopt};

    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)?;
    // SAFETY: a valid socket handle owned by `socket` for the whole call, and a 4-byte option value.
    if unsafe { setsockopt(SOCKET(socket.as_raw_socket() as usize), SOL_SOCKET, SO_EXCLUSIVEADDRUSE, Some(&1i32.to_ne_bytes())) } != 0 {
        return Err(io::Error::last_os_error());
    }
    socket.bind(&std::net::SocketAddr::from(([127, 0, 0, 1], port)).into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    TcpListener::from_std(socket.into())
}

/// Run blocking work (UAC prompts, DPAPI, file I/O) off the proxy's worker threads.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> io::Result<T> + Send + 'static) -> Result<T, String> {
    spawn_blocking(work).await.map_err(|e| e.to_string())?.map_err(|e| e.to_string())
}

impl App {
    fn status(&self) -> Value {
        self.problems.save_if_changed();
        json!({
            "running":        self.running.is_some(),
            "cert_installed": self.cert_installed,
            "cert_renewal":   self.cert_renewal,
            "blocked":        self.shared.blocked.load(Ordering::Relaxed),
            "allowed":        self.shared.allowed.load(Ordering::Relaxed),
            "rules":          self.rules,
            "port":           self.config.port,
            "exclusions":     self.config.exclusions,
            "lists":          self.config.lists,
            "userscripts":    self.config.userscripts,
            "problems":       self.problems.list(),
        })
    }

    fn refresh_cert(&mut self) {
        self.cert_installed = ca::is_installed().unwrap_or(false);
        self.cert_renewal   = if self.cert_installed { ca::renewal_reason() } else { None };
    }

    async fn rebuild(&mut self) {
        let (engine, rules) = filter::build_engine(&self.config).await;
        // Swap under the lock, free the old engine after releasing it
        let old = std::mem::replace(&mut *self.shared.engine.write().unwrap(), Arc::new(engine));
        drop(old);
        self.rules = rules;
    }

    async fn reload_userscripts(&mut self) {
        let scripts = userscripts::load(&self.config).await;
        *self.shared.userscripts.write().unwrap() = Arc::new(scripts);
    }

    async fn start(&mut self) -> Result<(), String> {
        if self.running.is_some() {
            return Ok(());
        }
        if !self.cert_installed {
            return Err("Install the certificate first.".into());
        }

        let issuer   = blocking(ca::load_or_create).await?;
        let ca       = ca::LeafAuthority::new(issuer, aws_lc_rs::default_provider()).map_err(|e| e.to_string())?;
        let listener = bind_exclusive(self.config.port).map_err(|e| format!("Port {} unavailable: {e}", self.config.port))?;
        let (tx, rx) = oneshot::channel();

        // Verify sites with Windows' own certificate check, so Coffee Filter accepts and
        // rejects exactly what a browser on this PC would (including work or custom roots)
        let tls = ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
            .with_safe_default_protocol_versions()
            .and_then(|b| b.with_platform_verifier())
            .map_err(|e| e.to_string())?
            .with_no_client_auth();
        // HTTP/2 when the site supports it (one connection, many requests at once). No Nagle
        // delay on small writes. Public names that resolve to this PC or the home network are
        // refused, so a web page can't use the proxy to reach local devices (DNS rebinding).
        let mut http = HttpConnector::new_with_resolver(filter::PublicOnly::default());
        http.enforce_http(false);
        http.set_nodelay(true);
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls.clone())
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(http);

        let proxy = Proxy::builder()
            .with_listener(listener)
            .with_ca(ca)
            .with_http_connector(https)
            .with_websocket_connector(Connector::Rustls(Arc::new(tls)))
            .with_http_handler(Blocker::new(self.shared.clone()))
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .build()
            .map_err(|e| e.to_string())?;

        let handle = tokio::spawn(async move {
            if let Err(e) = proxy.start().await {
                eprintln!("proxy stopped: {e}");
            }
        });

        let pac_path = sysproxy::new_pac_path();
        *self.shared.pac.write().unwrap() = (pac_path.clone(), sysproxy::pac_script(self.config.port));
        if let Err(e) = sysproxy::enable(self.config.port, &pac_path) {
            // Don't leave a proxy running (and the port taken) that Windows isn't using
            let _ = sysproxy::restore();
            let _ = tx.send(());
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
            return Err(format!("Couldn't change the Windows proxy setting: {e}"));
        }
        self.running = Some((tx, handle));
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), String> {
        let restored = sysproxy::restore().map_err(|e| e.to_string());
        if let Some((tx, mut handle)) = self.running.take() {
            let _ = tx.send(());
            // Graceful shutdown waits for open connections; don't let one hang stopping
            if tokio::time::timeout(Duration::from_secs(5), &mut handle).await.is_err() {
                handle.abort();
            }
        }
        restored
    }

    async fn run(&mut self, cmd: Command) -> Result<(), String> {
        match cmd {
            Command::Status => Ok(()),
            Command::Start => self.start().await,
            Command::Stop => self.stop().await,
            Command::InstallCert => {
                let installed = blocking(ca::install).await;
                self.refresh_cert();
                match installed? {
                    true => Ok(()),
                    false => Err("Certificate was not installed.".into()),
                }
            }
            Command::UninstallCert => {
                self.stop().await?;
                let removed = blocking(ca::uninstall).await;
                self.refresh_cert();
                match removed? {
                    true => Ok(()),
                    false => Err("Certificate was not removed.".into()),
                }
            }
            Command::RenewCert => {
                self.stop().await?;
                let renewed = blocking(ca::renew).await;
                self.refresh_cert();
                match renewed? {
                    true => Ok(()),
                    false => Err("Certificate was not renewed.".into()),
                }
            }
            Command::UpdateLists => {
                let (mut errors, script_errors) = tokio::join!(filter::download_lists(&self.config), userscripts::download_all(&self.config.userscripts));
                errors.extend(script_errors);
                self.rebuild().await;
                self.reload_userscripts().await;
                if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) }
            }
            Command::SetExclusions { hosts } => {
                let mut kept: Vec<String> = hosts.iter().filter_map(|h| normalize_host(h)).collect();
                kept.sort();
                kept.dedup();
                let dropped: Vec<&str> = hosts.iter().map(|h| h.trim()).filter(|h| !h.is_empty() && normalize_host(h).is_none()).collect();

                *self.shared.exclusions.write().unwrap() = kept.clone();
                self.config.exclusions = kept;
                self.config.save().map_err(|e| e.to_string())?;
                if dropped.is_empty() {
                    Ok(())
                } else {
                    Err(format!("These aren't site names and weren't saved: {}", dropped.join(", ")))
                }
            }
            Command::ClearProblems => {
                self.problems.clear();
                Ok(())
            }
            Command::SetList { name, enabled } => {
                let list = self.config.lists.iter_mut().find(|l| l.name == name).ok_or("Unknown list.")?;
                list.enabled = enabled;
                self.config.save().map_err(|e| e.to_string())?;
                self.rebuild().await;
                Ok(())
            }
            Command::AddList { name, url } => {
                let list = new_list(&self.config.lists, &name, &url)?;
                self.config.lists.push(list);
                self.config.save().map_err(|e| e.to_string())?;
                // Rebuild downloads lists that aren't cached yet
                self.rebuild().await;
                Ok(())
            }
            Command::RemoveList { name } => {
                self.config.lists.retain(|l| l.name != name);
                self.config.save().map_err(|e| e.to_string())?;
                self.rebuild().await;
                Ok(())
            }
            Command::Log { after } => {
                self.shared.log.set(true);
                self.reply_log = Some(json!(self.shared.log.since(after)));
                Ok(())
            }
            Command::LogStop => {
                self.shared.log.set(false);
                Ok(())
            }
            Command::AddUserscript { url } => {
                // https only: a script runs inside pages, so it must not be swappable on the way
                let url = url.trim().to_string();
                if !url.starts_with("https://") {
                    return Err("Enter a web address starting with https://".into());
                }
                if self.config.userscripts.iter().any(|s| s.url == url) {
                    return Err("That userscript is already added.".into());
                }
                let client = filter::client().map_err(|e| e.to_string())?;
                let header = userscripts::download(&client, &url).await?;
                if self.config.userscripts.iter().any(|s| s.name == header.name) {
                    userscripts::forget(&url);
                    return Err(format!("A userscript named \"{}\" is already added.", header.name));
                }
                self.config.userscripts.push(config::Userscript { name: header.name, url, enabled: true });
                self.config.save().map_err(|e| e.to_string())?;
                self.reload_userscripts().await;
                Ok(())
            }
            Command::SetUserscript { name, enabled } => {
                let script = self.config.userscripts.iter_mut().find(|s| s.name == name).ok_or("Unknown userscript.")?;
                script.enabled = enabled;
                self.config.save().map_err(|e| e.to_string())?;
                self.reload_userscripts().await;
                Ok(())
            }
            Command::RemoveUserscript { name } => {
                if let Some(script) = self.config.userscripts.iter().find(|s| s.name == name) {
                    userscripts::forget(&script.url);
                }
                self.config.userscripts.retain(|s| s.name != name);
                self.config.save().map_err(|e| e.to_string())?;
                self.reload_userscripts().await;
                Ok(())
            }
        }
    }
}

/// Resolves when Windows asks this process to end: Ctrl+C/Break, its console closing,
/// the user signing out, or the PC shutting down. The core runs with a hidden console,
/// so it receives these and can put the proxy settings back first.
async fn windows_ending() {
    let (Ok(mut c), Ok(mut b), Ok(mut close), Ok(mut logoff), Ok(mut shutdown)) = (ctrl_c(), ctrl_break(), ctrl_close(), ctrl_logoff(), ctrl_shutdown()) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = c.recv() => {}
        _ = b.recv() => {}
        _ = close.recv() => {}
        _ = logoff.recv() => {}
        _ = shutdown.recv() => {}
    }
}

// Certificate checks wait on Windows (sometimes on the network), so keep enough workers
// that one slow check never stalls other tabs. Idle workers cost nothing.
#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    // One core per data folder: a second one would restore (switch off) the first one's proxy
    let profile = config::data_dir().map(|d| d.to_string_lossy().to_lowercase()).unwrap_or_default();
    let name    = HSTRING::from(format!("Local\\CoffeeFilterCore-{}", &sha1_smol::Sha1::from(profile).digest().to_string()[..16]));
    // SAFETY: plain named-mutex creation; the handle lives until the process exits.
    let _single = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        println!("{}", json!({ "ok": false, "event": "ready", "error": "Coffee Filter is already running." }));
        return;
    }

    // Undo a system proxy left behind by a crash before doing anything else
    if let Err(e) = sysproxy::restore() {
        eprintln!("restore failed: {e}");
    }

    // Watch hudsucker's error reports for connections Coffee Filter broke
    let problems = Arc::new(problems::Problems::default());
    {
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::registry().with(problems::ProblemLayer(problems.clone()));
        let _ = tracing::subscriber::set_global_default(subscriber);
    }

    let (config, warning) = Config::load();
    let (engine, rules)   = filter::build_engine(&config).await;
    let scripts           = userscripts::load(&config).await;
    let shared = Arc::new(Shared {
        engine:      RwLock::new(Arc::new(engine)),
        pac:         RwLock::new((String::new(), String::new())),
        exclusions:  RwLock::new(config.exclusions.clone()),
        userscripts: RwLock::new(Arc::new(scripts)),
        blocked:     Default::default(),
        allowed:     Default::default(),
        log:         Default::default(),
    });
    let mut app = App { config, shared, problems, rules, running: None, cert_installed: false, cert_renewal: None, reply_log: None };
    app.refresh_cert();

    let mut stdout = tokio::io::stdout();
    let mut lines  = BufReader::new(tokio::io::stdin()).lines();
    let ready      = json!({ "ok": warning.is_none(), "event": "ready", "error": warning, "status": app.status() });
    let _ = stdout.write_all(format!("{ready}\n").as_bytes()).await;
    let _ = stdout.flush().await;

    // Command Loop
    let ending = windows_ending();
    tokio::pin!(ending);
    loop {
        let line = tokio::select! {
            line = lines.next_line() => line,
            _ = &mut ending => break,
        };
        let line = match line {
            Ok(Some(line)) => line,
            // A garbled line is answered as an error; only a closed or broken pipe ends the core
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                let _ = stdout.write_all(format!("{}\n", json!({ "ok": false, "error": "unreadable command" })).as_bytes()).await;
                let _ = stdout.flush().await;
                continue;
            }
            _ => break,
        };

        let mut reply = match serde_json::from_str::<Command>(&line) {
            Ok(cmd) => match app.run(cmd).await {
                Ok(()) => json!({ "ok": true, "status": app.status() }),
                Err(e) => json!({ "ok": false, "error": e, "status": app.status() }),
            },
            Err(e) => json!({ "ok": false, "error": e.to_string() }),
        };
        if let Some(log) = app.reply_log.take() {
            reply["log"] = log;
        }
        let _ = stdout.write_all(format!("{reply}\n").as_bytes()).await;
        let _ = stdout.flush().await;
    }

    let _ = app.stop().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_list_validation() {
        let lists = Config::default().lists;
        assert!(new_list(&lists, "Mine", "https://example.com/l.txt").is_ok_and(|l| l.kind == "custom" && l.enabled && !l.trusted));
        assert!(new_list(&lists, "", "https://example.com/l.txt").is_err());
        assert!(new_list(&lists, "Mine", "http://example.com/l.txt").is_err(), "plain http could be tampered with in transit");
        assert!(new_list(&lists, "Mine", "file:///C:/secret.txt").is_err());
        assert!(new_list(&lists, "EasyList", "https://other.com").is_err());
        assert!(new_list(&lists, "Dup", &lists[0].url).is_err());
    }

    #[test]
    fn commands_parse_from_json() {
        assert!(matches!(serde_json::from_str(r#"{"cmd":"start"}"#), Ok(Command::Start)));
        assert!(matches!(serde_json::from_str(r#"{"cmd":"renew_cert"}"#), Ok(Command::RenewCert)));
        assert!(matches!(
            serde_json::from_str(r#"{"cmd":"add_list","name":"a","url":"https://a"}"#),
            Ok(Command::AddList { .. })
        ));
        assert!(matches!(
            serde_json::from_str(r#"{"cmd":"set_userscript","name":"a","enabled":false}"#),
            Ok(Command::SetUserscript { .. })
        ));
        assert!(serde_json::from_str::<Command>(r#"{"cmd":"rm -rf"}"#).is_err());
    }

    #[test]
    fn normalize_host_rejects_junk() {
        assert_eq!(normalize_host(" *.Bank.com. ").as_deref(), Some("bank.com"));
        assert_eq!(normalize_host("bank.com/path"), None);
        assert_eq!(normalize_host(""), None);
    }
}
