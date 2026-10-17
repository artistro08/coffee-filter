//! Local root CA: generation, DPAPI-protected key storage, trust store install, and the
//! site certificates the proxy shows browsers.
//!
//! The CA is generated on this machine and never leaves it. Its private key is encrypted
//! with DPAPI (current-user scope), so only this Windows account can decrypt it. The
//! certificate is installed in the machine Root store (one UAC prompt), which every
//! browser on Windows trusts, and it can only sign website certificates (path length 0,
//! Server Authentication only).
//!
//! Site certificates use a separate key that exists only in memory for one proxy run,
//! so the CA key is never used in a TLS handshake.

use crate::config::{data_dir, write_atomic};
use hudsucker::{
    certificate_authority::CertificateAuthority,
    hyper::http::uri::Authority,
    rcgen::{
        BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose, SanType, SerialNumber, string::Ia5String,
    },
    rustls::{
        ServerConfig,
        crypto::CryptoProvider,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};
use std::{
    collections::HashMap,
    fs, io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};
use time::{Duration, OffsetDateTime};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HLOCAL, LocalFree, WAIT_TIMEOUT},
        Security::Cryptography::{
            CERT_FIND_SHA1_HASH, CERT_OPEN_STORE_FLAGS, CERT_QUERY_ENCODING_TYPE, CERT_STORE_OPEN_EXISTING_FLAG,
            CERT_STORE_PROV_SYSTEM_W, CERT_STORE_READONLY_FLAG, CERT_SYSTEM_STORE_LOCAL_MACHINE, CRYPT_INTEGER_BLOB,
            CRYPTPROTECT_UI_FORBIDDEN, CertCloseStore, CertFindCertificateInStore, CertFreeCertificateContext, CertOpenStore,
            CryptProtectData, CryptUnprotectData, PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
        },
        System::{
            Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx},
            SystemInformation::GetSystemDirectoryW,
            Threading::{GetExitCodeProcess, TerminateProcess, WaitForSingleObject},
        },
        UI::{
            Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
            WindowsAndMessaging::SW_HIDE,
        },
    },
    core::{HSTRING, PCWSTR, w},
};

pub const CA_NAME: &str = "Coffee Filter Local CA";

/// How long the CA is valid, and how early it's renewed before that.
const CA_LIFETIME: Duration = Duration::days(365 * 3);
const RENEW_BEFORE: Duration = Duration::days(30);

/// Site certificates: short-lived, regenerated daily while the proxy runs.
const LEAF_LIFETIME: Duration = Duration::days(7);
const LEAF_CACHE_SECS: u64 = 24 * 60 * 60;
const LEAF_CACHE_MAX: usize = 1_000;

fn cert_path() -> io::Result<PathBuf> {
    Ok(data_dir()?.join("ca.cer"))
}

fn key_path() -> io::Result<PathBuf> {
    Ok(data_dir()?.join("ca.key.dpapi"))
}

/// System32, asked from Windows itself. Not from the SystemRoot environment variable, which
/// whoever starts the core controls, and which picks the program run as admin.
fn system32() -> PathBuf {
    let mut buf = [0u16; 260];
    // SAFETY: the buffer outlives the call and its length is passed with it.
    let len = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
    if len == 0 || len > buf.len() {
        return PathBuf::from(r"C:\Windows\System32");
    }
    PathBuf::from(String::from_utf16_lossy(&buf[..len]))
}

// =========================================================================
// DPAPI
// =========================================================================

fn dpapi(data: &[u8], protect: bool) -> io::Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();

    // SAFETY: input points at a live slice; output is allocated by Windows and freed below.
    unsafe {
        if protect {
            CryptProtectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output)
        } else {
            CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output)
        }
        .map_err(io::Error::other)?;

        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(output.pbData as _)));
        Ok(bytes)
    }
}

// =========================================================================
// CA LIFECYCLE
// =========================================================================

/// Build a fresh CA key and self-signed certificate, in memory only.
fn new_ca() -> io::Result<(KeyPair, Certificate)> {
    let key = KeyPair::generate().map_err(io::Error::other)?;

    let mut params = CertificateParams::default();
    let mut name   = DistinguishedName::new();
    name.push(DnType::CommonName, CA_NAME);
    name.push(DnType::OrganizationName, "Coffee Filter");
    params.distinguished_name = name;
    // Path length 0 and Server Authentication only: it can sign website certificates and
    // nothing else (no other CAs, no code signing, no email)
    params.is_ca               = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages          = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.not_before          = OffsetDateTime::now_utc() - Duration::days(1);
    params.not_after           = OffsetDateTime::now_utc() + CA_LIFETIME;

    let cert = params.self_signed(&key).map_err(io::Error::other)?;
    Ok((key, cert))
}

/// Create a fresh CA and write it to disk (cert in DER, key DPAPI-encrypted).
fn generate() -> io::Result<()> {
    let (key, cert) = new_ca()?;
    write_atomic(&key_path()?, &dpapi(key.serialize_pem().as_bytes(), true)?)?;
    write_atomic(&cert_path()?, cert.der())?;
    Ok(())
}

/// Load the CA, generating one on first run.
pub fn load_or_create() -> io::Result<Issuer<'static, KeyPair>> {
    if !cert_path()?.exists() || !key_path()?.exists() {
        generate()?;
    }

    let der     = CertificateDer::from(fs::read(cert_path()?)?);
    let key_pem = String::from_utf8(dpapi(&fs::read(key_path()?)?, false)?).map_err(io::Error::other)?;
    let key     = KeyPair::from_pem(&key_pem).map_err(io::Error::other)?;
    Issuer::from_ca_cert_der(&der, key).map_err(io::Error::other)
}

/// Why the installed CA should be replaced, if it should: close to expiring, or made by an
/// older version without the Server Authentication restriction.
pub fn renewal_reason() -> Option<&'static str> {
    let der       = fs::read(cert_path().ok()?).ok()?;
    let (_, cert) = x509_parser::parse_x509_certificate(&der).ok()?;

    let expires = cert.validity().not_after.timestamp();
    if expires - (OffsetDateTime::now_utc() + RENEW_BEFORE).unix_timestamp() < 0 {
        return Some("The certificate is about to expire.");
    }
    let server_only = cert.extended_key_usage().ok().flatten().is_some_and(|e| e.value.server_auth && !e.value.any);
    if !server_only {
        return Some("The certificate is from an older version and can be made more secure.");
    }
    None
}

/// SHA-1 thumbprint of the CA certificate, the ID Windows uses for it.
fn thumbprint() -> io::Result<[u8; 20]> {
    Ok(sha1_smol::Sha1::from(fs::read(cert_path()?)?).digest().bytes())
}

fn thumbprint_hex() -> io::Result<String> {
    Ok(thumbprint()?.iter().map(|b| format!("{b:02x}")).collect())
}

// =========================================================================
// TRUST STORE
// =========================================================================
//
// The CA goes in the machine Root store. Chrome also reads the user store, but some
// Chromium browsers (Dia) only trust machine-wide roots. Reading the store needs no
// rights; writing it runs certutil elevated (one UAC prompt).

/// Longest an elevated step may take (UAC prompt, confirmation dialog, certutil) before
/// it's ended, so a dialog nobody answers can't hang the core.
const ELEVATED_TIMEOUT_MS: u32 = 120_000;

/// Run a System32 program elevated (UAC prompt), started in System32 so it finds System32's
/// own certutil first. Returns false if the user says no, it fails, or it times out.
fn run_elevated(program: &str, params: &str) -> io::Result<bool> {
    // ShellExecuteEx expects COM on the calling thread; a repeat call on a reused thread is harmless
    // SAFETY: plain per-thread COM setup with no pointers involved.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let exe    = HSTRING::from(system32().join(program).as_os_str());
    let dir    = HSTRING::from(system32().as_os_str());
    let params = HSTRING::from(params);
    let mut info = SHELLEXECUTEINFOW {
        cbSize:       size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask:        SEE_MASK_NOCLOSEPROCESS,
        lpVerb:       w!("runas"),
        lpFile:       PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        // Elevated process starts in System32, not a user-writable folder
        lpDirectory:  PCWSTR(dir.as_ptr()),
        nShow:        SW_HIDE.0,
        ..Default::default()
    };

    // SAFETY: info and its strings outlive the call; the process handle is closed below.
    unsafe {
        if ShellExecuteExW(&mut info).is_err() {
            return Ok(false);
        }
        if WaitForSingleObject(info.hProcess, ELEVATED_TIMEOUT_MS) == WAIT_TIMEOUT {
            let _ = TerminateProcess(info.hProcess, 1);
            let _ = CloseHandle(info.hProcess);
            return Ok(false);
        }
        let mut code = 1u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        Ok(code == 0)
    }
}

/// Is our CA in the machine Root store? Read in-process with CryptoAPI (no child process).
/// Still a store lookup, so the core caches the answer instead of asking on every status reply.
pub fn is_installed() -> io::Result<bool> {
    if !cert_path()?.exists() {
        return Ok(false);
    }
    let mut hash = thumbprint()?;
    let blob     = CRYPT_INTEGER_BLOB { cbData: hash.len() as u32, pbData: hash.as_mut_ptr() };
    let flags    = CERT_OPEN_STORE_FLAGS(CERT_SYSTEM_STORE_LOCAL_MACHINE) | CERT_STORE_READONLY_FLAG | CERT_STORE_OPEN_EXISTING_FLAG;

    // SAFETY: the store and context are closed/freed before returning; blob outlives the find.
    unsafe {
        let store = CertOpenStore(CERT_STORE_PROV_SYSTEM_W, CERT_QUERY_ENCODING_TYPE(0), None, flags, Some(w!("Root").as_ptr().cast()))
            .map_err(io::Error::other)?;
        let found = CertFindCertificateInStore(
            store,
            X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
            0,
            CERT_FIND_SHA1_HASH,
            Some((&blob as *const CRYPT_INTEGER_BLOB).cast()),
            None,
        );
        let installed = !found.is_null();
        if installed {
            let _ = CertFreeCertificateContext(Some(found));
        }
        let _ = CertCloseStore(Some(store), 0);
        Ok(installed)
    }
}

/// Shows a UAC prompt; returns false if the user declines.
/// Refuse to touch the trust store unless ca.cer looks like Coffee Filter's own certificate.
///
/// ca.cer lives in a folder any program running as this user can write to, while the
/// certutil steps run as admin. This stops a copy of a real root CA (or any other CA) from
/// being removed from or added to Windows through the prompt the user expects. It can't stop
/// a program running as this user from making its own look-alike: that program can already
/// read the real key, so the account itself is the trust boundary. `strict` (installing) also
/// requires today's limits: a CA that can't make other CAs and can only sign websites.
fn ensure_ours(strict: bool) -> io::Result<()> {
    check_ours(&fs::read(cert_path()?)?, strict)
}

fn check_ours(der: &[u8], strict: bool) -> io::Result<()> {
    let refuse    = || io::Error::other("ca.cer isn't a Coffee Filter certificate; it won't be added to or removed from Windows");
    let (_, cert) = x509_parser::parse_x509_certificate(der).map_err(|_| refuse())?;

    let named_ours  = cert.subject().iter_common_name().next().and_then(|cn| cn.as_str().ok()) == Some(CA_NAME);
    let self_signed = cert.subject() == cert.issuer();
    if !named_ours || !self_signed {
        return Err(refuse());
    }
    if !strict {
        return Ok(());
    }

    let limited     = cert.basic_constraints().ok().flatten().is_some_and(|b| b.value.ca && b.value.path_len_constraint == Some(0));
    let server_only = cert.extended_key_usage().ok().flatten().is_some_and(|e| {
        let e = e.value;
        e.server_auth && !e.any && !e.client_auth && !e.code_signing && !e.email_protection && !e.time_stamping && !e.ocsp_signing && e.other.is_empty()
    });
    if limited && server_only { Ok(()) } else { Err(refuse()) }
}

pub fn install() -> io::Result<bool> {
    load_or_create()?;
    if is_installed()? {
        return Ok(true);
    }
    ensure_ours(true)?;
    let path = cert_path()?;
    let path = path.to_string_lossy();
    if path.contains('"') {
        return Err(io::Error::other("unexpected character in data folder path"));
    }
    run_elevated("certutil.exe", &format!("-addstore Root \"{path}\""))?;
    is_installed()
}

/// Versions before 0.2 installed into the user's own Root store. Checked in the registry
/// (that store's backing key), because asking CryptoAPI would also see machine roots.
fn in_user_store() -> io::Result<bool> {
    let thumb = thumbprint_hex()?.to_uppercase();
    Ok(winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(format!(r"Software\Microsoft\SystemCertificates\Root\Certificates\{thumb}"))
        .is_ok())
}

/// Remove the cert from both trust stores and delete the CA files, so the key is gone for good.
///
/// Both deletions run in one elevated step (one UAC prompt). Windows also asks to confirm
/// removing a root from the user store; started from the approved UAC prompt, that dialog
/// is allowed to the front instead of opening hidden behind other windows.
pub fn uninstall() -> io::Result<bool> {
    if !cert_path()?.exists() {
        return Ok(true);
    }
    ensure_ours(false)?;
    let thumb   = thumbprint_hex()?;
    let machine = is_installed()?;
    let user    = in_user_store()?;
    // `thumb` is hex only, so it's safe inside the command line
    let steps = match (machine, user) {
        (true, true) => Some(format!("/d /c certutil.exe -delstore Root {thumb} & certutil.exe -user -delstore Root {thumb}")),
        (true, false) => Some(format!("/d /c certutil.exe -delstore Root {thumb}")),
        (false, true) => Some(format!("/d /c certutil.exe -user -delstore Root {thumb}")),
        (false, false) => None,
    };
    if let Some(steps) = steps {
        run_elevated("cmd.exe", &steps)?;
    }
    if is_installed()? || in_user_store()? {
        return Ok(false);
    }
    let _ = fs::remove_file(key_path()?);
    let _ = fs::remove_file(cert_path()?);
    Ok(true)
}

/// Replace the CA with a fresh one: remove the old (UAC), then make and install a new (UAC).
pub fn renew() -> io::Result<bool> {
    Ok(uninstall()? && install()?)
}

// =========================================================================
// SITE CERTIFICATES
// =========================================================================

/// Issues the certificates browsers see for decrypted sites.
///
/// Unlike hudsucker's RcgenAuthority (which reuses the CA key for every site), leaves use
/// their own key, generated at proxy start and never written anywhere. Leaves are valid
/// for 7 days and cached for 1 day per host, at most 1,000 hosts.
pub struct LeafAuthority {
    issuer:   Issuer<'static, KeyPair>,
    leaf_key: KeyPair,
    provider: Arc<CryptoProvider>,
    cache:    Mutex<HashMap<String, (Instant, Arc<ServerConfig>)>>,
}

impl LeafAuthority {
    pub fn new(issuer: Issuer<'static, KeyPair>, provider: CryptoProvider) -> io::Result<Self> {
        Ok(Self {
            issuer,
            leaf_key: KeyPair::generate().map_err(io::Error::other)?,
            provider: Arc::new(provider),
            cache:    Mutex::new(HashMap::new()),
        })
    }

    /// A certificate for `host`, signed by the CA, carrying the leaf key.
    fn leaf_cert(&self, host: &str) -> io::Result<CertificateDer<'static>> {
        let mut serial = [0u8; 8];
        getrandom::fill(&mut serial).map_err(io::Error::other)?;

        let mut params = CertificateParams::default();
        let mut name   = DistinguishedName::new();
        name.push(DnType::CommonName, host);
        params.distinguished_name = name;
        params.serial_number      = Some(SerialNumber::from(u64::from_be_bytes(serial)));
        params.not_before         = OffsetDateTime::now_utc() - Duration::minutes(1);
        params.not_after          = OffsetDateTime::now_utc() + LEAF_LIFETIME;
        params.key_usages         = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        params.subject_alt_names = vec![match host.trim_matches(['[', ']']).parse::<std::net::IpAddr>() {
            Ok(ip) => SanType::IpAddress(ip),
            Err(_) => SanType::DnsName(Ia5String::try_from(host).map_err(io::Error::other)?),
        }];

        Ok(params.signed_by(&self.leaf_key, &self.issuer).map_err(io::Error::other)?.into())
    }

    fn server_config(&self, host: &str) -> io::Result<Arc<ServerConfig>> {
        let key    = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(self.leaf_key.serialize_der()));
        let mut cfg = ServerConfig::builder_with_provider(self.provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(io::Error::other)?
            .with_no_client_auth()
            .with_single_cert(vec![self.leaf_cert(host)?], key)
            .map_err(io::Error::other)?;
        // HTTP/2 first: browsers then send every request for a site over one connection
        cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Arc::new(cfg))
    }
}

impl CertificateAuthority for LeafAuthority {
    async fn gen_server_config(&self, authority: &Authority) -> Arc<ServerConfig> {
        let host = authority.host().to_ascii_lowercase();

        if let Some((made, cfg)) = self.cache.lock().unwrap().get(&host)
            && made.elapsed().as_secs() < LEAF_CACHE_SECS
        {
            return cfg.clone();
        }

        // A host name we can't put in a certificate can't be decrypted; the handshake fails
        // and the problem shows up in the app like any other refused connection
        let cfg = self.server_config(&host).unwrap_or_else(|_| self.server_config("invalid.invalid").expect("static host name"));

        let mut cache = self.cache.lock().unwrap();
        // Full: drop only the oldest site, so the rest keep their certificates and TLS sessions
        if cache.len() >= LEAF_CACHE_MAX
            && let Some(oldest) = cache.iter().min_by_key(|(_, (made, _))| *made).map(|(h, _)| h.clone())
        {
            cache.remove(&oldest);
        }
        cache.insert(host, (Instant::now(), cfg.clone()));
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hudsucker::rustls::{
        RootCertStore,
        client::{WebPkiServerVerifier, danger::ServerCertVerifier},
        crypto::aws_lc_rs,
        pki_types::{ServerName, UnixTime},
    };

    fn dump(der: &[u8]) -> String {
        let file = std::env::temp_dir().join(format!("cf-test-{}-{}.cer", std::process::id(), der.len()));
        fs::write(&file, der).unwrap();
        // Let Windows itself parse the certificate
        let out = std::process::Command::new(system32().join("certutil.exe")).args(["-dump", &file.to_string_lossy()]).output().unwrap();
        let _ = fs::remove_file(&file);
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn ca_is_constrained_to_signing_sites() {
        let (_, cert) = new_ca().unwrap();
        let dump      = dump(cert.der());
        assert!(dump.contains("Subject Type=CA"), "{dump}");
        assert!(dump.contains("Path Length Constraint=0"), "{dump}");
        assert!(dump.contains("Server Authentication"), "{dump}");
        assert!(!dump.contains("Code Signing"), "{dump}");
        assert!(dump.contains(CA_NAME));
    }

    #[test]
    fn leaves_chain_to_the_ca_with_their_own_key() {
        let (ca_key, ca_cert) = new_ca().unwrap();
        let ca_public         = ca_key.public_key_pem();
        let issuer            = Issuer::from_ca_cert_der(ca_cert.der(), ca_key).unwrap();
        let authority         = LeafAuthority::new(issuer, aws_lc_rs::default_provider()).unwrap();
        let leaf              = authority.leaf_cert("example.com").unwrap();

        let mut roots = RootCertStore::empty();
        roots.add(ca_cert.der().clone()).unwrap();
        let verifier = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::new(aws_lc_rs::default_provider())).build().unwrap();
        let name     = ServerName::try_from("example.com").unwrap();
        assert!(verifier.verify_server_cert(&leaf, &[], &name, &[], UnixTime::now()).is_ok(), "leaf chains to the CA");
        let other = ServerName::try_from("other.com").unwrap();
        assert!(verifier.verify_server_cert(&leaf, &[], &other, &[], UnixTime::now()).is_err(), "only valid for its own host");

        assert_ne!(authority.leaf_key.public_key_pem(), ca_public, "leaf key is not the CA key");
        let (_, parsed) = x509_parser::parse_x509_certificate(&leaf).unwrap();
        let days = (parsed.validity().not_after.timestamp() - parsed.validity().not_before.timestamp()) / 86_400;
        assert!(days <= 8, "short-lived leaf ({days} days)");
    }

    #[test]
    fn only_our_own_certificate_reaches_the_trust_store() {
        let (_, ours) = new_ca().unwrap();
        assert!(check_ours(ours.der(), true).is_ok());

        // Someone else's CA, or a copy of a real root, dropped into ca.cer
        let mut params = CertificateParams::default();
        params.distinguished_name.push(DnType::CommonName, "Evil Root");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let other = params.self_signed(&KeyPair::generate().unwrap()).unwrap();
        assert!(check_ours(other.der(), false).is_err());
        assert!(check_ours(b"not a certificate", false).is_err());

        // Our name without today's limits: removable (older versions), never installable
        let mut params = CertificateParams::default();
        params.distinguished_name.push(DnType::CommonName, CA_NAME);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let loose = params.self_signed(&KeyPair::generate().unwrap()).unwrap();
        assert!(check_ours(loose.der(), false).is_ok());
        assert!(check_ours(loose.der(), true).is_err());
    }

    #[test]
    fn leaves_for_ip_hosts_use_ip_names() {
        let (ca_key, ca_cert) = new_ca().unwrap();
        let authority = LeafAuthority::new(Issuer::from_ca_cert_der(ca_cert.der(), ca_key).unwrap(), aws_lc_rs::default_provider()).unwrap();
        assert!(dump(&authority.leaf_cert("1.2.3.4").unwrap()).contains("IP Address=1.2.3.4"));
    }

    #[test]
    fn each_ca_is_unique() {
        let (a, _) = new_ca().unwrap();
        let (b, _) = new_ca().unwrap();
        assert_ne!(a.serialize_der(), b.serialize_der());
    }

    #[test]
    fn dpapi_roundtrip() {
        let sealed = dpapi(b"secret", true).unwrap();
        assert_ne!(sealed, b"secret");
        assert_eq!(dpapi(&sealed, false).unwrap(), b"secret");
    }
}
