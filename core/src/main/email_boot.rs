//! Email channel bring-up for the `kastellan` binary entrypoint.
//!
//! Sibling of `matrix_boot.rs`, deliberately mirroring its structure —
//! backend selection, egress wiring, `ChannelBus::spawn` shape — with two
//! differences forced by the email channel's own design
//! (`docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`):
//!
//! 1. **The email CHANNEL refuses to start; the DAEMON does not.**
//!    [`kastellan_core::channel::email::config::EmailConfig::from_env`]
//!    refuses a PARTIAL config (`Err`, not `Ok(None)`) precisely so a missing
//!    `KASTELLAN_EMAIL_AUTHSERV_ID` etc. is loud instead of quietly
//!    rejecting every message and looking like a delivery bug — see that
//!    function's module docs. This function turns that `Err`, and a
//!    `spawn_email_worker` `Err`, into a **prominent `error!` plus `None`**:
//!    the email channel does not come up, everything else does.
//!
//!    That is a deliberate correction (final whole-branch review, Important
//!    5). An earlier version propagated both through `?`, aborting daemon
//!    startup. Three reasons that was wrong: (a) **availability inversion** —
//!    this channel exists *because* Matrix has no homeserver failover, so a
//!    typo in the fallback's config must not take the primary channel and the
//!    scheduler down with it; (b) **spec deviation** — design §6 says the
//!    daemon refuses to start *the email channel*, not the daemon; (c) the
//!    fail-closed argument did not actually apply — a half-configured channel
//!    already fails **closed** (a blank authserv-id makes
//!    `gate::trusted_dmarc_pass` reject every message), so there was no
//!    fail-open case the abort protected against. What the abort really
//!    bought was *loudness*, which an `error!` delivers without the
//!    collateral — and it additionally removed the last startup path that
//!    skipped `main.rs`'s graceful shutdown sequence (bus shutdowns →
//!    scheduler → audit mirror → `pool.close()`).
//!
//!    The posture DIFFERENCE from Matrix is real and kept: Matrix fails soft
//!    over an unreachable homeserver because that is a transient outage,
//!    whereas `email.init` does no network I/O at all (see
//!    `workers/email-in/src/handler.rs`), so any failure here is a
//!    deployment/config problem (bad worker_bin, sandbox refusal, egress
//!    misconfig) and is logged at `error!` accordingly, naming every missing
//!    variable so one restart is enough to fix it.
//! 2. **No microVM branch.** [`kastellan_core::channel::email::config::EmailConfig`]
//!    has no `use_microvm` field in this slice — the worker always runs on
//!    the host jail backend (bwrap on Linux, Seatbelt on macOS), which is
//!    also the sidecar backend (the same 5c invariant Matrix documents: the
//!    egress proxy needs a real network route; a VM sidecar would have none).
//!
//! The final `PgCompletedTasks::connect` step behaves the same way (matching
//! Matrix exactly) — a LISTEN/NOTIFY bring-up hiccup on an already-open pool
//! is a generic DB-listener concern, not an email-channel-specific
//! misconfiguration — so every arm of this module now converges on one
//! outcome: `None` plus a loud, unmistakable `error!` (see
//! `log_channel_disabled`). The single exception is an **unset**
//! `KASTELLAN_EMAIL_ENDPOINT`, which is silent by design: the channel is
//! simply not configured, which is the default and not a problem to report.

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


/// Loud, unmistakable log line for every "the email channel is NOT running"
/// outcome. Kept in one place so no failure arm can accidentally word it in a
/// way an operator could read as "the channel came up".
///
/// Deliberately shouty and explicit about the two facts that matter: the
/// channel is **off**, and the daemon is **fine**. `cause` carries the
/// underlying error, which for a config problem names every missing variable
/// (`email::config::parse_email_config`).
fn log_channel_disabled(cause: &anyhow::Error) {
    error!(
        error = %format!("{cause:#}"),
        "EMAIL CHANNEL DISABLED — it did NOT start and NO email will be received. \
         The rest of the daemon (Matrix, scheduler, tools) is running normally. \
         Fix what `error` names, then restart the daemon to enable the channel."
    );
}

/// Spawn the email channel bus if `KASTELLAN_EMAIL_ENDPOINT` is set.
///
/// Gated on [`kastellan_core::channel::email::config::EmailConfig::from_env`]:
/// unset ⇒ `None` with **no log noise at all** (the channel is simply absent
/// and the daemon is byte-identical to an email-less build). Fully configured
/// ⇒ the sandboxed worker is spawned (force-routed through a real 1:1
/// intercepting sidecar whenever `force_routing` is `Some` — the `Some`/`None`
/// branching mirrors `matrix_boot::spawn_matrix_channel`'s `MatrixEgress`
/// wiring exactly, but the TLS posture does NOT: Matrix's sidecar stays a
/// transparent tunnel (matrix-sdk terminates its own TLS end-to-end), while
/// this one always intercepts (`Mitm::Intercept`) so an operator's
/// upstream-extra-CA anchor can reach a self-signed localmail — passing
/// `None` here would silently degrade the worker onto the HOST network
/// namespace, see [`kastellan_core::channel::email::EmailEgress`]'s docs) and
/// a real audit closure is wired so every skipped id lands in `audit_log`.
///
/// **Every failure arm returns `None` after a loud
/// [`log_channel_disabled`]** — a set-but-partial config, a worker spawn
/// failure, and a `PgCompletedTasks::connect` failure alike. None of them
/// aborts the daemon; see the module docs' point 1 for why that is the
/// correct posture for a *fallback* channel. There is no `Err` variant to
/// return, which is what makes it impossible for a future caller to
/// re-introduce the abort with a stray `?`.
///
/// * `pool` — daemon-scoped runtime pool (cloned into the authorizer, pairing
///   service, events, completion seams, and the skipped-id audit sink).
/// * `sandboxes` — the per-OS backend bundle; the email worker always runs on
///   the host jail (bwrap/Seatbelt) — there is no microVM option in this
///   slice.
/// * `force_routing` — the resolved egress force-routing config; `Some` ⇒
///   each (re)spawn gets a 1:1 intercepting sidecar via `EmailEgress` (see
///   that type's docs for why its TLS posture differs from Matrix's).
pub(crate) async fn spawn_email_channel(
    pool: &PgPool,
    sandboxes: &SandboxBackends,
    force_routing: &Option<Arc<ForceRoutingConfig>>,
) -> Option<ChannelBus> {
    let cfg = match kastellan_core::channel::email::config::EmailConfig::from_env() {
        // Unset ⇒ channel absent. Silent on purpose: this is the default for
        // every deployment that doesn't use the email fallback.
        Ok(None) => return None,
        Ok(Some(cfg)) => cfg,
        Err(e) => {
            log_channel_disabled(&e.context("email channel configuration is incomplete or invalid"));
            return None;
        }
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

    let spawned = match kastellan_core::channel::email::spawn_email_worker(
        backend,
        ChannelId("email".to_string()),
        &cfg,
        egress,
        audit_ack_only,
    ) {
        Ok(s) => s,
        Err(e) => {
            log_channel_disabled(&e.context("the email worker failed to start"));
            return None;
        }
    };

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
            Some(bus)
        }
        Err(e) => {
            log_channel_disabled(&e.context("PgCompletedTasks::connect (LISTEN/NOTIFY) failed"));
            None
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
