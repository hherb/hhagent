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

/// Render the one `[SKIP] <reason>` line every helper in this crate prints.
///
/// Pure, and that is the point: a `[SKIP]` line is **evidence** in this tree —
/// `cargo test -- --nocapture | grep -c '^\[SKIP\]'` is how a run is audited
/// for tests that reported green without executing anything. A unit test that
/// wants to pin the rendering must therefore be able to do so **without
/// emitting a line**, or it inflates the very count it is checking. Assert on
/// this; call the `skip_if_*` wrappers only from real fixtures.
///
/// `reason` is flattened to a single line. Probe errors are not single-line
/// values — both supervisor backends embed a `\n\n` operator hint
/// (`supervisor/src/systemd_user.rs`, `launchd_agents.rs`), and on macOS
/// unconditionally — so without this every such skip would emit a `[SKIP]`
/// line plus orphan continuation lines, and under
/// [`crate::gliner_e2e::UnmetAction::Fail`] a multi-line panic message. The
/// grep count survives either way; what does not is being able to say the
/// reason *is* one line, which the `*_or_reason` docs all promise.
pub fn skip_line(reason: &str) -> String {
    format!("\n[SKIP] {}\n", one_line(reason))
}

/// Collapse a probe reason to a single line.
///
/// Shared by [`skip_line`] and by [`crate::gliner_e2e::report_unmet_to`]'s
/// panic arm, so a reason renders the same way whichever verdict a caller puts
/// on it — a `[SKIP]` line and a demanded-run panic should not disagree about
/// what the reason *is*.
pub fn one_line(reason: &str) -> String {
    reason.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Render a `[WARN] <reason>` line on the same stream as [`skip_line`].
///
/// The third state a preflight can be in. `[SKIP]` says a precondition is
/// unmet and the test did not run; a plain pass says it ran. Neither can say
/// *"it ran, but something about the run was not what you think"* — which is
/// exactly what an out-of-dialect `REQUIRE` flag and an unverifiable rootfs
/// image both are. Both were previously silent, and a silent caveat on a green
/// run is the false-green pattern one step removed.
///
/// Pure, and one renderer rather than the hand-written copy it replaces plus
/// the second copy this change would otherwise have added, for the same
/// reason [`skip_line`] is: `grep -c '^\[WARN\]'` over a run is evidence, so
/// the shape must not drift between callers. Flattened to a single line for
/// the same reason too.
pub fn warn_line(reason: &str) -> String {
    format!("\n[WARN] {}\n", one_line(reason))
}

/// How long to wait for a TCP connect when probing a real origin's reachability.
const ORIGIN_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Why the user-level supervisor is unusable on this host, or `None` when it
/// is fine. The string is a *reason*, with no `[SKIP]` prefix, so a caller that
/// must not skip (see [`crate::gliner_e2e`]) can render it as a failure
/// instead. It may span lines; [`skip_line`] flattens it.
///
/// Probe failures are normal on headless Linux without
/// `loginctl enable-linger`, and on SSH-only macOS sessions where
/// `gui/<uid>` is unreachable.
///
/// `SupervisorError::Probe` — the variant both backends' `probe()` returns on
/// an unreachable user manager — already renders as `supervisor probe failed:
/// …`, so prefixing it again produced `supervisor probe failed: supervisor
/// probe failed: …`. But `default_probe` can also surface `Io`,
/// `CommandFailed` or `NotImplemented`, and the last of those carries no
/// "supervisor" context at all, so the prefix is added only when it is missing
/// rather than dropped outright.
pub fn supervisor_unavailable_reason() -> Option<String> {
    default_probe().err().map(|e| prefix_supervisor_context(&e.to_string()))
}

/// Pure: give a supervisor error the `supervisor …` context it may already have.
fn prefix_supervisor_context(rendered: &str) -> String {
    if rendered.starts_with("supervisor ") {
        rendered.to_string()
    } else {
        format!("supervisor unavailable: {rendered}")
    }
}


/// Returns `true` if the user-level supervisor probe fails. Caller
/// should `return` immediately so the test body never runs.
///
/// The skip-as-pass half of [`supervisor_unavailable_reason`].
pub fn skip_if_no_supervisor() -> bool {
    match supervisor_unavailable_reason() {
        Some(reason) => {
            eprint!("{}", skip_line(&reason));
            true
        }
        None => false,
    }
}

/// Returns the discovered Postgres `bin/` directory, or the *reason* no known
/// PGDG / Homebrew layout was found on this host — no `[SKIP]` prefix, so a
/// caller that must not skip can render it as a failure.
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
            eprint!("{}", skip_line(&reason));
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
            eprint!(
                "{}",
                skip_line(&format!(
                    "cannot resolve {host}: {e} (this tier needs outbound HTTPS)"
                ))
            );
            return true;
        }
    };
    for addr in &addrs {
        if std::net::TcpStream::connect_timeout(addr, ORIGIN_PROBE_TIMEOUT).is_ok() {
            return false;
        }
    }
    eprint!(
        "{}",
        skip_line(&format!(
            "cannot reach {host}:443 (this tier needs outbound HTTPS)"
        ))
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `Probe` variant already names itself; prefixing again gave the
    /// operator `supervisor probe failed: supervisor probe failed: …`.
    #[test]
    fn an_already_contextual_supervisor_error_is_not_prefixed_twice() {
        assert_eq!(
            prefix_supervisor_context("supervisor probe failed: no bus"),
            "supervisor probe failed: no bus"
        );
        assert_eq!(
            prefix_supervisor_context("supervisor I/O error: broken pipe"),
            "supervisor I/O error: broken pipe"
        );
    }

    /// ...but a variant that does not name the subsystem still must, or the
    /// reason reads as an unattributed failure in a panic naming no component.
    #[test]
    fn a_context_free_supervisor_error_gets_the_prefix() {
        assert_eq!(
            prefix_supervisor_context("not yet implemented: default_probe"),
            "supervisor unavailable: not yet implemented: default_probe"
        );
    }

    /// `[WARN]` must be greppable on its own line and must NOT read as a
    /// skip: the two are counted separately when a run is audited, and a
    /// caveat that inflated the skip count would misattribute a test that
    /// actually ran.
    #[test]
    fn warn_line_is_greppable_and_is_not_a_skip() {
        let rendered = warn_line("image freshness could not be established");
        assert!(rendered.starts_with("\n[WARN] "), "must be its own line: {rendered:?}");
        assert!(rendered.ends_with('\n'), "must terminate the line: {rendered:?}");
        assert!(!rendered.contains("[SKIP]"), "a warning is not a skip: {rendered:?}");
        assert!(rendered.contains("freshness"), "must carry the reason: {rendered:?}");
    }

    /// Flattened for the same reason a `[SKIP]` reason is: an orphan
    /// continuation line cannot be attributed to the warning above it.
    #[test]
    fn warn_line_flattens_a_multi_line_reason() {
        assert_eq!(warn_line("two\n\n   lines"), "\n[WARN] two lines\n");
    }

    /// A reason is one line whatever the probe embedded in it.
    #[test]
    fn one_line_collapses_embedded_hints_and_indentation() {
        assert_eq!(
            one_line("failed: no bus\n\n   The per-user manager\n   is not running."),
            "failed: no bus The per-user manager is not running."
        );
        assert_eq!(one_line("already one line"), "already one line");
    }
}
