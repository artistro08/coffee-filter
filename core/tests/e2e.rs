//! End-to-end test of the real proxy binary.
//!
//! Runs coffee-filter-core in an isolated profile (temp LOCALAPPDATA, port 18229)
//! that reuses this PC's installed CA, then checks blocking, exclusions, upstream
//! certificate checks, and that proxy settings are restored after stop, app exit,
//! and a crash. It changes the system proxy for a few seconds and needs internet,
//! so it only runs on request:
//!
//!     cargo test --release -- --ignored

use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};
use winreg::{RegKey, enums::*};

const PORT: u16 = 18229;
const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";
const TEST_PAC: &str = "http://127.0.0.1:1/coffee-filter-test.pac";
const TEST_LIST_URL: &str = "https://invalid.invalid/never-downloaded";

/// One network rule, plus one of each cosmetic kind on example.com, and one on github.com
/// (strict CSP: inline scripts only run with our nonce).
const TEST_LIST: &str = "||doubleclick.net^
example.com##h1
example.com##p:has-text(documentation):remove()
example.com#%#//scriptlet('remove-attr', 'lang', 'html')
github.com##footer:remove()
";

// =========================================================================
// HELPERS
// =========================================================================

struct Core {
    child:  Child,
    stdin:  Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Core {
    fn spawn(profile: &Path) -> (Self, Value) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_coffee-filter-core"))
            .env("LOCALAPPDATA", profile)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin    = child.stdin.take();
        let stdout   = BufReader::new(child.stdout.take().unwrap());
        let mut core = Core { child, stdin, stdout };
        let ready    = core.read();
        assert_eq!(ready["event"], "ready");
        (core, ready)
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn send(&mut self, cmd: Value) -> Value {
        writeln!(self.stdin.as_mut().unwrap(), "{cmd}").unwrap();
        self.read()
    }
}

fn settings() -> RegKey {
    RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(KEY, KEY_READ | KEY_WRITE).unwrap()
}

fn proxy_state() -> (u32, Option<String>, Option<String>) {
    let key = settings();
    (
        key.get_value("ProxyEnable").unwrap_or(0),
        key.get_value("ProxyServer").ok(),
        key.get_value("AutoConfigURL").ok(),
    )
}

fn set_opt(name: &str, value: &Option<String>) {
    match value {
        Some(v) => settings().set_value(name, v).unwrap(),
        None => {
            let _ = settings().delete_value(name);
        }
    }
}

/// Windows curl uses the Windows trust store, like browsers do. Revocation is skipped
/// because local CAs have no revocation server (browsers don't hard-fail on that).
fn curl(url: &str, headers: &[&str]) -> String {
    let curl = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join(r"System32\curl.exe");
    let mut cmd = Command::new(curl);
    cmd.args(["-s", "--ssl-no-revoke", "--max-time", "20", "-o", "NUL", "-w", "%{http_code}"]);
    cmd.args(["-x", &format!("127.0.0.1:{PORT}")]);
    for h in headers {
        cmd.args(["-H", h]);
    }
    String::from_utf8(cmd.arg(url).output().unwrap().stdout).unwrap()
}

/// Load a page through the proxy in headless Chrome and return the DOM after scripts ran.
/// Returns None when Chrome isn't installed.
fn chrome_dom(url: &str) -> Option<String> {
    chrome_dom_with(url, true)
}

/// `explicit_proxy: false` lets Chrome use Windows' settings (our PAC script), like a normal browser.
fn chrome_dom_with(url: &str, explicit_proxy: bool) -> Option<String> {
    let chrome = PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe");
    if !chrome.exists() {
        return None;
    }
    let profile = std::env::temp_dir().join(format!("coffee-filter-e2e-chrome-{}", std::process::id()));
    let out = Command::new(chrome)
        .args(["--headless=new", "--no-first-run", "--virtual-time-budget=5000"])
        .arg(format!("--user-data-dir={}", profile.display()))
        .args(explicit_proxy.then(|| format!("--proxy-server=127.0.0.1:{PORT}")))
        .args(["--dump-dom", url])
        .output()
        .ok()?;
    let _ = fs::remove_dir_all(&profile);
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Temp profile with this PC's CA, a one-rule test list, and its own port.
fn make_profile() -> PathBuf {
    let real    = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("CoffeeFilter");
    let profile = std::env::temp_dir().join(format!("coffee-filter-e2e-{}", std::process::id()));
    let dir     = profile.join("CoffeeFilter");
    fs::create_dir_all(dir.join("lists")).unwrap();

    for file in ["ca.cer", "ca.key.dpapi", "scriptlets.json"] {
        fs::copy(real.join(file), dir.join(file)).expect("install the certificate from the app first");
    }
    // Same file name the core uses for a cached list: name plus a hash of its URL
    let hash = sha1_smol::Sha1::from(TEST_LIST_URL).digest().to_string();
    fs::write(dir.join("lists").join(format!("Test-{}.txt", &hash[..8])), TEST_LIST).unwrap();
    fs::write(
        dir.join("config.json"),
        json!({
            "port": PORT,
            "exclusions": [],
            "lists": [{ "name": "Test", "url": TEST_LIST_URL, "enabled": true, "kind": "custom" }],
        })
        .to_string(),
    )
    .unwrap();
    profile
}

// =========================================================================
// TEST
// =========================================================================

#[test]
#[ignore = "changes the system proxy and needs internet; run with --ignored"]
fn proxy_end_to_end() {
    let original = proxy_state();
    let profile  = make_profile();

    // Pretend another tool (like Zen) set a PAC script, which would override our proxy
    set_opt("AutoConfigURL", &Some(TEST_PAC.into()));
    let before = proxy_state();

    let result = std::panic::catch_unwind(|| {
        // Start: the user's PAC is replaced by ours
        let (mut core, ready) = Core::spawn(&profile);
        assert_eq!(ready["status"]["rules"], 5);
        assert_eq!(ready["status"]["cert_installed"], true);
        assert_eq!(core.send(json!({ "cmd": "start" }))["ok"], true);
        // Windows is pointed at our PAC script, which the proxy serves
        let (enabled, _, pac) = proxy_state();
        assert_eq!(enabled, 0, "PAC only, no fixed proxy");
        let pac = pac.expect("PAC URL set");
        assert!(pac.starts_with(&format!("http://127.0.0.1:{PORT}/proxy-")) && pac.ends_with(".pac"), "{pac}");
        let direct = |url: &str| {
            let out = Command::new(PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join(r"System32\curl.exe"))
                .args(["-s", "--noproxy", "*", "-w", "\n%{http_code}", url])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let script = direct(&pac);
        assert!(direct(&format!("http://127.0.0.1:{PORT}/proxy.pac")).ends_with("404"), "the PAC path can't be guessed");
        assert!(script.contains("FindProxyForURL") && script.contains(&format!("PROXY 127.0.0.1:{PORT}")), "{script}");
        assert!(script.contains("192.168.0.0") && script.contains("return \"DIRECT\""), "home network and non-web traffic go direct");

        // A second core on the same profile refuses to start, so it can't switch the first one off
        let second = Command::new(env!("CARGO_BIN_EXE_coffee-filter-core")).env("LOCALAPPDATA", &profile).stdin(Stdio::null()).output().unwrap();
        assert!(String::from_utf8_lossy(&second.stdout).contains("already running"));
        assert_eq!(proxy_state().0, 0);
        assert!(proxy_state().2.is_some_and(|p| p == pac), "first core still in charge");

        // Filtering
        let ad = "https://securepubads.g.doubleclick.net/tag/js/gpt.js";
        assert_eq!(curl("https://example.com/", &[]), "200", "clean site loads");
        assert_eq!(curl(ad, &["Sec-Fetch-Dest: script", "Referer: https://news.example/"]), "403", "ad blocked");
        assert_eq!(curl("https://doubleclick.net/", &["Sec-Fetch-Dest: document"]), "403", "blocked page load");
        assert_eq!(curl("https://self-signed.badssl.com/", &[]), "502", "bad upstream cert refused");
        assert_eq!(curl("https://expired.badssl.com/", &[]), "502", "expired upstream cert refused");

        // Cosmetic filtering in a real browser: hide (CSS), procedural (script), AdGuard scriptlet, strict CSP
        if let Some(dom) = chrome_dom("https://example.com/") {
            assert!(dom.contains("h1{display:none!important}"), "hide rule injected");
            assert!(!dom.contains("documentation examples"), "procedural :has-text():remove() ran");
            assert!(!dom.contains("<html lang="), "AdGuard remove-attr scriptlet ran");
        }
        if let Some(dom) = chrome_dom_with("https://example.com/", false) {
            assert!(dom.contains("h1{display:none!important}"), "a browser on Windows' settings goes through the PAC script into Coffee Filter");
        }
        if let Some(dom) = chrome_dom("https://github.com/") {
            assert!(dom.contains("<script nonce="), "script injected");
            assert!(!dom.contains("<footer"), "injected script ran despite GitHub's CSP");
        }

        // Exclusions: tunneled untouched, so no blocking and curl sees the real (bad) cert itself
        let reply = core.send(json!({ "cmd": "set_exclusions", "hosts": ["*.DoubleClick.net", "badssl.com", "not a host"] }));
        assert_eq!(reply["status"]["exclusions"], json!(["badssl.com", "doubleclick.net"]));
        assert_ne!(curl(ad, &["Sec-Fetch-Dest: script"]), "403", "excluded host not filtered");
        assert_eq!(curl("https://self-signed.badssl.com/", &[]), "000", "excluded host not decrypted");

        // An app with its own certificate store (like a pinned app) rejects ours. OpenSSL uses its own bundle.
        let openssl = PathBuf::from(r"C:\Program Files\Git\usr\bin\openssl.exe");
        if openssl.exists() {
            let _ = Command::new(&openssl)
                .args(["s_client", "-proxy", &format!("127.0.0.1:{PORT}"), "-connect", "www.wikipedia.org:443", "-servername", "www.wikipedia.org", "-verify_return_error"])
                .stdin(Stdio::null())
                .output();
        }

        // Connection problems Coffee Filter caused are reported per host
        let problems = core.send(json!({ "cmd": "status" }))["status"]["problems"].clone();
        let kind_of  = |host: &str| problems.as_array().unwrap().iter().find(|p| p["host"] == host).map(|p| p["kind"].clone());
        assert_eq!(kind_of("self-signed.badssl.com"), None, "fails in browsers too, not ours");
        if PathBuf::from(r"C:\Program Files\Git\usr\bin\openssl.exe").exists() {
            assert_eq!(kind_of("www.wikipedia.org"), Some(json!("app_rejected_certificate")), "{problems}");
        }
        assert_eq!(kind_of("example.com"), None, "working sites aren't problems");

        let status = core.send(json!({ "cmd": "status" }))["status"].clone();
        assert!(status["blocked"].as_u64().unwrap() >= 1);
        assert!(status["allowed"].as_u64().unwrap() >= 1);

        // Bad input is rejected without killing the core
        assert_eq!(core.send(json!({ "cmd": "nope" }))["ok"], false);
        assert_eq!(core.send(json!({ "cmd": "add_list", "name": "x", "url": "file:///C:/x" }))["ok"], false);

        // Stop restores everything, including the PAC
        assert_eq!(core.send(json!({ "cmd": "stop" }))["ok"], true);
        assert_eq!(proxy_state(), before);

        // App exits (stdin closes): core restores and exits
        assert_eq!(core.send(json!({ "cmd": "start" }))["ok"], true);
        drop(core.stdin.take());
        assert!(core.child.wait().unwrap().success());
        assert_eq!(proxy_state(), before);

        // Crash: settings stay ours until the next launch restores them
        let (mut core, _) = Core::spawn(&profile);
        assert_eq!(core.send(json!({ "cmd": "start" }))["ok"], true);
        core.child.kill().unwrap();
        let _ = core.child.wait();
        assert!(proxy_state().2.is_some_and(|p| p.starts_with(&format!("http://127.0.0.1:{PORT}/proxy-"))), "still ours until next launch");
        let (mut core, _) = Core::spawn(&profile);
        assert_eq!(proxy_state(), before);
        drop(core.stdin.take());
        let _ = core.child.wait();
    });

    // Put the real settings back no matter what happened
    let key = settings();
    key.set_value("ProxyEnable", &original.0).unwrap();
    set_opt("ProxyServer", &original.1);
    set_opt("AutoConfigURL", &original.2);
    let _ = fs::remove_dir_all(&profile);

    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
