//! Email channel bring-up for the `kastellan` binary entrypoint.
//!
//! Sibling of `matrix_boot.rs`, deliberately mirroring its structure —
//! backend selection, egress wiring, `ChannelBus::spawn` shape — with two
//! differences forced by the email channel's own design
//! (`docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`):
//!
//! 1. **Fail-closed on misconfiguration or spawn failure, not fail-soft.**
//!    [`kastellan_core::channel::email::config::EmailConfig::from_env`]
//!    already refuses a PARTIAL config (`Err`, not `Ok(None)`) precisely so
//!    a missing `KASTELLAN_EMAIL_AUTHSERV_ID` etc. aborts startup instead of
//!    quietly rejecting every message and looking like a delivery bug — see
//!    that function's module docs. This function preserves that posture end
//!    to end: both a config `Err` and a `spawn_email_worker` `Err` propagate
//!    out through `?` and abort daemon bring-up, unlike Matrix's
//!    unreachable-homeserver case (logs and returns `None`). `email.init`
//!    does no network I/O at all (see `workers/email-in/src/handler.rs`), so
//!    unlike Matrix's login there is no "the remote service happens to be
//!    down right now" case to fail-soft over — a spawn failure here means a
//!    deployment/config problem (bad worker_bin, sandbox refusal, egress
//!    misconfig), not a transient outage.
//! 2. **No microVM branch.** [`kastellan_core::channel::email::config::EmailConfig`]
//!    has no `use_microvm` field in this slice — the worker always runs on
//!    the host jail backend (bwrap on Linux, Seatbelt on macOS), which is
//!    also the sidecar backend (the same 5c invariant Matrix documents: the
//!    egress proxy needs a real network route; a VM sidecar would have none).
//!
//! The final `PgCompletedTasks::connect` step stays fail-soft (`Ok(None)`,
//! matching Matrix exactly) — a LISTEN/NOTIFY bring-up hiccup on an
//! already-open pool is a generic DB-listener concern, not an
//! email-channel-specific misconfiguration.

use std::sync::Arc;

use sqlx::PgPool;
use tracing::{error, info};

use kastellan_core::channel::polled_driver::AckOnlyAudit;
use kastellan_core::channel::{ChannelBus, ChannelId};
use kastellan_core::worker_lifecycle::force_route::ForceRoutingConfig;
use kastellan_sandbox::{SandboxBackend, SandboxBackends};

/// Cap on `reason`'s length before it becomes a durable `audit_log` payload
/// value. `reason` originates from the worker's `skipped[].reason`
/// (`workers/email-in/src/handler.rs::describe_email_error`), which already
/// truncates an upstream (localmail) HTTP error body to 200 chars — but
/// `polled_driver::run`, which hands it to this sink, is DB-free by design
/// and applies no cap of its own. Defence in depth: this sink must not trust
/// a future worker change (or a compromised worker) to keep bounding it
/// before it lands permanently in `audit_log`. Comfortably above the
/// worker's own 200-char cap so today's values pass through untouched.
const AUDIT_REASON_CAP_CHARS: usize = 256;

/// Truncate `reason` to [`AUDIT_REASON_CAP_CHARS`] on a `char` boundary
/// (never mid-UTF-8-codepoint — `reason` may echo arbitrary upstream text).
/// A no-op for anything at or under the cap, which covers every value the
/// worker emits today.
fn cap_reason(reason: &str) -> String {
    if reason.chars().count() <= AUDIT_REASON_CAP_CHARS {
        reason.to_string()
    } else {
        let mut capped: String = reason.chars().take(AUDIT_REASON_CAP_CHARS).collect();
        capped.push_str("...(truncated)");
        capped
    }
}

/// Build the [`AckOnlyAudit`] closure `spawn_email_worker` invokes for every
/// message id it acks without that id ever becoming a bus event (localmail's
/// `skipped` list — an unattributable `From`, an unfetchable detail fetch,
/// etc.). Those ids are messages the agent silently never saw, so they must
/// stay traceable in `audit_log` even though the polled driver that acks
/// them is DB-free by design.
///
/// Payload carries the message id and the reason ONLY — never a body, never
/// headers — and `reason` is capped via [`cap_reason`] before it is written
/// (see that function's docs for why this sink applies its own bound rather
/// than trusting the worker's). Shape otherwise mirrors
/// `crate::egress::net_worker::pg_decision_sink` exactly: capture a cloned
/// `PgPool` + `tokio::runtime::Handle`, then `block_on` the async insert from
/// inside a closure that itself is called synchronously from
/// `polled_driver::run`'s background thread (not a tokio task), so
/// `Handle::block_on` — rather than `.await` — is the only way in.
fn email_skipped_audit_sink(pool: PgPool, handle: tokio::runtime::Handle) -> AckOnlyAudit {
    Box::new(move |message_id: &str, reason: &str| {
        let payload = serde_json::json!({
            "channel": "email",
            "message_id": message_id,
            "reason": cap_reason(reason),
        });
        let res = handle.block_on(kastellan_db::audit::insert(
            &pool,
            "channel",
            kastellan_core::channel::actions::SKIPPED_ACK_ONLY,
            payload,
        ));
        if let Err(e) = res {
            error!(error = %e, message_id, "email: skipped-id audit insert failed (non-fatal)");
        }
    })
}


/// Spawn the email channel bus if `KASTELLAN_EMAIL_ENDPOINT` is set.
///
/// Gated on [`kastellan_core::channel::email::config::EmailConfig::from_env`]:
/// unset ⇒ `Ok(None)` and the daemon is byte-identical to an email-less
/// build. Set-but-partial ⇒ `Err`, which the caller should propagate to abort
/// startup (see the module docs above for why). Fully configured ⇒ the
/// sandboxed worker is spawned (force-routed through a real 1:1
/// transparent-tunnel sidecar whenever `force_routing` is `Some`, exactly
/// mirroring `matrix_boot::spawn_matrix_channel`'s `MatrixEgress` wiring —
/// passing `None` here would silently degrade the worker onto the HOST
/// network namespace, see [`kastellan_core::channel::email::EmailEgress`]'s
/// docs) and a real audit closure is wired so every skipped id lands in
/// `audit_log`. A worker spawn failure also aborts startup (`Err`) — see the
/// module docs' point 1.
///
/// * `pool` — daemon-scoped runtime pool (cloned into the authorizer, pairing
///   service, events, completion seams, and the skipped-id audit sink).
/// * `sandboxes` — the per-OS backend bundle; the email worker always runs on
///   the host jail (bwrap/Seatbelt) — there is no microVM option in this
///   slice.
/// * `force_routing` — the resolved egress force-routing config; `Some` ⇒
///   each (re)spawn gets a 1:1 transparent-tunnel sidecar via `EmailEgress`.
pub(crate) async fn spawn_email_channel(
    pool: &PgPool,
    sandboxes: &SandboxBackends,
    force_routing: &Option<Arc<ForceRoutingConfig>>,
) -> anyhow::Result<Option<ChannelBus>> {
    let Some(cfg) = kastellan_core::channel::email::config::EmailConfig::from_env()
        .map_err(|e| anyhow::anyhow!("email channel misconfigured: {e}"))?
    else {
        return Ok(None);
    };

    // Worker backend: always the host jail — no microVM option for this
    // worker in this slice. SIDECAR backend always stays the host
    // bwrap/Seatbelt too (5c invariant — the egress proxy needs a real
    // network route; a VM here would boot a proxy with none), same as Matrix.
    #[cfg(target_os = "linux")]
    let backend: Arc<dyn SandboxBackend> = Arc::clone(&sandboxes.bwrap);
    #[cfg(target_os = "linux")]
    let sidecar_backend: Arc<dyn SandboxBackend> = Arc::clone(&sandboxes.bwrap);
    #[cfg(target_os = "macos")]
    let backend: Arc<dyn SandboxBackend> = Arc::clone(&sandboxes.seatbelt);
    #[cfg(target_os = "macos")]
    let sidecar_backend: Arc<dyn SandboxBackend> = Arc::clone(&sandboxes.seatbelt);

    let egress = force_routing.as_ref().map(|fr| kastellan_core::channel::email::EmailEgress {
        sidecar_backend: Arc::clone(&sidecar_backend),
        routing: Arc::clone(fr),
    });

    let audit_ack_only =
        Some(email_skipped_audit_sink(pool.clone(), tokio::runtime::Handle::current()));

    let spawned = kastellan_core::channel::email::spawn_email_worker(
        backend,
        ChannelId("email".to_string()),
        &cfg,
        egress,
        audit_ack_only,
    )
    .map_err(|e| anyhow::anyhow!("email worker failed to start: {e:#}"))?;

    info!(identity = %spawned.identity, "email worker started; starting channel bus");
    let authorizer =
        Arc::new(kastellan_core::channel::auth::DbPeerAuthorizer::new(pool.clone()));
    let pairing =
        Arc::new(kastellan_core::channel::pairing::DbPairingService::new(pool.clone()));
    let events = Arc::new(kastellan_core::channel::bus::PgChannelEvents::new(pool.clone()));
    match kastellan_core::channel::bus::PgCompletedTasks::connect(pool.clone()).await {
        Ok(completed) => {
            let bus = ChannelBus::spawn(
                vec![Box::new(spawned.channel)],
                authorizer,
                Some(pairing),
                events,
                Box::new(completed),
            );
            info!("email channel bus running");
            Ok(Some(bus))
        }
        Err(e) => {
            error!(error = %e, "email: PgCompletedTasks::connect failed; channel not started");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_reason_passes_short_input_through_unchanged() {
        assert_eq!(cap_reason("no usable From address"), "no usable From address");
    }

    #[test]
    fn cap_reason_passes_input_at_exactly_the_cap_through_unchanged() {
        let at_cap = "a".repeat(AUDIT_REASON_CAP_CHARS);
        assert_eq!(cap_reason(&at_cap), at_cap);
    }

    #[test]
    fn cap_reason_truncates_an_oversized_upstream_error_body() {
        // Simulates `describe_email_error`'s `localmail {status}: {body}` shape
        // with a body well past its own 200-char worker-side cap — this sink
        // must not rely on that cap holding.
        let huge = format!("localmail 500: {}", "x".repeat(5_000));
        let capped = cap_reason(&huge);
        assert!(capped.chars().count() <= AUDIT_REASON_CAP_CHARS + "...(truncated)".len());
        assert!(capped.ends_with("...(truncated)"), "{capped}");
        assert!(huge.len() > capped.len(), "must actually shrink an oversized reason");
    }

    #[test]
    fn cap_reason_truncates_on_a_char_boundary_not_mid_utf8_codepoint() {
        // Multi-byte chars (e.g. from a non-ASCII upstream error message)
        // straddling the cap must not panic or produce an invalid `String`.
        let multibyte = "€".repeat(AUDIT_REASON_CAP_CHARS + 10);
        let capped = cap_reason(&multibyte); // would panic on a mid-codepoint byte slice
        assert!(capped.starts_with('€'));
    }
}
