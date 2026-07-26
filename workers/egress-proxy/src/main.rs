//! egress-proxy: a per-worker egress boundary. Listens on a UDS, enforces the
//! worker's host allowlist + SSRF/IP defense per CONNECT, tunnels to the pinned
//! IP. For TLS tunnels it MITM-terminates with a per-instance CA leaf and
//! re-originates a validated TLS session to the origin (slice #3a inspection);
//! plaintext tunnels pass through untouched. The public CA cert is exported as
//! `ca.pem` next to the UDS for the host to inject into the worker's trust store.
//! Design: docs/superpowers/specs/2026-06-10-egress-proxy-boundary-enforcement-design.md
//!
//! Env contract (set by the host-side `core::egress::spawn_sidecar`):
//!   KASTELLAN_EGRESS_PROXY_UDS       — absolute path of the UDS to bind.
//!   KASTELLAN_EGRESS_PROXY_ALLOWLIST — JSON array of `host[:port]` endpoint
//!       strings. A `:port` suffix scopes the grant to that port (#241); a bare
//!       host grants any port (the weaker back-compat form, flagged in the audit
//!       reason). See `proxy::decide` / `proxy::allowed_reason`.
//!   KASTELLAN_EGRESS_PROXY_WORKER    — the calling worker's name (for audit).
//!   KASTELLAN_EGRESS_PROXY_DISABLE_MITM — `"1"` ⇒ never MITM; transparently
//!       tunnel even a TLS ClientHello. For a worker that does its own
//!       end-to-end TLS and can't trust our per-instance CA (the browser,
//!       egress slice #2). Allowlist + SSRF at CONNECT are unaffected.

mod ca;
mod leaf_cache;
mod mitm;
mod pins;
mod proxy;
mod report;
mod request_line;

use std::os::unix::net::UnixListener;

use kastellan_worker_web_common::allowlist::HostAllowlist;

use proxy::{handle_conn, MitmCtx, StdResolve};
use report::LineReporter;

fn main() -> anyhow::Result<()> {
    // rustls 0.23: install the ring provider as the process default (needed by
    // both the server-side leaf configs and the upstream client config).
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("install rustls ring provider"))?;

    let uds = std::env::var("KASTELLAN_EGRESS_PROXY_UDS")
        .map_err(|_| anyhow::anyhow!("KASTELLAN_EGRESS_PROXY_UDS unset"))?;
    let allow_json = std::env::var("KASTELLAN_EGRESS_PROXY_ALLOWLIST")
        .map_err(|_| anyhow::anyhow!("KASTELLAN_EGRESS_PROXY_ALLOWLIST unset"))?;
    let worker = std::env::var("KASTELLAN_EGRESS_PROXY_WORKER").unwrap_or_else(|_| "unknown".into());
    // No-MITM mode: a worker that does end-to-end TLS itself and cannot trust
    // our per-instance CA (the browser, egress slice #2) sets this so the proxy
    // transparently tunnels instead of intercepting. Allowlist + SSRF still apply.
    // Strict on the value: only `"1"` (trimmed) counts, so this safety-relevant
    // mode is never enabled by an accidental/ambiguous spelling.
    let disable_mitm = matches!(
        std::env::var("KASTELLAN_EGRESS_PROXY_DISABLE_MITM")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1")
    );
    // Parse `host[:port]` endpoint entries so the boundary check is port-scoped
    // (#241), not host-only.
    let entries: Vec<String> = serde_json::from_str(&allow_json)
        .map_err(|e| anyhow::anyhow!("KASTELLAN_EGRESS_PROXY_ALLOWLIST is not a JSON array: {e}"))?;
    let allow = HostAllowlist::from_endpoints(&entries);

    // Bind the UDS *before* lock-down (Landlock will forbid fs mutation after).
    let _ = std::fs::remove_file(&uds);
    let listener = UnixListener::bind(&uds)?;

    // Generate the per-instance CA and export ONLY its public cert next to the
    // UDS, before lock-down. The host waits for this file and injects it into
    // the worker's trust store.
    let ca = std::sync::Arc::new(ca::generate_ca().map_err(|e| anyhow::anyhow!("generate CA: {e}"))?);
    let ca_path = std::path::Path::new(&uds)
        .parent()
        .ok_or_else(|| anyhow::anyhow!("UDS path has no parent dir"))?
        .join("ca.pem");
    std::fs::write(&ca_path, ca.cert_pem())
        .map_err(|e| anyhow::anyhow!("write CA cert {ca_path:?}: {e}"))?;

    // The host provisions the credential-leak scanner's fingerprints here (slice
    // #3b), a sibling of the UDS + CA. Derived once from the same already-
    // validated parent dir so the per-connection accept path never re-derives it
    // (and never panics) after lock-down.
    let secret_hashes_path = std::path::Path::new(&uds)
        .parent()
        .ok_or_else(|| anyhow::anyhow!("UDS path has no parent dir"))?
        .join("secret_hashes.json");

    // Upstream trust for the re-origination leg: the REAL public roots, plus an
    // optional operator-provided SPKI pin overlay (slice #4) AND an optional
    // operator-provided extra CA (#491, for a self-signed private origin like a
    // personal localmail). Both are off by default; a malformed value aborts
    // startup (fail-closed) rather than silently disabling protection.
    let upstream_extra_ca = std::env::var("KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA").ok();
    if let Some(ref p) = upstream_extra_ca {
        eprintln!(
            "[egress-proxy] WARN: trusting operator-provided upstream extra CA {p:?} on the \
             re-origination leg (widens upstream trust beyond webpki roots, for EVERY host \
             this sidecar may reach). The cert must be a real CA that signed the origin's \
             leaf, or a self-signed leaf with basicConstraints CA:FALSE — a CA:TRUE \
             self-signed leaf is rejected at handshake time (CaUsedAsEndEntity) and shows \
             up as a `mitm_failed: …` decision, not as a startup error."
        );
    }
    let upstream_tls = pins::build_upstream_client_config(
        std::env::var("KASTELLAN_EGRESS_PROXY_PINS").ok().as_deref(),
        upstream_extra_ca.as_deref().map(std::path::Path::new),
    )
    .map_err(|e| anyhow::anyhow!("build upstream TLS config: {e}"))?;

    // Worker-side defense-in-depth (Linux Landlock+seccomp; no-op on macOS,
    // where the parent Seatbelt profile contains us). Outbound socket(2) +
    // AF_UNIX accept must remain permitted — see the net_client profile.
    // NOTE (Linux verification, run on the DGX — tracked in #243): confirm the
    // seccomp profile permits AF_UNIX bind/listen/accept *and* AF_INET connect
    // for a process that both serves and dials; widen `seccomp_lock` if `accept`
    // is refused.
    let _report = kastellan_worker_prelude::lock_down()?;

    let resolver = StdResolve;
    // One leaf cache for the life of the proxy: connections are handled serially
    // (`thread::scope` joins each before the next `incoming()`), so a single
    // `&mut` borrow into each scope is sound, and repeat CONNECTs to the same
    // host across separate connections reuse the issued leaf.
    let mut cache = leaf_cache::LeafCache::new();
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let allow = &allow;
        let worker = worker.clone();
        let ca = &ca;
        let upstream_tls = &upstream_tls;
        let cache = &mut cache;
        let secret_hashes_path = &secret_hashes_path;
        // One thread per connection; the proxy is SingleUse + short-lived.
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut reporter = LineReporter { out: std::io::stdout().lock() };
                let mut mitm = MitmCtx {
                    ca: ca.as_ref(),
                    leaf_cache: cache,
                    upstream_tls: std::sync::Arc::clone(upstream_tls),
                    secret_hashes_path: Some(secret_hashes_path.clone()),
                    disable_mitm,
                };
                handle_conn(conn, &worker, allow, &resolver, &mut reporter, &mut mitm);
            });
        });
    }
    Ok(())
}
