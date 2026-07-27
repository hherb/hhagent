//! Operator upstream extra-CA config (#492): parse the host-keyed
//! `{origin-host: /abs/path/ca.pem}` JSON an operator sets, and select the one
//! CA a given force-routed worker's sidecar may trust on its **re-origination
//! (upstream) leg**.
//!
//! # What this is for, in one paragraph
//!
//! When a worker is force-routed, it does not talk to the internet directly.
//! Its traffic goes to a per-worker egress-proxy sidecar, which terminates the
//! worker's TLS (MITM), inspects it, and then opens a *second*, fresh TLS
//! connection to the real origin. That second connection is the "upstream leg",
//! and the proxy validates the origin's certificate against the **webpki root
//! store** — the same public CAs a browser trusts. A private origin with a
//! self-signed certificate (a personal localmail server, say) is therefore
//! unreachable: no public CA signed it. #491 added the *capability* to hand the
//! proxy one extra trust anchor for that leg; this module is the *operator
//! configuration* that decides which worker gets which anchor.
//!
//! # Why the rules below are so strict
//!
//! Adding a trust anchor to a sidecar widens what that sidecar will believe.
//! `egress-proxy::pins::build_upstream_client_config` adds the anchor to the
//! whole upstream root store, so it is trusted for **every host that sidecar can
//! reach** — not only the host it was keyed under. Host-keying alone therefore
//! bounds *which sidecar* holds the anchor, but not *which hosts within it*. If
//! a worker's allowlist ever mixed a private origin with a public one, the
//! operator's CA could impersonate that public host.
//!
//! Rather than document that hazard and hope, [`select_ca_for_allowlist`]
//! **enforces** it: an extra CA is handed out only when the worker's allowlist
//! resolves to a single private origin, written as an IP literal. Anything else
//! fails closed and refuses the spawn. See that function for the full rule.
//!
//! # Layering
//!
//! Like its sibling [`super::cert_pins`], the checks here are **structural**:
//! the JSON shape, path absoluteness, and the allowlist/private-origin rules.
//! The certificate file's *content* stays the egress-proxy's authority — it
//! parses the PEM and fails its own startup closed on a bad one. The one
//! content check here ([`check_ca_pem_contents`]) is a cheap startup sanity
//! probe so an operator typo surfaces when the daemon starts rather than on the
//! first force-routed dispatch.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use kastellan_net_classify::is_denied_range;

use super::cert_pins::host_of_endpoint;

/// The PEM header every X.509 certificate block starts with. Used only by the
/// startup sanity probe; the proxy does the real parse.
const PEM_CERT_MARKER: &str = "-----BEGIN CERTIFICATE-----";

/// A parsed, structurally-valid operator extra-CA config: lowercased origin host
/// → the absolute path of the PEM file to trust for it.
///
/// Invariant: every path is absolute and non-empty (both enforced by
/// [`parse_upstream_cas`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpstreamCaMap(BTreeMap<String, PathBuf>);

impl UpstreamCaMap {
    /// True when no origin has a CA configured (`{}`).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every configured `(host, ca_path)` pair, for startup validation + logging.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.0.iter().map(|(h, p)| (h.as_str(), p.as_path()))
    }
}

/// Structural failure parsing the operator's extra-CA JSON. Every variant makes
/// the daemon fail closed at startup rather than silently drop the config.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UpstreamCaError {
    /// Not valid JSON, or not a JSON object of host -> string path.
    #[error("upstream extra-CA config must be a JSON object of host -> \"/abs/path/ca.pem\": {0}")]
    Shape(String),
    /// A host mapped to an empty string — almost certainly a half-finished edit.
    #[error("host {0:?} has an empty CA path")]
    EmptyPath(String),
    /// A relative path. The CA is bound into the proxy jail via
    /// `SandboxPolicy.fs_read`, which rejects relative entries — so a relative
    /// path would fail later, naming the sandbox instead of this config.
    #[error("host {host:?} CA path {path:?} must be absolute (it is bound into the proxy jail)")]
    RelativePath { host: String, path: String },
}

/// Parse + structurally validate the operator extra-CA JSON.
///
/// Accepts a JSON object mapping an origin host to an absolute PEM path, e.g.
/// `{"10.0.0.3":"/home/me/.config/localmail/tls/cert.pem"}`. Host keys are
/// lowercased (DNS is case-insensitive, and the allowlist match below is done on
/// lowercased hosts).
pub fn parse_upstream_cas(json: &str) -> Result<UpstreamCaMap, UpstreamCaError> {
    // serde rejects any non-object / non-string-valued shape for us.
    let raw: BTreeMap<String, String> =
        serde_json::from_str(json).map_err(|e| UpstreamCaError::Shape(e.to_string()))?;
    let mut out = BTreeMap::new();
    for (host, path) in raw {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return Err(UpstreamCaError::EmptyPath(host));
        }
        let path_buf = PathBuf::from(trimmed);
        if !path_buf.is_absolute() {
            return Err(UpstreamCaError::RelativePath { host, path: trimmed.to_string() });
        }
        out.insert(host.to_ascii_lowercase(), path_buf);
    }
    Ok(UpstreamCaMap(out))
}

/// Why a worker may **not** be handed a configured extra CA. Each variant
/// refuses the spawn; none of them silently drops the anchor, because an
/// operator who configured one and got neither the anchor nor an error would
/// hold exactly the false "the force-routed path reaches my origin" belief that
/// #491/#492 exist to correct.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UpstreamCaSelectError {
    /// The worker's allowlist contains hosts beyond the keyed origin. Widening
    /// the sidecar's trust would let the operator CA vouch for those hosts too.
    #[error(
        "worker allowlist mixes the extra-CA origin {host:?} with other host(s) {others:?}; \
         an extra CA widens trust for EVERY host its sidecar can reach, so it is allowed \
         only for a single-origin worker"
    )]
    MixedAllowlist { host: String, others: Vec<String> },
    /// More than one configured origin is in this worker's allowlist. A sidecar
    /// takes at most one extra CA, and picking one silently would be arbitrary.
    #[error(
        "worker allowlist matches {0:?} extra-CA origins; a sidecar takes at most one \
         extra CA, and a single-origin worker is the only supported shape"
    )]
    MultipleKeyedHosts(Vec<String>),
    /// The keyed origin is not a private/loopback **IP literal**.
    ///
    /// Two independent reasons this must be a literal in a private range:
    /// widening trust is only defensible for an origin the operator physically
    /// controls; and the egress proxy denies any *hostname* that resolves into a
    /// private range (its SSRF guard), allowing only operator-allowlisted IP
    /// literals through the carve-out — so a name-keyed private origin would be
    /// unreachable regardless.
    #[error(
        "extra-CA origin {0:?} is not a private/loopback IP literal; an extra trust anchor \
         is supported only for an origin you control, written as a literal address \
         (e.g. 10.0.0.3 or 127.0.0.1) — the egress proxy's SSRF guard blocks hostnames \
         that resolve into private ranges anyway"
    )]
    NotPrivateOrigin(String),
}

/// Pure: is this allowlist host an origin we may widen trust for?
///
/// True only for an IP literal inside a range the proxy's SSRF guard would
/// otherwise deny (private, loopback, link-local, ULA, CGNAT, …) — i.e. an
/// address on the operator's own network rather than a public one.
fn is_private_literal(host: &str) -> bool {
    host.parse::<IpAddr>().map(is_denied_range).unwrap_or(false)
}

/// Select the extra CA for a worker, given its egress allowlist.
///
/// Returns:
/// * `Ok(None)` — no configured origin appears in this worker's allowlist, so
///   the worker's sidecar gets no extra anchor and the path stays byte-identical
///   to the webpki-only default. This is the case for every worker today except
///   a deliberately configured one.
/// * `Ok(Some(path))` — exactly one configured origin is in the allowlist, that
///   origin is the *only* host in the allowlist, and it is a private IP literal.
/// * `Err(_)` — the config and the worker disagree in a way that would either
///   widen trust too far or leave the operator with a false belief. The caller
///   must refuse the spawn (fail closed).
///
/// `allowlist` entries are `host:port` (the shape the proxy and web-common use);
/// matching is on the host alone, lowercased.
pub fn select_ca_for_allowlist<'m>(
    map: &'m UpstreamCaMap,
    allowlist: &[String],
) -> Result<Option<&'m Path>, UpstreamCaSelectError> {
    // Distinct hosts this worker may dial. A single origin published on two
    // ports (`h:80`, `h:443`) is still ONE host, which is why this is a set of
    // hosts rather than a count of allowlist entries.
    let hosts: BTreeSet<String> = allowlist
        .iter()
        .map(|ep| host_of_endpoint(ep).to_ascii_lowercase())
        .collect();

    let matched: Vec<(&String, &PathBuf)> =
        map.0.iter().filter(|(host, _)| hosts.contains(*host)).collect();

    let (host, path) = match matched.as_slice() {
        [] => return Ok(None),
        [one] => *one,
        many => {
            return Err(UpstreamCaSelectError::MultipleKeyedHosts(
                many.iter().map(|(h, _)| (*h).clone()).collect(),
            ))
        }
    };

    // The anchor is trusted for every host this sidecar can reach, so the
    // sidecar must be able to reach exactly one host.
    let others: Vec<String> = hosts.iter().filter(|h| *h != host).cloned().collect();
    if !others.is_empty() {
        return Err(UpstreamCaSelectError::MixedAllowlist { host: host.clone(), others });
    }

    if !is_private_literal(host) {
        return Err(UpstreamCaSelectError::NotPrivateOrigin(host.clone()));
    }

    Ok(Some(path.as_path()))
}

/// Why a configured CA file is unusable, detected at daemon startup.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UpstreamCaFileError {
    /// The file could not be read (missing, wrong permissions, a directory…).
    #[error("extra CA for {host:?} at {path:?} could not be read: {reason}")]
    Unreadable { host: String, path: String, reason: String },
    /// The file read fine but holds no PEM certificate block at all — a wrong
    /// path or a private key pasted in by mistake.
    #[error(
        "extra CA for {host:?} at {path:?} contains no PEM certificate block \
         (expected a line {marker:?})"
    )]
    NoCertificate { host: String, path: String, marker: &'static str },
}

/// Pure: sanity-check the *contents* of a configured CA file at startup.
///
/// Deliberately shallow — it only asserts that at least one PEM certificate
/// block is present, and reports how many were found. The egress-proxy remains
/// the authority on whether the certificate actually parses and validates; this
/// exists so the common operator typos (wrong path, empty file, a private-key
/// PEM) fail at daemon startup with a clear message instead of surfacing much
/// later as a sidecar bring-up failure on the first force-routed dispatch.
///
/// Returns the number of certificate blocks found (always ≥ 1 on `Ok`).
pub fn check_ca_pem_contents(
    host: &str,
    path: &Path,
    contents: &str,
) -> Result<usize, UpstreamCaFileError> {
    let count = contents.matches(PEM_CERT_MARKER).count();
    if count == 0 {
        return Err(UpstreamCaFileError::NoCertificate {
            host: host.to_string(),
            path: path.display().to_string(),
            marker: PEM_CERT_MARKER,
        });
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a map from JSON, panicking on a parse error — test sugar.
    fn map(json: &str) -> UpstreamCaMap {
        parse_upstream_cas(json).expect("valid config")
    }

    #[test]
    fn parses_valid_map_and_lowercases_hosts() {
        let m = map(r#"{"MyHost.Local":"/etc/ca.pem"}"#);
        assert!(!m.is_empty());
        assert_eq!(m.0.get("myhost.local"), Some(&PathBuf::from("/etc/ca.pem")));
        assert_eq!(m.0.get("MyHost.Local"), None);
    }

    #[test]
    fn empty_object_is_empty_map() {
        assert!(map("{}").is_empty());
    }

    #[test]
    fn rejects_relative_path() {
        let err = parse_upstream_cas(r#"{"10.0.0.3":"ca.pem"}"#).unwrap_err();
        assert_eq!(
            err,
            UpstreamCaError::RelativePath {
                host: "10.0.0.3".to_string(),
                path: "ca.pem".to_string()
            }
        );
    }

    #[test]
    fn rejects_empty_path() {
        let err = parse_upstream_cas(r#"{"10.0.0.3":"   "}"#).unwrap_err();
        assert_eq!(err, UpstreamCaError::EmptyPath("10.0.0.3".to_string()));
    }

    #[test]
    fn rejects_non_object_shape() {
        assert!(matches!(parse_upstream_cas("[]").unwrap_err(), UpstreamCaError::Shape(_)));
        assert!(matches!(parse_upstream_cas(r#"{"h":5}"#).unwrap_err(), UpstreamCaError::Shape(_)));
        assert!(matches!(
            parse_upstream_cas(r#"{"h":["/a"]}"#).unwrap_err(),
            UpstreamCaError::Shape(_)
        ));
    }

    #[test]
    fn entries_exposes_host_and_path() {
        let m = map(r#"{"10.0.0.3":"/etc/ca.pem"}"#);
        let got: Vec<(&str, &Path)> = m.entries().collect();
        assert_eq!(got, vec![("10.0.0.3", Path::new("/etc/ca.pem"))]);
    }

    // ---- selection: the happy path -------------------------------------

    #[test]
    fn selects_ca_for_single_private_literal_origin() {
        let m = map(r#"{"10.0.0.3":"/etc/localmail/ca.pem"}"#);
        let got = select_ca_for_allowlist(&m, &["10.0.0.3:8443".to_string()]).unwrap();
        assert_eq!(got, Some(Path::new("/etc/localmail/ca.pem")));
    }

    #[test]
    fn selects_for_loopback_literal() {
        let m = map(r#"{"127.0.0.1":"/etc/ca.pem"}"#);
        assert!(select_ca_for_allowlist(&m, &["127.0.0.1:8443".to_string()]).unwrap().is_some());
    }

    #[test]
    fn selects_for_bracketed_ipv6_ula() {
        // Bracketed IPv6 is the allowlist convention; fd00::/8 is a ULA, which
        // the proxy's SSRF guard denies for resolved names — i.e. private.
        let m = map(r#"{"fd00::1":"/etc/ca.pem"}"#);
        assert!(select_ca_for_allowlist(&m, &["[fd00::1]:8443".to_string()]).unwrap().is_some());
    }

    #[test]
    fn same_host_on_two_ports_is_still_one_origin() {
        let m = map(r#"{"10.0.0.3":"/etc/ca.pem"}"#);
        let allow = vec!["10.0.0.3:8443".to_string(), "10.0.0.3:443".to_string()];
        assert!(select_ca_for_allowlist(&m, &allow).unwrap().is_some());
    }

    // ---- selection: the no-op path -------------------------------------

    #[test]
    fn no_configured_origin_in_allowlist_is_none() {
        let m = map(r#"{"10.0.0.3":"/etc/ca.pem"}"#);
        assert!(select_ca_for_allowlist(&m, &["example.com:443".to_string()]).unwrap().is_none());
        assert!(select_ca_for_allowlist(&m, &[]).unwrap().is_none());
    }

    #[test]
    fn empty_config_is_none_for_any_allowlist() {
        let m = map("{}");
        assert!(select_ca_for_allowlist(&m, &["10.0.0.3:8443".to_string()]).unwrap().is_none());
    }

    // ---- selection: the fail-closed rules -------------------------------

    #[test]
    fn refuses_when_allowlist_mixes_the_origin_with_another_host() {
        // THE security assertion: a mixed allowlist would let the operator CA
        // vouch for api.example.com, because the anchor is added to the whole
        // upstream root store of that sidecar.
        let m = map(r#"{"10.0.0.3":"/etc/ca.pem"}"#);
        let allow = vec!["10.0.0.3:8443".to_string(), "api.example.com:443".to_string()];
        let err = select_ca_for_allowlist(&m, &allow).unwrap_err();
        assert_eq!(
            err,
            UpstreamCaSelectError::MixedAllowlist {
                host: "10.0.0.3".to_string(),
                others: vec!["api.example.com".to_string()],
            }
        );
    }

    #[test]
    fn refuses_when_two_configured_origins_are_both_allowlisted() {
        let m = map(r#"{"10.0.0.3":"/etc/a.pem","10.0.0.4":"/etc/b.pem"}"#);
        let allow = vec!["10.0.0.3:8443".to_string(), "10.0.0.4:8443".to_string()];
        let err = select_ca_for_allowlist(&m, &allow).unwrap_err();
        assert_eq!(
            err,
            UpstreamCaSelectError::MultipleKeyedHosts(vec![
                "10.0.0.3".to_string(),
                "10.0.0.4".to_string(),
            ])
        );
    }

    #[test]
    fn refuses_a_public_literal_origin() {
        // 93.184.216.34 is a public address: widening trust there would let the
        // operator CA impersonate a host they do not control.
        let m = map(r#"{"93.184.216.34":"/etc/ca.pem"}"#);
        let err =
            select_ca_for_allowlist(&m, &["93.184.216.34:443".to_string()]).unwrap_err();
        assert_eq!(
            err,
            UpstreamCaSelectError::NotPrivateOrigin("93.184.216.34".to_string())
        );
    }

    #[test]
    fn refuses_a_hostname_origin_even_if_it_sounds_local() {
        // The proxy's SSRF guard denies hostnames resolving into private ranges,
        // so a name-keyed private origin is unreachable regardless of trust.
        let m = map(r#"{"localmail.lan":"/etc/ca.pem"}"#);
        let err = select_ca_for_allowlist(&m, &["localmail.lan:8443".to_string()]).unwrap_err();
        assert_eq!(
            err,
            UpstreamCaSelectError::NotPrivateOrigin("localmail.lan".to_string())
        );
    }

    #[test]
    fn selection_is_case_insensitive_on_host() {
        let m = map(r#"{"myhost":"/etc/ca.pem"}"#);
        // Case-folding happens before the private-literal check, so this still
        // fails on privateness rather than silently missing the match.
        let err = select_ca_for_allowlist(&m, &["MYHOST:8443".to_string()]).unwrap_err();
        assert_eq!(err, UpstreamCaSelectError::NotPrivateOrigin("myhost".to_string()));
    }

    // ---- startup PEM sanity probe ---------------------------------------

    #[test]
    fn pem_probe_counts_certificate_blocks() {
        let pem = format!("{PEM_CERT_MARKER}\nAAA\n-----END CERTIFICATE-----\n");
        assert_eq!(check_ca_pem_contents("h", Path::new("/etc/ca.pem"), &pem).unwrap(), 1);
        let two = format!("{pem}{pem}");
        assert_eq!(check_ca_pem_contents("h", Path::new("/etc/ca.pem"), &two).unwrap(), 2);
    }

    #[test]
    fn pem_probe_rejects_a_file_with_no_certificate() {
        // A private key pasted in by mistake is the realistic version of this.
        let err = check_ca_pem_contents(
            "h",
            Path::new("/etc/ca.pem"),
            "-----BEGIN PRIVATE KEY-----\nAAA\n-----END PRIVATE KEY-----\n",
        )
        .unwrap_err();
        assert!(matches!(err, UpstreamCaFileError::NoCertificate { .. }));
    }

    #[test]
    fn pem_probe_rejects_an_empty_file() {
        assert!(check_ca_pem_contents("h", Path::new("/etc/ca.pem"), "").is_err());
    }
}
