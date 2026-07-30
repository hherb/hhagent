//! The channel bus runtime: an inbound pump per channel (recv → classify →
//! audit + enqueue) and one outbound pump (completed-task NOTIFY → route → send).
//! All DB access is behind two seams so the pumps are testable without Postgres.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use kastellan_db::tasks::{self, Lane};

use super::auth::{AuthDecision, PeerAuthorizer};
use super::ingest::{screen_and_classify, InboundDecision};
use super::route::reply_for_completed_task;
use super::{actions, Channel, ChannelId, IncomingMessage, OutgoingMessage, PeerId};

/// Body of the reply sent back when a peer pairs successfully.
pub const PAIRED_ACK_BODY: &str = "\u{2713} Paired \u{2014} you can now message me.";

/// Inbound side-effects seam: enqueue a task + write audit rows. Real impl wraps
/// `kastellan_db::{tasks::insert_pending, audit::insert}`; the fake records calls.
#[async_trait::async_trait]
pub trait ChannelEvents: Send + Sync {
    /// Enqueue a channel task; returns its id.
    async fn enqueue(&self, lane: Lane, payload: Value) -> anyhow::Result<i64>;
    /// Best-effort audit row (never fatal; log on error).
    async fn audit(&self, action: &str, payload: Value);
}

/// Outbound source seam: a stream of completed task ids + a reader for the row.
#[async_trait::async_trait]
pub trait CompletedTasks: Send + Sync {
    /// Next completed task id, or `None` when the stream ends.
    async fn next_completed(&mut self) -> Option<i64>;
    /// Fetch `(payload, result)` for a task id, or `None` if absent.
    async fn load(&self, id: i64) -> anyhow::Result<Option<(Value, Option<Value>)>>;
}

/// Pairing carve-out seam: consulted **only** for authorizer-rejected peers, and
/// only ever compares the body against an operator-issued single-use code — never
/// interprets it, never reaches the agent. See the slice-#3 design's security
/// analysis. Real impl: `channel::pairing::DbPairingService`.
#[async_trait::async_trait]
pub trait PairingService: Send + Sync {
    async fn try_pair(&self, channel: &ChannelId, peer: &PeerId, body: &str) -> PairingOutcome;
}

/// Outcome of a pairing attempt by an unpaired peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingOutcome {
    /// The body matched an active code; the peer is now bound.
    Paired,
    /// No active code, or the body didn't match one — treat as a normal
    /// (dropped) unpaired message.
    NotAPairingAttempt,
}

/// Real DB-backed `ChannelEvents` over the runtime pool.
pub struct PgChannelEvents {
    pool: sqlx::PgPool,
}
impl PgChannelEvents {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}
#[async_trait::async_trait]
impl ChannelEvents for PgChannelEvents {
    async fn enqueue(&self, lane: Lane, payload: Value) -> anyhow::Result<i64> {
        Ok(tasks::insert_pending(&self.pool, lane, payload).await?)
    }
    async fn audit(&self, action: &str, payload: Value) {
        if let Err(e) = kastellan_db::audit::insert(&self.pool, "channel", action, payload).await {
            warn!(action, error = %e, "channel audit insert failed (non-fatal)");
        }
    }
}

/// Real `CompletedTasks` over a `PgListener` on `tasks_completed` + `tasks::get`.
/// Construct via [`PgCompletedTasks::connect`].
pub struct PgCompletedTasks {
    listener: sqlx::postgres::PgListener,
    pool: sqlx::PgPool,
}
impl PgCompletedTasks {
    pub async fn connect(pool: sqlx::PgPool) -> anyhow::Result<Self> {
        let mut listener = sqlx::postgres::PgListener::connect_with(&pool).await?;
        listener.listen("tasks_completed").await?;
        Ok(Self { listener, pool })
    }
}
#[async_trait::async_trait]
impl CompletedTasks for PgCompletedTasks {
    async fn next_completed(&mut self) -> Option<i64> {
        loop {
            match self.listener.recv().await {
                Ok(n) => {
                    if let Ok(id) = n.payload().parse::<i64>() {
                        return Some(id);
                    }
                }
                Err(e) => {
                    warn!(error = %e, "tasks_completed listener error; stopping outbound pump");
                    return None;
                }
            }
        }
    }
    async fn load(&self, id: i64) -> anyhow::Result<Option<(Value, Option<Value>)>> {
        Ok(tasks::get(&self.pool, id).await?.map(|t| (t.payload, t.result)))
    }
}

/// Handle one inbound message. Order is security-load-bearing:
///   1. **authorize** (`(channel, peer, evidence)`), yielding three distinct
///      outcomes:
///      - `RejectedUnauthentic(reason)` — the transport-supplied evidence
///        didn't check out (bad DMARC / missing-or-wrong token / a pairing
///        row with no token at all). Dropped + audited immediately, carrying
///        `reason`'s stable label so the four denial arms are tellable apart
///        in `audit_log`, and BEFORE and
///        WITHOUT the pairing carve-out: that carve-out compares unpaired
///        input against a live single-use code, and a transport that cannot
///        authenticate its sender must never get to attempt that;
///      - `Rejected` — no active pairing at all. The pairing carve-out
///        (compare-only) is consulted **only when `msg.evidence.is_none()`**
///        — i.e. only for a transport that authenticates its own peers
///        (Matrix). A transport that hands us `evidence` (email) is, by
///        construction, one that cannot vouch for its sender; letting an
///        unpaired evidence-bearing peer probe the carve-out would turn an
///        operator-issued single-use code into a brute-force target reachable
///        over a spoofable transport — email pairing is meant to be
///        operator-only. So an evidence-bearing `Rejected` skips straight to
///        the drop + audit, same as if no `PairingService` were configured;
///      - `Recognised` — proceed to step 2.
///   2. **screen** (injection guard) and enqueue or block.
///
/// Returns `Some(ack)` only on a successful pairing (the per-channel task delivers
/// it via the same channel).
pub async fn handle_inbound(
    authorizer: &dyn PeerAuthorizer,
    pairing: Option<&dyn PairingService>,
    events: &dyn ChannelEvents,
    msg: &IncomingMessage,
) -> Option<OutgoingMessage> {
    match authorizer.authorize(&msg.channel, &msg.peer, msg.evidence.as_ref()).await {
        AuthDecision::Recognised => {}
        AuthDecision::RejectedUnauthentic(reason) => {
            // Deliberately BEFORE and WITHOUT the pairing carve-out: the carve-out
            // compares unpaired input against a live code, and a transport that
            // cannot authenticate its sender must not get to attempt that.
            // Payload carries the channel + peer + the reason CODE only — never
            // the body, never the token, never a header. `reason` is one of a
            // fixed set of `[a-z_]` labels (`UnauthenticReason::as_str`), which
            // is what makes it safe to persist: without it every denial arm
            // (DMARC fail, no token, wrong token, token-less pairing) writes a
            // byte-identical row, and a wrong `KASTELLAN_EMAIL_AUTHSERV_ID` —
            // the single most likely misconfiguration here, which rejects
            // EVERY message — is indistinguishable from a token typo.
            events
                .audit(
                    actions::REJECTED_UNAUTHENTIC,
                    serde_json::json!({
                        "channel": msg.channel.0,
                        "peer": msg.peer.0,
                        "reason": reason.as_str(),
                    }),
                )
                .await;
            return None;
        }
        AuthDecision::Rejected => {
            // Pairing carve-out: the ONLY place unpaired input is touched, and
            // only ever compared against an operator-issued code (never
            // enqueued/echoed) — and ONLY for a transport that authenticates
            // its own peers (`evidence.is_none()`, e.g. Matrix). A transport
            // that supplies evidence (email) cannot vouch for its sender, so
            // an unpaired peer on it must not get a shot at the carve-out
            // either — same reasoning as `RejectedUnauthentic` above, just
            // for "no pairing at all" instead of "pairing but bad evidence".
            if msg.evidence.is_none() {
                if let Some(p) = pairing {
                    if p.try_pair(&msg.channel, &msg.peer, &msg.body).await == PairingOutcome::Paired {
                        events
                            .audit(
                                actions::PAIRED,
                                serde_json::json!({"channel": msg.channel.0, "peer": msg.peer.0}),
                            )
                            .await;
                        return Some(OutgoingMessage {
                            channel: msg.channel.clone(),
                            peer: msg.peer.clone(),
                            conversation: msg.conversation.clone(),
                            body: PAIRED_ACK_BODY.to_string(),
                        });
                    }
                }
            }
            events
                .audit(
                    actions::REJECTED_UNPAIRED,
                    serde_json::json!({"channel": msg.channel.0, "peer": msg.peer.0}),
                )
                .await;
            return None;
        }
    }

    match screen_and_classify(msg) {
        InboundDecision::Enqueue { payload } => match events.enqueue(Lane::Fast, payload).await {
            Ok(id) => {
                events
                    .audit(
                        actions::RECEIVED,
                        serde_json::json!({
                            "task_id": id, "channel": msg.channel.0,
                            "peer": msg.peer.0, "conversation": msg.conversation.0,
                        }),
                    )
                    .await;
            }
            Err(e) => warn!(error = %e, "channel enqueue failed; message dropped"),
        },
        InboundDecision::InjectionBlocked { sha256, reason_codes, score } => {
            events
                .audit(
                    actions::INJECTION_BLOCKED,
                    serde_json::json!({
                        "channel": msg.channel.0, "peer": msg.peer.0,
                        "sha256": sha256, "reason_codes": reason_codes, "score": score,
                    }),
                )
                .await;
        }
    }
    None
}

/// Handle one completed-task id on the outbound side: load it, route it (pure),
/// and `send` via the matching channel. `senders` maps `ChannelId` → an outbound
/// `send` handle. Returns the `OutgoingMessage` actually sent (for tests).
pub async fn handle_completed(
    completed: &dyn CompletedTasks,
    events: &dyn ChannelEvents,
    senders: &HashMap<ChannelId, mpsc::Sender<OutgoingMessage>>,
    id: i64,
) -> Option<OutgoingMessage> {
    let (payload, result) = match completed.load(id).await {
        Ok(Some(pr)) => pr,
        Ok(None) => return None, // rolled back between NOTIFY and SELECT — benign
        Err(e) => {
            warn!(task_id = id, error = %e, "outbound load failed");
            return None;
        }
    };
    let out = reply_for_completed_task(&payload, result.as_ref())?;
    let Some(tx) = senders.get(&out.channel) else {
        // NOT a warning: the daemon runs one `ChannelBus` per channel family
        // (`main.rs` spawns a Matrix bus and an email bus), and every bus's
        // completed-task pump sees EVERY completed channel task via
        // LISTEN/NOTIFY. So "this reply isn't for a channel I serve" is the
        // normal case on each reply — the other bus is handling it — and
        // logging it at `warn` fired a misleading "dropping" line for every
        // successfully delivered reply, which is exactly how an operator learns
        // to ignore warnings (review finding). Unifying the buses would remove
        // the ambiguity outright and let this go back to being a real `warn!`
        // (it would then mean "nothing serves this channel at all") — #497.
        debug!(channel = %out.channel.0, "reply is for a channel this bus does not serve; ignoring");
        return None;
    };
    if let Err(e) = tx.send(out.clone()).await {
        warn!(error = %e, "outbound send queue closed; reply dropped");
        return None;
    }
    events
        .audit(
            actions::REPLIED,
            serde_json::json!({"task_id": id, "channel": out.channel.0, "peer": out.peer.0}),
        )
        .await;
    Some(out)
}

/// A running bus. Owns the spawned pump tasks; `shutdown()` aborts them.
pub struct ChannelBus {
    handles: Vec<JoinHandle<()>>,
}

impl ChannelBus {
    /// Spawn one inbound/outbound pump per channel + one completed-task pump. Each
    /// per-channel task owns its `Channel` and `select!`s `recv()` (inbound)
    /// against an mpsc bridge carrying replies (outbound `send`), so the single
    /// `&mut Channel` owner does both and there is no cross-task contention.
    pub fn spawn(
        channels: Vec<Box<dyn Channel>>,
        authorizer: Arc<dyn PeerAuthorizer>,
        pairing: Option<Arc<dyn PairingService>>,
        events: Arc<dyn ChannelEvents>,
        mut completed: Box<dyn CompletedTasks>,
    ) -> Self {
        let mut handles = Vec::new();
        let mut senders: HashMap<ChannelId, mpsc::Sender<OutgoingMessage>> = HashMap::new();

        for mut ch in channels {
            let id = ch.id();
            let (tx, mut rx) = mpsc::channel::<OutgoingMessage>(32);
            senders.insert(id.clone(), tx);

            let authorizer = authorizer.clone();
            let pairing = pairing.clone();
            let events = events.clone();
            handles.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        inbound = ch.recv() => match inbound {
                            Some(msg) => {
                                if let Some(ack) =
                                    handle_inbound(&*authorizer, pairing.as_deref(), &*events, &msg).await
                                {
                                    if let Err(e) = ch.send(ack).await {
                                        warn!(channel = %id.0, error = %e, "pairing ack send failed");
                                    }
                                }
                            }
                            None => { info!(channel = %id.0, "inbound closed"); break; }
                        },
                        Some(out) = rx.recv() => {
                            // `handle_completed` already wrote `channel.replied`
                            // when it queued this — that row means "routed",
                            // not "delivered" (see `actions::REPLIED`). The
                            // actual transport attempt is HERE, so a failure
                            // must leave its own durable trace, or the audit
                            // trail asserts a delivery that never happened. In
                            // slice 1 `EmailChannel::send` always fails, so
                            // without this pair every email answer looked
                            // delivered. Payload is channel + peer only: the
                            // error is transport text, not a fixed label, and
                            // the body must never be persisted.
                            let peer = out.peer.clone();
                            if let Err(e) = ch.send(out).await {
                                warn!(channel = %id.0, error = %e, "channel send failed");
                                events
                                    .audit(
                                        actions::REPLY_UNDELIVERED,
                                        serde_json::json!({
                                            "channel": id.0, "peer": peer.0,
                                        }),
                                    )
                                    .await;
                            }
                        }
                    }
                }
            }));
        }

        // Outbound pump: NOTIFY → load → route → push into the per-channel sender.
        let events_out = events.clone();
        handles.push(tokio::spawn(async move {
            while let Some(id) = completed.next_completed().await {
                handle_completed(&*completed, &*events_out, &senders, id).await;
            }
            info!("outbound pump stopped");
        }));

        Self { handles }
    }

    /// Abort all pump tasks (called on daemon shutdown), then join them so any
    /// resources they hold are released before the caller proceeds. The
    /// completed-task pump owns a `PgListener`, which holds a checked-out pool
    /// connection that sqlx 0.9 only releases when the listener is dropped; the
    /// daemon's shutdown closes the pool right after, and `Pool::close()` blocks
    /// until every connection is returned. Aborting without joining would leave
    /// that release racing the pool close. Mirrors the scheduler/audit-mirror
    /// shutdowns, which signal-then-join for the same reason.
    pub async fn shutdown(self) {
        for h in &self.handles {
            h.abort();
        }
        for h in self.handles {
            let _ = h.await;
        }
    }
}

#[cfg(test)]
mod tests;
