//! The daemon's startup notice when egress force-routing is OFF.
//!
//! `main.rs` logged an `info!` when force-routing was ON and said **nothing at
//! all** when it was off — the `if let Some(..)` had no `else`. With it off,
//! host workers fall back to `--share-net` with only the in-worker allowlist,
//! and no line, row or metric records that.
//!
//! That silence matters more than it looks. The unit sets
//! `Environment=KASTELLAN_EGRESS_FORCE_ROUTING=1`, but systemd applies
//! `EnvironmentFile=` **after** `Environment=` (measured on a live user manager,
//! not assumed), so the env file the installer regenerates — and the operator
//! overlay beside it — can turn this off. A posture that an ordinary config file
//! can flip must announce itself.
//!
//! The actor is `daemon`, not `egress_proxy`: this is the daemon's own startup
//! posture, and attributing it to a proxy that by definition is not running
//! would be wrong.
//!
//! **Everything an operator reads lives here, not at the call site.** The first
//! cut kept only the grep token here and hand-typed the surrounding sentence in
//! `main.rs`, with a near-duplicate of it again in the audit payload — three
//! wordings of one claim, which is the drift shape #516/#524/#525 each cost a
//! review round. The message is now assembled from the same consts the payload
//! uses, and `main.rs` interpolates it whole.

/// Operator-facing phrase, grep-able in `~/.local/state/kastellan/*.out`.
pub const FORCE_ROUTING_DISABLED_LOG_PHRASE: &str = "EGRESS FORCE-ROUTING DISABLED";

/// The env var that controls the posture. Aliased from the module that actually
/// reads it, so the notice cannot name a variable the code no longer consults.
pub const ENV_VAR: &str = crate::worker_lifecycle::force_route::ENV_ENABLE;

/// What being off actually costs. Shared verbatim by the log line and the audit
/// row, so an operator reading either sees the same claim.
pub const CONSEQUENCE: &str = "Net::Allowlist workers spawn with a direct network route; \
                               only the in-worker allowlist applies, and no egress proxy \
                               enforces host:port or SSRF checks.";

/// Audit actor for daemon-level startup posture rows.
///
/// Named `DAEMON_ACTOR` rather than `ACTOR` because `egress::audit::ACTOR` is a
/// differently-valued `pub const ACTOR` one module away.
pub const DAEMON_ACTOR: &str = "daemon";

/// Audit action. Renaming is an audit-trail contract break.
pub const ACTION_FORCE_ROUTING_DISABLED: &str = "egress.force_routing_disabled";

/// The full line the operator reads at startup.
///
/// Pure and returned rather than logged, for the reason `render_drop_warning`
/// is: the point of this text is that it is SEEN, and a message with no test is
/// one refactor away from silently losing the half that says what to do.
pub fn force_routing_disabled_message() -> String {
    format!(
        "{FORCE_ROUTING_DISABLED_LOG_PHRASE} — {CONSEQUENCE} Set {ENV_VAR}=1 in \
         kastellan.env.local (see docs/deploy/operator-env.md for how to apply it on your \
         platform) unless this is a deliberate bring-up without the proxy."
    )
}

/// Payload for the `egress.force_routing_disabled` row.
///
/// Pure, so the wire shape is unit-testable without a live pool.
pub fn force_routing_disabled_payload() -> serde_json::Value {
    serde_json::json!({
        "phrase": FORCE_ROUTING_DISABLED_LOG_PHRASE,
        "env_var": ENV_VAR,
        "consequence": CONSEQUENCE,
    })
}

#[cfg(test)]
mod tests;
