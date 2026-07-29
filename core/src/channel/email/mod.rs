//! Email fallback channel (Phase 2, slice #5). Inbound only in this slice.
//!
//! Design: `docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`.
//!
//! Email cannot authenticate its own senders the way Matrix can (E2E +
//! homeserver auth), so this module supplies the evidence the bus needs to
//! decide: a DMARC verdict from our own MX, and a per-pairing shared token.
//! Both are computed by pure functions in [`gate`] — in core, not in the
//! worker, so every rejection still lands in `audit_log`.
//!
//! ## Layout
//!
//! Mirrors `channel/matrix.rs`'s split (each file under the project's
//! 500-LOC guideline):
//! - [`gate`] — the pure DMARC + token checks (unchanged by this module).
//! - [`wire`] — the polled-driver spec + the pure poll/send/ack codecs that
//!   turn `email-in`'s raw material into [`super::PeerEvidence`].
//! - [`policy`] — the pure [`SandboxPolicy`] builder for the email-in worker.
//! - [`config`] — the env-gated [`config::EmailConfig`] parsing.
//! - this parent — [`EmailChannel`] (the [`super::Channel`] impl) and
//!   [`spawn_email_worker`] (the orchestration that wires the two above +
//!   [`PolledWorkerDriver`] together).
//!
//! [`SandboxPolicy`]: kastellan_sandbox::SandboxPolicy

use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tokio::sync::mpsc as tok_mpsc;

use kastellan_sandbox::SandboxBackend;

use crate::channel::polled_driver::PolledWorkerDriver;
use crate::worker_lifecycle::persistent::{
    ClientTransport, PersistentFactory, PersistentTransport, PersistentWorker,
};
use crate::worker_lifecycle::RestartBackoff;

use super::{Channel, ChannelId, IncomingMessage, OutgoingMessage};

mod authres_parse;
pub mod config;
pub mod gate;
pub mod policy;
pub mod wire;

use config::EmailConfig;

/// A live email channel: owns the driver thread; implements the [`Channel`]
/// trait the [`super::bus::ChannelBus`] consumes. Structurally identical to
/// `matrix::MatrixChannel` — see that type's docs for why dropping a
/// [`thread::JoinHandle`] here is fine (it detaches, doesn't join; the driver
/// thread exits on its own once both channel endpoints are dropped).
pub struct EmailChannel {
    id: ChannelId,
    inbound_rx: tok_mpsc::Receiver<IncomingMessage>,
    // Never sent through in slice 1 (`send` always bails — no outbound
    // worker yet), but must stay owned here regardless: dropping BOTH driver
    // endpoints together is what stops the driver thread (see
    // `polled_driver::run`'s step 1 `Disconnected` arm), so keeping this
    // field alive for RAII is the point even though nothing ever calls
    // `.send()` on it. Slice 2 starts reading it.
    _outbound_tx: std_mpsc::Sender<OutgoingMessage>,
    // Kept for ownership clarity only (dropping a JoinHandle detaches, it
    // does not join) — see `matrix::MatrixChannel`'s identical field.
    _driver: thread::JoinHandle<()>,
}

impl EmailChannel {
    /// Wrap a running [`PolledWorkerDriver`]'s endpoints as the bus-facing
    /// [`Channel`]. The driver (and the supervisor + worker under it) shuts
    /// down via RAII when this channel is dropped.
    pub fn from_driver(id: ChannelId, driver: PolledWorkerDriver) -> Self {
        let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
        Self { id, inbound_rx, _outbound_tx: outbound_tx, _driver: join }
    }
}

#[async_trait::async_trait]
impl Channel for EmailChannel {
    fn id(&self) -> ChannelId {
        self.id.clone()
    }

    async fn recv(&mut self) -> Option<IncomingMessage> {
        // Cancellation-safe: a dropped `recv()` future (the bus `select!`
        // losing the race to an outbound) leaves any buffered event in the
        // channel for the next call — same contract as MatrixChannel::recv.
        self.inbound_rx.recv().await
    }

    /// Slice 1 has no outbound worker (design §8: "Slice 2 — outbound").
    /// Always errors; the bus logs + audits the failure, same as any other
    /// delivery failure, rather than silently dropping the reply.
    async fn send(&self, _msg: OutgoingMessage) -> anyhow::Result<()> {
        anyhow::bail!("email outbound not configured (slice 2)")
    }
}

/// A spawned live email worker: the [`Channel`] for the bus plus the
/// identity `email.init` reported (configured address + subscription name).
pub struct SpawnedEmailWorker {
    pub channel: EmailChannel,
    pub identity: serde_json::Value,
}

/// Email respawn backoff: 1s → 30s doubling, the same envelope
/// `matrix::matrix_backoff` uses — no reason for the two long-lived polled
/// channels to behave differently here.
fn email_backoff() -> RestartBackoff {
    RestartBackoff {
        base: Duration::from_secs(1),
        factor_num: 2,
        factor_den: 1,
        cap: Duration::from_secs(30),
    }
}

/// Bring up the sandboxed email-in worker: build the [`SandboxPolicy`]
/// (`Net::Allowlist` scoped to the localmail endpoint), spawn it (via
/// [`PersistentWorker`], respawning on death with capped backoff), and block
/// on `email.init` so the returned worker is confirmed live. `backend` is an
/// [`Arc`] so the respawn factory can outlive this call — mirrors
/// `matrix::spawn_matrix_worker`'s direct (non-VM, non-egress-sidecar) path;
/// egress force-routing for email is not wired in this slice.
///
/// Records `cfg.authserv_id` via [`wire::set_authserv_id`] *before* starting
/// the driver, so the very first `email.poll` result is parsed against the
/// correct trust root.
///
/// [`SandboxPolicy`]: kastellan_sandbox::SandboxPolicy
pub fn spawn_email_worker(
    backend: Arc<dyn SandboxBackend>,
    id: ChannelId,
    cfg: &EmailConfig,
) -> anyhow::Result<SpawnedEmailWorker> {
    let (host, port) = crate::channel::matrix::host_port_from_url(&cfg.endpoint)?;

    let mut policy =
        policy::build_email_policy(cfg.worker_bin.clone(), &host, port, cfg.token_file.clone());
    policy.env.push(("KASTELLAN_EMAIL_ENDPOINT".into(), cfg.endpoint.clone()));
    policy.env.push(("KASTELLAN_EMAIL_SUBSCRIPTION".into(), cfg.subscription.clone()));
    policy.env.push(("KASTELLAN_EMAIL_ADDRESS".into(), cfg.address.clone()));
    policy
        .env
        .push(("KASTELLAN_EMAIL_TOKEN_FILE".into(), cfg.token_file.display().to_string()));

    let program = cfg
        .worker_bin
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("worker bin path not UTF-8: {:?}", cfg.worker_bin))?
        .to_string();

    // PersistentFactory: each call brings up a fresh worker on the
    // SUPERVISOR's persistent thread (PDEATHSIG-safe, #348). No egress
    // sidecar branch (unlike matrix::spawn_matrix_worker) — force-routing
    // for email is a follow-up, not part of this slice.
    let factory: PersistentFactory = Box::new(move || {
        let t = ClientTransport::spawn(&*backend, &policy, &program, &[])?;
        Ok(Box::new(t) as Box<dyn PersistentTransport>)
    });

    // Must happen before PolledWorkerDriver::spawn: the driver thread starts
    // polling immediately, and parse_email_poll reads AUTHSERV_ID.
    wire::set_authserv_id(&cfg.authserv_id);

    let handle = PersistentWorker::spawn_with_backoff("email", factory, email_backoff())?;
    let (driver, identity) = PolledWorkerDriver::spawn(
        wire::EMAIL_POLLED_SPEC,
        Box::new(handle),
        wire::parse_email_poll,
        wire::encode_email_send,
        Some(wire::encode_email_ack),
        Some(wire::parse_email_skipped_ids),
        id.clone(),
    )?;
    Ok(SpawnedEmailWorker { channel: EmailChannel::from_driver(id, driver), identity })
}
