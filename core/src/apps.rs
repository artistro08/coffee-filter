//! Excluded apps: which program opened a connection to the proxy, so connections from apps
//! you excluded pass through untouched.
//!
//! Windows' TCP table maps the browser-side end of each connection (127.0.0.1:<port>) to the
//! process that owns it. From the process we get its program path, and for Microsoft Store
//! (MSIX) apps its package family name, which stays the same across updates.

use crate::config::ExcludedApp;
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    sync::Mutex,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS},
        NetworkManagement::IpHelper::{GetExtendedTcpTable, MIB_TCP_STATE_ESTAB, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_CONNECTIONS},
        Networking::WinSock::AF_INET,
        Storage::Packaging::Appx::GetPackageFamilyName,
        System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW},
    },
    core::PWSTR,
};

/// The program behind a connection.
#[derive(Debug, PartialEq)]
pub struct Owner {
    /// Full program path, lowercase.
    pub path:    String,
    /// Package family name for Microsoft Store (MSIX) apps.
    pub package: Option<String>,
}

impl ExcludedApp {
    fn matches(&self, owner: &Owner) -> bool {
        match (&self.package, &self.path) {
            (Some(package), _) => owner.package.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(package)),
            // Full Unicode lowercase on both sides, so "C:\Users\Émile\App.exe" matches too
            (None, Some(path)) => owner.path == path.to_lowercase(),
            (None, None) => false,
        }
    }
}

/// Which process owns the browser-side end of a connection to the proxy (`client` is the
/// address the proxy sees). IPv4 only: the PAC script sends traffic to 127.0.0.1.
pub fn owner(client: SocketAddr) -> Option<Owner> {
    let SocketAddr::V4(client) = client else { return None };
    let pid = owning_pid(*client.ip(), client.port())?;
    process_owner(pid)
}

fn owning_pid(ip: Ipv4Addr, port: u16) -> Option<u32> {
    // Ask for the size, then fill; try again (twice) if connections were added in between
    let mut size = 0u32;
    for _ in 0..3 {
        // u32 elements, so the buffer is aligned for the table's fields
        let mut buf = vec![0u32; (size as usize).div_ceil(4)];
        let ptr     = if buf.is_empty() { None } else { Some(buf.as_mut_ptr().cast()) };
        // SAFETY: `buf` holds at least `size` bytes for the call to fill, or nothing when asking for the size.
        let result  = unsafe { GetExtendedTcpTable(ptr, &mut size, false, AF_INET.0 as u32, TCP_TABLE_OWNER_PID_CONNECTIONS, 0) };
        if result != ERROR_SUCCESS.0 || buf.is_empty() {
            continue;
        }
        // SAFETY: on success the aligned buffer starts with the entry count, followed by that many
        // rows at the table's field offset; both stay inside the `size` bytes Windows wrote.
        let rows = unsafe {
            let count = buf.as_ptr().cast::<u32>().read() as usize;
            let first = buf.as_ptr().cast::<u8>().add(std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table)).cast::<MIB_TCPROW_OWNER_PID>();
            std::slice::from_raw_parts(first, count)
        };
        return rows.iter().find(|row| is_end(row, ip, port)).map(|row| row.dwOwningPid);
    }
    None
}

/// The live connection whose local end is ip:port. Closed connections waiting out TIME_WAIT
/// can share the port with a newer one (or belong to no process), so only established ones
/// count. Addresses and ports in the table are in network byte order.
fn is_end(row: &MIB_TCPROW_OWNER_PID, ip: Ipv4Addr, port: u16) -> bool {
    let row_ip   = Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes());
    let row_port = u16::from_be(row.dwLocalPort as u16);
    row.dwState == MIB_TCP_STATE_ESTAB.0 as u32 && row_ip == ip && row_port == port
}

fn process_owner(pid: u32) -> Option<Owner> {
    // SAFETY: a query-only handle, closed below on every path.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;

    let mut path = [0u16; 1024];
    let mut len  = path.len() as u32;
    // SAFETY: `path` holds `len` UTF-16 units and outlives the call.
    let named    = unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut len) }.is_ok();

    let mut family = [0u16; 256];
    let mut flen   = family.len() as u32;
    // SAFETY: as above; fails with APPMODEL_ERROR_NO_PACKAGE for ordinary programs.
    let packaged   = unsafe { GetPackageFamilyName(process, &mut flen, Some(PWSTR(family.as_mut_ptr()))) } == ERROR_SUCCESS;

    // SAFETY: the handle came from OpenProcess above.
    let _ = unsafe { CloseHandle(process) };
    if !named {
        return None;
    }
    Some(Owner {
        path:    String::from_utf16_lossy(&path[..len as usize]).to_lowercase(),
        // The length includes the terminating zero
        package: packaged.then(|| String::from_utf16_lossy(&family[..flen.saturating_sub(1) as usize])),
    })
}

/// Remembers, per connection, whether it came from an excluded app, so each connection is
/// looked up once rather than on every request it carries.
#[derive(Default)]
pub struct Cache {
    seen: Mutex<HashMap<SocketAddr, (Instant, bool)>>,
}

impl Cache {
    /// Forgotten after this long. A live connection's program can't change; this only covers
    /// Windows handing the port to another program later, which waits out TIME_WAIT (minutes).
    const KEEP: Duration = Duration::from_secs(60);

    pub fn get(&self, client: SocketAddr) -> Option<bool> {
        self.seen.lock().unwrap().get(&client).filter(|(at, _)| at.elapsed() < Self::KEEP).map(|(_, excluded)| *excluded)
    }

    pub fn put(&self, client: SocketAddr, excluded: bool) {
        let mut seen = self.seen.lock().unwrap();
        if seen.len() > 1024 {
            seen.retain(|_, (at, _)| at.elapsed() < Self::KEEP);
        }
        seen.insert(client, (Instant::now(), excluded));
    }

    pub fn clear(&self) {
        self.seen.lock().unwrap().clear();
    }
}

/// Whether the connection from `client` belongs to one of `apps`. Runs the Windows lookups
/// off the async workers; the answer is cached per connection.
pub async fn is_excluded(apps: &[ExcludedApp], cache: &Cache, client: SocketAddr) -> bool {
    if apps.is_empty() {
        return false;
    }
    if let Some(known) = cache.get(client) {
        return known;
    }
    let owner    = tokio::task::spawn_blocking(move || owner(client)).await.ok().flatten();
    let excluded = owner.is_some_and(|o| apps.iter().any(|a| a.matches(&o)));
    cache.put(client, excluded);
    excluded
}

/// Check an app before it's saved: a full path to an .exe, or a package family name
/// (`Name_publisherhash`, as Windows shows it).
pub fn validate(app: &ExcludedApp) -> Result<(), String> {
    if app.name.trim().is_empty() {
        return Err("The app needs a name.".into());
    }
    match (&app.path, &app.package) {
        (Some(path), None) => {
            let p = std::path::Path::new(path);
            if p.is_absolute() && path.to_ascii_lowercase().ends_with(".exe") {
                Ok(())
            } else {
                Err("Choose a program file (.exe).".into())
            }
        }
        (None, Some(package)) => {
            let valid = package.split_once('_').is_some_and(|(name, publisher)| {
                !name.is_empty()
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
                    && publisher.len() == 13
                    && publisher.chars().all(|c| c.is_ascii_alphanumeric())
            });
            if valid { Ok(()) } else { Err("That isn't an installed app's package name.".into()) }
        }
        _ => Err("Choose either a program or a Store app.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_program_behind_a_connection() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client   = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_server, seen_from) = listener.accept().unwrap();

        // This test's own program owns the client end
        let me    = std::env::current_exe().unwrap().to_string_lossy().to_lowercase();
        let found = owner(seen_from).expect("connection owner");
        assert_eq!(found.path, me);
        assert_eq!(found.package, None, "an ordinary program has no package");
        drop(client);
    }

    #[test]
    fn apps_match_by_package_or_path() {
        let program = Owner { path: r"c:\tools\app.exe".into(), package: None };
        let store   = Owner { path: r"c:\program files\windowsapps\x\app.exe".into(), package: Some("Contoso.App_8wekyb3d8bbwe".into()) };

        let by_path    = ExcludedApp { name: "App".into(), path: Some(r"C:\Tools\App.exe".into()), package: None };
        let accented   = ExcludedApp { name: "Émile".into(), path: Some(r"C:\Users\Émile\App.exe".into()), package: None };
        assert!(accented.matches(&Owner { path: r"c:\users\émile\app.exe".into(), package: None }), "non-English letters in the path");
        let by_package = ExcludedApp { name: "Store".into(), path: None, package: Some("contoso.app_8wekyb3d8bbwe".into()) };
        assert!(by_path.matches(&program) && !by_path.matches(&store));
        assert!(by_package.matches(&store) && !by_package.matches(&program));
    }

    #[test]
    fn only_real_programs_and_packages_are_saved() {
        let app = |path: Option<&str>, package: Option<&str>| ExcludedApp { name: "X".into(), path: path.map(Into::into), package: package.map(Into::into) };
        assert!(validate(&app(Some(r"C:\Tools\App.exe"), None)).is_ok());
        assert!(validate(&app(Some("app.exe"), None)).is_err(), "relative path");
        assert!(validate(&app(Some(r"C:\Tools\notes.txt"), None)).is_err());
        assert!(validate(&app(None, Some("Microsoft.WindowsStore_8wekyb3d8bbwe"))).is_ok());
        assert!(validate(&app(None, Some("bad name"))).is_err());
        assert!(validate(&app(Some(r"C:\a.exe"), Some("A_8wekyb3d8bbwe"))).is_err(), "one or the other");
    }
}
