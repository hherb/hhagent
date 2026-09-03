//! `[SKIP]` early-return helpers.
//!
//! The pattern: print `[SKIP] <reason>` to stderr and return `true` (or
//! `None`) so the calling test can `return` immediately. The eprintln!
//! is load-bearing — a green CI run with `[SKIP]` lines means the test
//! never executed its assertions, not that containment held. Visible
//! only under `cargo test -- --nocapture`.

use std::path::PathBuf;
use std::time::Duration;

use kastellan_db::{find_pg_bin_dir, pg_bin_dir_candidates_with_env_override};
use kastellan_supervisor::default_probe;

/// How long to wait for a TCP connect when probing a real origin's reachability.
const ORIGIN_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Why the user-level supervisor is unusable on this host, or `None` when it
/// is fine. The string is a *reason*, with no `[SKIP]` prefix and no newlines,
/// so a caller that must not skip (see
/// [`crate::gliner_e2e`]) can render it as a failure instead.
///
/// Probe failures are normal on headless Linux without
/// `loginctl enable-linger`, and on SSH-only macOS sessions where
/// `gui/<uid>` is unreachable.
pub fn supervisor_unavailable_reason() -> Option<String> {
    default_probe().err().map(|e| format!("supervisor probe failed: {e}"))
}

/// Returns `true` if the user-level supervisor probe fails. Caller
/// should `return` immediately so the test body never runs.
///
/// The skip-as-pass half of [`supervisor_unavailable_reason`].
pub fn skip_if_no_supervisor() -> bool {
    match supervisor_unavailable_reason() {
        Some(reason) => {
            eprintln!("\n[SKIP] {reason}\n");
            true
        }
        None => false,
    }
}

/// Returns the discovered Postgres `bin/` directory, or the *reason* no known
/// PGDG / Homebrew layout was found on this host — no `[SKIP]` prefix, no
/// newlines, so a caller that must not skip can render it as a failure.
///
/// Honours the `KASTELLAN_PG_BIN_DIR` env var via
/// [`pg_bin_dir_candidates_with_env_override`] so operators running on
/// Postgres.app or any non-standard install can opt in by exporting the
/// bin-dir path; see that helper's doc-comment for semantics.
pub fn pg_bin_dir_or_reason() -> Result<PathBuf, String> {
    find_pg_bin_dir(&pg_bin_dir_candidates_with_env_override())
        .map_err(|e| format!("no Postgres install found: {e}"))
}

/// The skip-as-pass half of [`pg_bin_dir_or_reason`]: print `[SKIP] <reason>`
/// and return `None` so test runs stay auditable.
pub fn pg_bin_dir_or_skip() -> Option<PathBuf> {
    match pg_bin_dir_or_reason() {
        Ok(dir) => Some(dir),
        Err(reason) => {
            eprintln!("\n[SKIP] {reason}\n");
            None
        }
    }
}

/// Returns `true` if `host:443` is not reachable from this box, so the caller
/// should `return` immediately.
///
/// Some egress e2e tiers need a **real public HTTPS origin** and cannot be made
/// hermetic. Two independent reasons, both structural rather than laziness:
///
/// * A **transparent-tunnel** (no-MITM) worker such as browser-driver does its
///   own end-to-end TLS, so it must trust the origin's certificate on its own
///   root store — a self-signed loopback origin would need a CA installed in the
///   guest's trust store.
/// * A **MITM** worker such as web-fetch has the reverse problem one hop later:
///   the egress proxy re-originates the connection and validates the origin
///   against `webpki_roots` only (`egress-proxy`'s `build_upstream_client_config`
///   has no extra-root knob), so a self-signed loopback origin fails at the
///   proxy's upstream leg.
///
/// Widening either trust store to make a test pass would weaken production, so
/// these tiers take the real-network dependency instead — and skip cleanly when
/// the network is absent. The `[SKIP]` line is load-bearing: a silent skip is
/// exactly the false-green pattern `CLAUDE.md` warns about.
pub fn skip_if_origin_unreachable(host: &str) -> bool {
    use std::net::ToSocketAddrs;
    let addrs = match (host, 443u16).to_socket_addrs() {
        Ok(a) => a.collect::<Vec<_>>(),
        Err(e) => {
            eprintln!("\n[SKIP] cannot resolve {host}: {e} (this tier needs outbound HTTPS)\n");
            return true;
        }
    };
    for addr in &addrs {
        if std::net::TcpStream::connect_timeout(addr, ORIGIN_PROBE_TIMEOUT).is_ok() {
            return false;
        }
    }
    eprintln!("\n[SKIP] cannot reach {host}:443 (this tier needs outbound HTTPS)\n");
    true
}
