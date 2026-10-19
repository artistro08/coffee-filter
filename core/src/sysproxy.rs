//! Windows system proxy (WinINet, current user) with crash-safe restore.
//!
//! Coffee Filter points Windows at a PAC script it serves itself (like Zen does)
//! instead of a fixed proxy. Apps ask the script per connection: web traffic goes
//! to the proxy, everything else connects directly. A fixed proxy breaks apps that
//! open non-web connections, because some (Qt apps like Stream Deck, for its Network
//! Dock) try to push UDP or raw sockets through it and fail with "The proxy type is
//! invalid for this operation"; Qt doesn't apply the bypass list to UDP at all.
//!
//! Before switching on, the user's own `ProxyEnable` and `AutoConfigURL` are saved to
//! `proxy-backup.json`; those are the only two values changed. If the app crashes, the
//! next start sees the backup and restores it first, so the PC is never left pointing
//! at a dead proxy.

use crate::config::{data_dir, write_atomic};
use serde::{Deserialize, Serialize};
use std::{fs, io, path::PathBuf};
use windows::Win32::Networking::WinInet::{
    INTERNET_OPTION_REFRESH, INTERNET_OPTION_SETTINGS_CHANGED, InternetSetOptionW,
};
use winreg::{RegKey, enums::*};

const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";

/// Name endings that mean "this PC or the home network". Shared with the proxy's own
/// never-decrypt check so the two can't drift apart.
pub const LOCAL_SUFFIXES: [&str; 4] = [".local", ".lan", ".home.arpa", ".localhost"];

/// Windows' own update and Microsoft Store servers (and their subdomains). Windows Update
/// downloads Store apps and updates using your proxy setting, and refuses any connection it
/// can't verify end to end, so going through the proxy made updates fail (0x80240437). These
/// always connect directly; there are no ads on them.
pub const SYSTEM_HOSTS: [&str; 8] = [
    "mp.microsoft.com",
    "update.microsoft.com",
    "windowsupdate.microsoft.com",
    "windowsupdate.com",
    "wustat.windows.com",
    "storequality.microsoft.com",
    "login.live.com",
    "msftconnecttest.com",
];

#[derive(Serialize, Deserialize)]
struct Backup {
    enable: u32,
    /// The user's own PAC script URL, replaced by ours while blocking.
    pac:    Option<String>,
}

/// A fresh, unguessable PAC path for one proxy run, so web pages can't probe
/// `127.0.0.1:<port>/proxy.pac` to tell that Coffee Filter is running.
pub fn new_pac_path() -> String {
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).expect("OS random number generator");
    format!("/proxy-{}.pac", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// Zen's list of sites that are never decrypted, copied as is from
/// <https://github.com/ZenPrivacy/zen-desktop/tree/master/internal/sysproxy/exclusions>
/// (MIT, see exclusions/LICENSE): logins, password managers, banks, government sites, and
/// Microsoft and Xbox services. They connect directly, around the proxy.
const ZEN_EXCLUSIONS: [&str; 2] = [include_str!("exclusions/common.txt"), include_str!("exclusions/windows.txt")];

/// Zen's never-decrypted sites, one name each (comments and blank lines dropped).
pub fn never_decrypted() -> &'static [&'static str] {
    static HOSTS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    HOSTS.get_or_init(|| {
        ZEN_EXCLUSIONS
            .iter()
            .flat_map(|list| list.lines())
            .map(|line| line.split('#').next().unwrap_or("").trim())
            .filter(|host| !host.is_empty())
            .collect()
    })
}

/// The PAC script: web traffic to the proxy; this PC, home-network devices, and any other
/// protocol connect directly. Address checks only run on IP literals, so the browser never
/// has to look up a site's name just to decide.
///
/// Web traffic falls back to DIRECT when the proxy can't be reached. Microsoft Store apps
/// (the Store itself, Xbox, Photos, ...) run sandboxed and Windows won't let them connect
/// to 127.0.0.1 at all; without the fallback they'd have no internet while blocking is on.
pub fn pac_script(port: u16) -> String {
    let suffixes: String = LOCAL_SUFFIXES
        .iter()
        .map(|s| format!(" || dnsDomainIs(host, \"{s}\")"))
        .chain(SYSTEM_HOSTS.iter().map(|h| format!(" || host === \"{h}\" || dnsDomainIs(host, \".{h}\")")))
        .collect();
    // The site or anything below it; not look-alikes ("evilmicrosoft.com")
    let direct = never_decrypted().iter().map(|h| format!("\"{h}\"")).collect::<Vec<_>>().join(",");
    format!(
        r#"var direct = [{direct}];

function FindProxyForURL(url, host) {{
    var ip = /^\d{{1,3}}(\.\d{{1,3}}){{3}}$/.test(host);
    if (isPlainHostName(host) || host === "localhost" || host.indexOf(":") !== -1{suffixes} ||
        (ip && (isInNet(host, "127.0.0.0", "255.0.0.0") || isInNet(host, "10.0.0.0", "255.0.0.0") ||
                isInNet(host, "172.16.0.0", "255.240.0.0") || isInNet(host, "192.168.0.0", "255.255.0.0") ||
                isInNet(host, "169.254.0.0", "255.255.0.0") || isInNet(host, "100.64.0.0", "255.192.0.0") ||
                isInNet(host, "0.0.0.0", "255.0.0.0")))) {{
        return "DIRECT";
    }}
    for (var i = 0; i < direct.length; i++) {{
        if (host === direct[i] || dnsDomainIs(host, "." + direct[i])) {{
            return "DIRECT";
        }}
    }}
    if (url.substring(0, 5) === "http:" || url.substring(0, 6) === "https:") {{
        return "PROXY 127.0.0.1:{port}; DIRECT";
    }}
    return "DIRECT";
}}
"#
    )
}

fn backup_path() -> io::Result<PathBuf> {
    Ok(data_dir()?.join("proxy-backup.json"))
}

fn key() -> io::Result<RegKey> {
    RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(KEY, KEY_READ | KEY_WRITE)
}

fn set_opt(key: &RegKey, name: &str, value: &Option<String>) -> io::Result<()> {
    match value {
        Some(v) => key.set_value(name, v),
        None => match key.delete_value(name) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
}

/// Tell running apps the proxy settings changed.
fn notify() {
    // SAFETY: documented null-handle broadcast calls with no buffers.
    unsafe {
        let _ = InternetSetOptionW(None, INTERNET_OPTION_SETTINGS_CHANGED, None, 0);
        let _ = InternetSetOptionW(None, INTERNET_OPTION_REFRESH, None, 0);
    }
}

fn is_ours(pac: &str) -> bool {
    pac.starts_with("http://127.0.0.1:") && pac.contains("/proxy-") && pac.ends_with(".pac")
}

pub fn enable(port: u16, pac_path: &str) -> io::Result<()> {
    let key = key()?;

    // Only back up once; an existing backup holds the user's real settings
    if !backup_path()?.exists() {
        let pac: Option<String> = key.get_value("AutoConfigURL").ok();
        let backup = Backup {
            enable: key.get_value("ProxyEnable").unwrap_or(0),
            pac:    pac.filter(|p| !is_ours(p)),
        };
        write_atomic(&backup_path()?, serde_json::to_string(&backup)?.as_bytes())?;
    }

    // PAC only; a fixed proxy alongside it would be used by apps that ignore PAC for everything
    key.set_value("ProxyEnable", &0u32)?;
    key.set_value("AutoConfigURL", &format!("http://127.0.0.1:{port}{pac_path}"))?;
    notify();
    Ok(())
}

/// Put back whatever was there before. No-op without a backup. A damaged backup can't
/// say what to restore, so then only our own PAC setting is removed.
pub fn restore() -> io::Result<()> {
    let path = backup_path()?;
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(());
    };

    let key = key()?;
    match serde_json::from_str::<Backup>(&text) {
        Ok(backup) => {
            key.set_value("ProxyEnable", &backup.enable)?;
            set_opt(&key, "AutoConfigURL", &backup.pac)?;
        }
        Err(_) => {
            let current: Option<String> = key.get_value("AutoConfigURL").ok();
            if current.as_deref().is_some_and(is_ours) {
                set_opt(&key, "AutoConfigURL", &None)?;
            }
        }
    }
    notify();
    fs::remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pac_paths_are_unguessable_and_recognized() {
        let (a, b) = (new_pac_path(), new_pac_path());
        assert_ne!(a, b);
        assert!(is_ours(&format!("http://127.0.0.1:18228{a}")));
        assert!(!is_ours("http://127.0.0.1:53847/proxy.pac?v=1"), "Zen's PAC is the user's, not ours");
    }

    #[test]
    fn pac_sends_only_web_traffic_to_the_proxy() {
        let pac = pac_script(18228);
        assert!(pac.contains("PROXY 127.0.0.1:18228; DIRECT"), "sandboxed Store apps can't reach the proxy and need a way out");
        for suffix in LOCAL_SUFFIXES {
            assert!(pac.contains(&format!("dnsDomainIs(host, \"{suffix}\")")));
        }
        assert!(pac.contains("(ip && (isInNet"), "address checks only on IP literals, no DNS lookups");
        assert!(pac.contains("100.64.0.0"));
        for host in SYSTEM_HOSTS {
            assert!(pac.contains(&format!("host === \"{host}\" || dnsDomainIs(host, \".{host}\")")), "{host} goes direct");
        }
        for host in ["1password.com", "chase.com", "accounts.google.com", "login.live.com", "windowsupdate.com"] {
            assert!(pac.contains(&format!("\"{host}\"")), "Zen's {host} goes direct");
        }
    }

    #[test]
    fn zen_list_is_plain_site_names() {
        let hosts = never_decrypted();
        assert!(hosts.len() > 100, "the whole list: {}", hosts.len());
        for host in hosts {
            assert!(host.contains('.') && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'), "{host:?}");
        }
    }

    #[test]
    fn old_backups_still_parse() {
        let old: Backup = serde_json::from_str(r#"{"enable":1,"server":"x:1","override_":"<local>","pac":null}"#).unwrap();
        assert_eq!(old.enable, 1);
    }
}
