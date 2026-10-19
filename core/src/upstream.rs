//! The proxy's own connections to sites, made to look like Chrome's.
//!
//! Coffee Filter ends the browser's TLS connection and opens its own to the site. Bot
//! protection (Akamai, DataDome, Cloudflare) reads that connection's TLS handshake, and a
//! handshake that doesn't match the browser named in the User-Agent reads as a bot: the site
//! answers with a block page, a challenge, or a reset (a 502 here). Zen copies the browser's
//! own handshake with uTLS for the same reason. Rust has no uTLS, so this builds the
//! handshake with BoringSSL, the TLS library Chrome itself uses, set up the way Chrome sets
//! it: cipher and group order (post-quantum first), signature algorithms, GREASE, shuffled
//! extensions, Brotli certificate compression, ALPS, ECH GREASE, OCSP, and SCTs.
//!
//! Certificates are still checked by Windows, exactly as before (rustls-platform-verifier,
//! the same check the browser makes, work and custom roots included); BoringSSL only checks
//! the handshake itself.
//!
//! <https://github.com/ZenPrivacy/zen-desktop/blob/master/internal/proxy/clienthello.go>

use crate::filter::PublicOnly;
use boring::ssl::{
    CertificateCompressionAlgorithm, CertificateCompressor, ConnectConfiguration, SslAlert, SslConnector, SslMethod, SslRef,
    SslSignatureAlgorithm as Sig, SslVerifyError, SslVerifyMode, SslVersion,
};
use hudsucker::rustls::{
    client::danger::ServerCertVerifier,
    crypto::aws_lc_rs,
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use foreign_types::ForeignTypeRef;
use hyper_boring::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use std::{io, sync::Arc};

/// Chrome's TLS 1.2 cipher order (TLS 1.3's is BoringSSL's own, the same as Chrome's).
const CIPHERS: &str = "ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES256-GCM-SHA384:\
ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:ECDHE-RSA-AES128-SHA:\
ECDHE-RSA-AES256-SHA:AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA:AES256-SHA";

/// Chrome's key exchange groups, post-quantum first.
const GROUPS: &str = "X25519MLKEM768:X25519:P-256:P-384";

/// Chrome's signature algorithms, in order, post-quantum (ML-DSA) first.
const SIGALGS: [Sig; 11] = [
    Sig::ML_DSA_44,
    Sig::ML_DSA_65,
    Sig::ML_DSA_87,
    Sig::ECDSA_SECP256R1_SHA256,
    Sig::RSA_PSS_RSAE_SHA256,
    Sig::RSA_PKCS1_SHA256,
    Sig::ECDSA_SECP384R1_SHA384,
    Sig::RSA_PSS_RSAE_SHA384,
    Sig::RSA_PKCS1_SHA384,
    Sig::RSA_PSS_RSAE_SHA512,
    Sig::RSA_PKCS1_SHA512,
];

/// The trust anchor IDs Chrome sends (draft-ietf-tls-trust-anchor-ids), copied from Chrome
/// 141's handshake: 32 IDs, each length-prefixed. They only tell the site which roots the
/// browser trusts; Windows still decides what's trusted.
const TRUST_ANCHORS: [u8; 204] = [
    0x05, 0x82, 0xdf, 0x13, 0x02, 0x06, 0x05, 0x82, 0xdf, 0x13, 0x02, 0x13, 0x04, 0xd6, 0x79, 0x09,
    0x0d, 0x04, 0xd6, 0x79, 0x09, 0x01, 0x08, 0x83, 0x9a, 0x64, 0x8c, 0x9b, 0x2d, 0x01, 0x09, 0x04,
    0xd6, 0x79, 0x09, 0x08, 0x05, 0x82, 0xdf, 0x13, 0x02, 0x0d, 0x08, 0x83, 0x9a, 0x64, 0x8c, 0x9b,
    0x2d, 0x01, 0x0b, 0x08, 0x83, 0x9a, 0x64, 0x8c, 0x9b, 0x2d, 0x01, 0x0a, 0x04, 0xd6, 0x79, 0x09,
    0x0b, 0x04, 0xd6, 0x79, 0x09, 0x05, 0x08, 0x83, 0x9a, 0x64, 0x8c, 0x9b, 0x2d, 0x01, 0x0c, 0x05,
    0x82, 0xdf, 0x13, 0x02, 0x01, 0x08, 0x83, 0x9a, 0x64, 0x8c, 0x9b, 0x2d, 0x01, 0x07, 0x05, 0x82,
    0xdf, 0x13, 0x02, 0x14, 0x04, 0xd6, 0x79, 0x09, 0x0c, 0x04, 0xd6, 0x79, 0x09, 0x03, 0x04, 0xd6,
    0x79, 0x09, 0x0f, 0x08, 0x83, 0x9a, 0x64, 0x8c, 0x9b, 0x2d, 0x01, 0x13, 0x04, 0xd6, 0x79, 0x09,
    0x04, 0x08, 0x83, 0x9a, 0x64, 0x8c, 0x9b, 0x2d, 0x01, 0x12, 0x04, 0xd6, 0x79, 0x09, 0x0a, 0x04,
    0xd6, 0x79, 0x09, 0x07, 0x04, 0xd6, 0x79, 0x09, 0x02, 0x08, 0x83, 0x9a, 0x64, 0x8c, 0x9b, 0x2d,
    0x01, 0x0d, 0x04, 0xd6, 0x79, 0x09, 0x0e, 0x05, 0x82, 0xdf, 0x13, 0x02, 0x0e, 0x04, 0xd6, 0x79,
    0x09, 0x09, 0x04, 0xd6, 0x79, 0x09, 0x06, 0x05, 0x82, 0xdf, 0x13, 0x02, 0x0f, 0x05, 0x82, 0xdf,
    0x13, 0x02, 0x12, 0x08, 0x83, 0x9a, 0x64, 0x8c, 0x9b, 0x2d, 0x01, 0x08
];

/// ALPN: HTTP/2, then HTTP/1.1, length-prefixed.
const ALPN: &[u8] = b"\x02h2\x08http/1.1";

/// HTTP/2 settings Chrome sends; the HTTP side of the fingerprint (see main.rs). Chrome
/// doesn't send a maximum frame size, so none is set. The order of the request's pseudo
/// headers is fixed by the h2 library (Zen leaves its HTTP/2 side as Go's, too).
pub const H2_HEADER_TABLE: u32      = 65_536;
pub const H2_STREAM_WINDOW: u32     = 6_291_456;
pub const H2_CONNECTION_WINDOW: u32 = 15_728_640;
pub const H2_MAX_HEADER_LIST: u32   = 262_144;

/// HTTPS connector for the proxy's upstream client: Chrome's handshake over a connection
/// that refuses local addresses (`PublicOnly`), with Windows checking each certificate.
pub fn connector(http: HttpConnector<PublicOnly>) -> Result<HttpsConnector<HttpConnector<PublicOnly>>, String> {
    let verifier = Arc::new(rustls_platform_verifier::Verifier::new(Arc::new(aws_lc_rs::default_provider())).map_err(|e| e.to_string())?);

    // The handshake Chrome sends
    let mut tls = SslConnector::builder(SslMethod::tls()).map_err(|e| e.to_string())?;
    tls.set_min_proto_version(Some(SslVersion::TLS1_2)).map_err(|e| e.to_string())?;
    tls.set_cipher_list(CIPHERS).map_err(|e| e.to_string())?;
    tls.set_curves_list(GROUPS).map_err(|e| e.to_string())?;
    tls.set_verify_algorithm_prefs(&SIGALGS).map_err(|e| e.to_string())?;
    tls.set_alpn_protos(ALPN).map_err(|e| e.to_string())?;
    tls.set_grease_enabled(true);
    tls.set_permute_extensions(true);
    tls.enable_ocsp_stapling();
    tls.enable_signed_cert_timestamps();
    tls.add_certificate_compression_algorithm(Brotli).map_err(|e| e.to_string())?;

    let mut https = HttpsConnector::with_connector(http, tls).map_err(|e| e.to_string())?;
    https.set_callback(move |config, uri| {
        chrome_extras(config);

        // Windows checks the certificate against the name the connection is for
        let host     = uri.host().unwrap_or("").trim_start_matches('[').trim_end_matches(']').to_string();
        let verifier = verifier.clone();
        config.set_custom_verify_callback(SslVerifyMode::PEER, move |ssl| verify(&*verifier, &host, ssl));
        Ok(())
    });
    Ok(https)
}

/// Per-connection parts of Chrome's handshake the safe API doesn't cover: ALPS (Chrome's
/// "application settings" for HTTP/2, on its current code point), trust anchor IDs, and ECH
/// GREASE.
fn chrome_extras(config: &mut ConnectConfiguration) {
    config.set_enable_ech_grease(true);
    // SAFETY: a valid SSL* owned by `config` for the whole call; the protocol name and trust
    // anchor IDs are static bytes that BoringSSL copies; no settings bytes are passed.
    unsafe {
        let ssl = config.as_ptr();
        boring_sys::SSL_set_alps_use_new_codepoint(ssl, 1);
        boring_sys::SSL_add_application_settings(ssl, b"h2".as_ptr(), 2, std::ptr::null(), 0);
        boring_sys::SSL_set1_requested_trust_anchors(ssl, TRUST_ANCHORS.as_ptr(), TRUST_ANCHORS.len());
    }
}

/// Windows' check of the site's certificate chain for `host`, like the browser's. Anything it
/// refuses ends the handshake, so the browser gets a 502 instead of a page from a site that
/// couldn't prove who it is.
fn verify(verifier: &dyn ServerCertVerifier, host: &str, ssl: &mut SslRef) -> Result<(), SslVerifyError> {
    let refuse = || SslVerifyError::Invalid(SslAlert::BAD_CERTIFICATE);

    let leaf = ssl.peer_certificate().and_then(|c| c.to_der().ok()).ok_or_else(refuse)?;
    let rest: Vec<CertificateDer<'static>> = ssl
        .peer_cert_chain()
        .map(|chain| chain.iter().filter_map(|c| c.to_der().ok()).filter(|der| *der != leaf).map(CertificateDer::from).collect())
        .unwrap_or_default();
    let name = ServerName::try_from(host.to_string()).map_err(|_| refuse())?;
    let ocsp = ssl.ocsp_status().unwrap_or(&[]).to_vec();

    verifier
        .verify_server_cert(&CertificateDer::from(leaf), &rest, &name, &ocsp, UnixTime::now())
        .map(|_| ())
        .map_err(|_| refuse())
}

/// Brotli certificate decompression, which Chrome offers (RFC 8879). The proxy only ever
/// receives compressed certificates, so it never compresses.
struct Brotli;

impl CertificateCompressor for Brotli {
    const ALGORITHM: CertificateCompressionAlgorithm = CertificateCompressionAlgorithm::BROTLI;
    const CAN_COMPRESS: bool                         = false;
    const CAN_DECOMPRESS: bool                       = true;

    fn decompress<W: io::Write>(&self, input: &[u8], output: &mut W) -> io::Result<()> {
        let mut reader = brotli_decompressor::Decompressor::new(input, 4096);
        io::copy(&mut reader, output).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_anchors_are_length_prefixed_ids() {
        let mut at    = 0;
        let mut count = 0;
        while at < TRUST_ANCHORS.len() {
            let len = TRUST_ANCHORS[at] as usize;
            assert!(len > 0, "IDs are never empty");
            at    += 1 + len;
            count += 1;
        }
        assert_eq!(at, TRUST_ANCHORS.len(), "the last ID ends exactly at the end");
        assert_eq!(count, 32);
    }

    #[test]
    fn brotli_certificates_decompress() {
        // "hello" compressed with Brotli
        let compressed = [0x0b, 0x02, 0x80, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x03];
        let mut out    = Vec::new();
        Brotli.decompress(&compressed, &mut out).unwrap();
        assert_eq!(out, b"hello");
    }
}
