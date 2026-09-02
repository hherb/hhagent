//! TLS certificate pinning for the egress-proxy upstream re-origination leg
//! (slice #4). Pure SPKI hashing + an RFC-7469 pin set + a custom rustls
//! `ServerCertVerifier` that overlays pin enforcement on top of (never instead
//! of) standard webpki chain validation. Design:
//! docs/superpowers/specs/2026-06-13-egress-proxy-slice4-tls-pinning-design.md

use sha2::{Digest, Sha256};
use x509_cert::der::{Decode, Encode};

/// Marker embedded in the rustls error a pin mismatch produces, so the sync
/// accept path (`proxy::run_mitm`) can distinguish a pin rejection from a
/// generic upstream-handshake failure without a typed error channel through
/// tokio-rustls.
pub const PIN_MISMATCH_MARKER: &str = "certificate pin mismatch";

/// Errors from parsing pins or extracting an SPKI. Display-only.
#[derive(Debug)]
pub enum PinError {
    /// The `KASTELLAN_EGRESS_PROXY_PINS` JSON did not parse / was the wrong shape.
    Json(String),
    /// A pin string was not a valid `sha256/<base64>` 32-byte digest.
    Pin(String),
    /// A host was listed with an empty pin array — almost certainly a
    /// misconfiguration (the operator meant to pin it but gave no pins). Carries
    /// the offending host.
    EmptyPinList(String),
    /// A certificate could not be parsed for SPKI extraction.
    X509(String),
    /// rustls refused to build the inner webpki verifier from the roots.
    Verifier(String),
    /// The operator-provided upstream extra CA could not be read or parsed.
    ExtraCa(String),
}

impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PinError::Json(s) => write!(f, "pins JSON: {s}"),
            PinError::Pin(s) => write!(f, "pin value: {s}"),
            PinError::EmptyPinList(host) => {
                write!(f, "host {host:?} has an empty pin list (omit the host to leave it unpinned, or add at least one sha256/<base64> pin)")
            }
            PinError::X509(s) => write!(f, "certificate SPKI: {s}"),
            PinError::Verifier(s) => write!(f, "webpki verifier: {s}"),
            PinError::ExtraCa(s) => write!(f, "upstream extra CA: {s}"),
        }
    }
}
impl std::error::Error for PinError {}

/// Compute the RFC-7469 pin pre-image hash of a certificate: `SHA-256` over the
/// DER-encoded `SubjectPublicKeyInfo`. `to_der()` re-encodes; for canonical DER
/// (every CA-issued cert) that is byte-identical to the original SPKI bytes —
/// pinned by `spki_sha256_matches_independently_computed_pin`.
pub fn spki_sha256(cert_der: &[u8]) -> Result<[u8; 32], PinError> {
    let cert = x509_cert::Certificate::from_der(cert_der)
        .map_err(|e| PinError::X509(format!("parse cert: {e}")))?;
    let spki_der = cert
        .tbs_certificate
        .subject_public_key_info
        .to_der()
        .map_err(|e| PinError::X509(format!("encode SPKI: {e}")))?;
    Ok(Sha256::digest(&spki_der).into())
}

use std::collections::{HashMap, HashSet};

/// A parsed set of SPKI pins, keyed by lowercased host.
#[derive(Debug, Default, Clone)]
pub struct PinSet {
    map: HashMap<String, HashSet<[u8; 32]>>,
}

impl PinSet {
    /// Parse the `KASTELLAN_EGRESS_PROXY_PINS` JSON:
    /// `{ "host": ["sha256/<base64>", ...], ... }`. Host keys are lowercased.
    /// Strict on structure (consistent with the module's fail-loud posture —
    /// `build_upstream_client_config` aborts proxy startup on any `Err`): anything
    /// that is not an object of string→array-of-`sha256/<base64>`-strings, a pin
    /// that does not decode to exactly 32 bytes, or a host listed with an *empty*
    /// pin array (an unsatisfiable set — almost always a misconfiguration), is an
    /// `Err`. An empty top-level object (`{}`) is fine and means "no hosts pinned".
    pub fn parse(json: &str) -> Result<PinSet, PinError> {
        let raw: HashMap<String, Vec<String>> = serde_json::from_str(json)
            .map_err(|e| PinError::Json(e.to_string()))?;
        let mut map = HashMap::new();
        for (host, pin_strs) in raw {
            if pin_strs.is_empty() {
                return Err(PinError::EmptyPinList(host));
            }
            let mut pins = HashSet::new();
            for s in &pin_strs {
                pins.insert(parse_pin(s)?);
            }
            map.insert(host.to_ascii_lowercase(), pins);
        }
        Ok(PinSet { map })
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The pin set for `host`, if the operator pinned it (case-insensitive).
    pub fn pins_for(&self, host: &str) -> Option<&HashSet<[u8; 32]>> {
        self.map.get(&host.to_ascii_lowercase())
    }
}

/// Decode one `sha256/<base64-standard>` pin string into a 32-byte digest.
fn parse_pin(s: &str) -> Result<[u8; 32], PinError> {
    use base64::Engine;
    let b64 = s
        .strip_prefix("sha256/")
        .ok_or_else(|| PinError::Pin(format!("missing `sha256/` prefix: {s:?}")))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| PinError::Pin(format!("base64: {e}")))?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| PinError::Pin(format!("expected 32 bytes, got {}", v.len())))
}

/// Test seam: match against already-hashed SPKIs. Production matching runs
/// against the VALIDATED path in [`verified_path_has_pin`] (the earlier
/// presented-list `chain_has_pin` was removed by the 2026-09-02 audit, F1).
#[cfg(test)]
pub(crate) fn chain_pins_contains(pins: &HashSet<[u8; 32]>, hashes: &[[u8; 32]]) -> bool {
    hashes.iter().any(|h| pins.contains(h))
}

use std::path::Path;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as RustlsError, RootCertStore, SignatureScheme};

/// Render a rustls `ServerName` to the host string used as the pin-map key.
fn server_name_host(name: &ServerName) -> String {
    match name {
        ServerName::DnsName(d) => d.as_ref().to_ascii_lowercase(),
        // Canonical text form (`1.2.3.4` / `::1`) so the key matches what an
        // operator writes in the pins JSON. IPv6 keys are BARE (no brackets);
        // operators must pin `"::1"`, not `"[::1]"`.
        ServerName::IpAddress(ip) => std::net::IpAddr::from(*ip).to_string(),
        // `ServerName` is non_exhaustive; an unknown kind is simply unpinnable.
        _ => String::new(),
    }
}

/// A rustls server-cert verifier that runs standard webpki chain validation and
/// then, for hosts in `pins`, additionally requires a chain SPKI to match a pin.
/// Unpinned hosts are unaffected (webpki only). Signature-verification methods
/// delegate to the inner webpki verifier unchanged.
#[derive(Debug)]
pub struct PinningVerifier {
    inner: Arc<WebPkiServerVerifier>,
    roots: Arc<RootCertStore>,
    pins: PinSet,
}

impl PinningVerifier {
    /// Build over `roots`. Returns `Err` only if rustls refuses the roots.
    pub fn new(roots: Arc<RootCertStore>, pins: PinSet) -> Result<Self, PinError> {
        let inner = WebPkiServerVerifier::builder(Arc::clone(&roots))
            .build()
            .map_err(|e| PinError::Verifier(e.to_string()))?;
        Ok(Self { inner, roots, pins })
    }
}

/// SHA-256 over the full SPKI `SEQUENCE` of a trust anchor. `TrustAnchor`
/// stores the *value* of the subjectPublicKeyInfo field (the bytes inside the
/// outer `SEQUENCE`), so re-wrap it before hashing so an anchor pin is the same
/// `sha256/<base64(SPKI DER)>` an operator computes from the root's PEM.
fn anchor_spki_sha256(anchor: &rustls::pki_types::TrustAnchor<'_>) -> [u8; 32] {
    let inner = anchor.subject_public_key_info.as_ref();
    let mut der = Vec::with_capacity(inner.len() + 6);
    der.push(0x30);
    let len = inner.len();
    if len < 0x80 {
        der.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let first = bytes.iter().position(|b| *b != 0).unwrap_or(bytes.len() - 1);
        der.push(0x80 | (bytes.len() - first) as u8);
        der.extend_from_slice(&bytes[first..]);
    }
    der.extend_from_slice(inner);
    Sha256::digest(&der).into()
}

/// True iff a pin matches the leaf, an intermediate ON THIS VALIDATED PATH, or
/// the anchor that terminates it. This is the check `verify_server_cert` runs
/// through webpki's `verify_path` callback (security audit 2026-09-02, egress
/// F1): the earlier check hashed every certificate the *server presented*,
/// and rustls hands the verifier the presented list unfiltered — webpki
/// path-builds internally and ignores certificates it does not need. So a
/// mis-issued but webpki-valid leaf shipped with the genuine pinned
/// intermediate appended (public information) satisfied the pin while the
/// validated path never touched it. Only certificates on the path count now.
fn verified_path_has_pin(
    pins: &HashSet<[u8; 32]>,
    leaf: &CertificateDer<'_>,
    path: &webpki::VerifiedPath<'_>,
) -> bool {
    if spki_sha256(leaf.as_ref()).is_ok_and(|h| pins.contains(&h)) {
        return true;
    }
    for cert in path.intermediate_certificates() {
        if spki_sha256(cert.der().as_ref()).is_ok_and(|h| pins.contains(&h)) {
            return true;
        }
    }
    pins.contains(&anchor_spki_sha256(path.anchor()))
}

impl ServerCertVerifier for PinningVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        // 1. ALWAYS: standard webpki chain validation. Fail-closed if it fails.
        self.inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)?;
        // 2. Pin overlay — only for hosts the operator pinned. Re-run path
        //    building with a `verify_path` callback so the pin is matched
        //    against the path webpki actually validated, not the presented
        //    list (security audit 2026-09-02, egress F1). webpki keeps
        //    building alternative paths while the callback rejects, so a pin
        //    on any valid path through the roots is honoured; if none carries
        //    a pin, the connection fails with the pin-mismatch marker.
        if let Some(pins) = self.pins.pins_for(&server_name_host(server_name)) {
            let leaf = webpki::EndEntityCert::try_from(end_entity)
                .map_err(|_| RustlsError::General(PIN_MISMATCH_MARKER.to_string()))?;
            let provider = rustls::crypto::CryptoProvider::get_default()
                .cloned()
                .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));
            let algs = provider.signature_verification_algorithms.all;
            let check = |path: &webpki::VerifiedPath<'_>| -> Result<(), webpki::Error> {
                if verified_path_has_pin(pins, end_entity, path) {
                    Ok(())
                } else {
                    Err(webpki::Error::UnknownIssuer)
                }
            };
            leaf.verify_for_usage(
                algs,
                &self.roots.roots,
                intermediates,
                now,
                webpki::KeyUsage::server_auth(),
                None,
                Some(&check),
            )
            .map_err(|_| RustlsError::General(PIN_MISMATCH_MARKER.to_string()))?;
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Add every certificate in `pem` to `roots` as a trust anchor for the upstream
/// re-origination leg. Fail-closed: an unparseable cert, or a PEM containing
/// **zero** certificates, is an error — we never proceed with an extra CA the
/// operator asked for but we could not load. Pure over its inputs (no
/// filesystem), so the trust-widening logic is unit-testable directly.
pub(crate) fn add_extra_ca_pem(roots: &mut RootCertStore, pem: &[u8]) -> Result<(), PinError> {
    let mut added = 0usize;
    for der in CertificateDer::pem_slice_iter(pem) {
        let der = der.map_err(|e| PinError::ExtraCa(format!("parse: {e}")))?;
        roots.add(der).map_err(|e| PinError::ExtraCa(format!("add: {e}")))?;
        added += 1;
    }
    if added == 0 {
        return Err(PinError::ExtraCa("PEM contained no certificates".to_string()));
    }
    Ok(())
}

/// Build the upstream-leg `ClientConfig` for the MITM re-origination.
///
/// * `None` / blank / `{}` ⇒ the plain webpki-roots config (byte-identical to
///   the pre-slice-#4 behaviour, zero added cost).
/// * a valid non-empty pin set ⇒ the same webpki roots wrapped in a
///   [`PinningVerifier`].
/// * a set-but-unparseable value ⇒ `Err` (the caller aborts startup — fail loud,
///   never silently degrade to no-pinning).
/// * `extra_ca_path`: `None` / absent ⇒ webpki-only, unchanged. `Some(path)` ⇒
///   the PEM's certificate(s) are added as extra trust anchors for the upstream
///   re-origination leg, **in addition to** webpki roots — for a self-signed
///   private origin (e.g. a personal localmail). A set-but-unreadable, invalid,
///   or zero-cert PEM ⇒ `Err` (fail-closed, aborts proxy startup, same as the
///   pins case above).
///
/// # Operator gotcha: the extra CA must not be a `CA:TRUE` self-signed *leaf*
///
/// Loading here only checks that the PEM parses as a certificate — it cannot
/// check that the anchor will actually validate the origin. Two shapes work:
/// a real CA that **signed** a separate origin leaf, or a self-signed leaf that
/// is its own anchor and carries `basicConstraints CA:FALSE`. A self-signed cert
/// used as both anchor and leaf while marked `CA:TRUE` does **not** work:
/// rustls-webpki rejects it with `CaUsedAsEndEntity` at handshake time, even
/// though `openssl verify` accepts it. That matters because `openssl req -x509`
/// commonly produces exactly that shape, and the failure surfaces late and
/// opaquely — as a `mitm_failed: …` egress decision, not as a startup error.
///
/// # Trust scope
///
/// The anchor lands in the sidecar's whole upstream root store, so it is trusted
/// for **every** host that sidecar can reach — not only the private origin. The
/// blast radius is bounded by one-sidecar-per-worker plus that worker's egress
/// allowlist, so it is safe for a single-origin worker (mail). Do not point it at
/// a worker whose allowlist mixes a private origin with public hosts: the extra
/// CA could then impersonate those public hosts. See #492.
pub fn build_upstream_client_config(
    pins_env: Option<&str>,
    extra_ca_path: Option<&Path>,
) -> Result<Arc<rustls::ClientConfig>, PinError> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    // Operator-provided extra trust anchor for the re-origination leg (e.g. a
    // self-signed personal localmail). Off by default ⇒ webpki-only, unchanged.
    // Fail-closed: a set-but-unreadable/invalid/zero-cert PEM aborts startup.
    if let Some(path) = extra_ca_path {
        let pem = std::fs::read(path)
            .map_err(|e| PinError::ExtraCa(format!("read {path:?}: {e}")))?;
        add_extra_ca_pem(&mut roots, &pem)?;
    }

    let pins = match pins_env.map(str::trim) {
        None | Some("") => PinSet::default(),
        Some(json) => PinSet::parse(json)?,
    };

    if pins.is_empty() {
        return Ok(Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        ));
    }

    let verifier = Arc::new(PinningVerifier::new(Arc::new(roots), pins)?);
    Ok(Arc::new(
        rustls::ClientConfig::builder()
            .dangerous() // custom verifier — STRENGTHENS validation (webpki + pin overlay)
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth(),
    ))
}

#[cfg(test)]
mod tests;
