//! Supervised channel bring-up (#514).
//!
//! Bringing a channel up is not one operation but a short chain — spawn a
//! sandboxed worker (and, when egress is force-routed, its 1:1 sidecar), log
//! in, open a LISTEN/NOTIFY connection, start a [`ChannelBus`] — and every
//! link in it can fail transiently. Before this module each boot module tried
//! that chain exactly once and returned `None` on any failure, so a blip in
//! the first seconds of daemon startup left the bot deaf for the life of the
//! process, with every unit `active`, Postgres healthy, and nothing further in
//! the log *because there was nothing to log*: no channel, so no message ever
//! arrived. That is not hypothetical — it cost 12 hours of missed Matrix
//! messages on 2026-08-03, and the same log line appears on four earlier
//! dates.
//!
//! The fix is the shape already used one layer down, where
//! [`crate::worker_lifecycle::PersistentWorker`] supervises a worker *after*
//! login: retry with capped exponential backoff. Unbounded, because a
//! homeserver can be down for an hour and the daemon should reconnect when it
//! returns rather than need a human.
//!
//! Two things keep that from becoming a different failure:
//!
//! 1. **[`BootOutcome::Fatal`]** — a statically-dead configuration must stop,
//!    not spin. A `localhost`-name homeserver under force-routing (#459) and a
//!    partial `EmailConfig` are both fixed for the lifetime of the process, so
//!    retrying them would be exactly the respawn loop those checks exist to
//!    prevent.
//! 2. **[`DowntimeEscalator`]** — once the backoff caps out, an unescalated
//!    loop would emit one identical line per minute for as long as the outage
//!    lasts.
//!
//! The loop itself is database-free and network-free: audit rows go out
//! through a boxed closure ([`BootAuditSink`] — the idiom
//! [`crate::channel::polled_driver::AckOnlyAudit`] already uses), so the whole
//! module is testable with scripted outcomes and a probe channel. The
//! Postgres implementation lives in [`pg_sink`].

use std::future::Future;
use std::time::Instant;

use futures::future::BoxFuture;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::channel::audit_text::cap_chars;
use crate::channel::ChannelBus;
use crate::worker_lifecycle::RestartBackoff;

pub mod downtime;
pub mod pg_sink;

pub use downtime::DowntimeEscalator;

#[cfg(test)]
mod tests;

/// Cap on the `cause` string before it becomes a durable `audit_log` value.
/// See [`crate::channel::audit_text`] for why a sink bounds this itself rather
/// than trusting the value's producer.
pub const AUDIT_CAUSE_CAP_CHARS: usize = 256;

/// A running channel, plus the one thing the supervisor ever does to it: stop
/// it.
///
/// Deliberately opaque. The supervisor never names [`ChannelBus`], which keeps
/// the retry policy independent of the channel layer and lets a test hand it a
/// probe that records whether shutdown ran.
pub struct StartedChannel {
    /// Boxed because the supervisor stores one of these across an await and
    /// must not be generic over the channel type to do it. `FnOnce` because
    /// stopping twice is not a thing that should be expressible.
    shutdown: Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>,
}

impl StartedChannel {
    /// Wrap anything whose shutdown is an async, by-value call.
    pub fn new<F, Fut>(shutdown: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self { shutdown: Box::new(move || Box::pin(shutdown())) }
    }

    /// The production case: a running [`ChannelBus`].
    pub fn from_bus(bus: ChannelBus) -> Self {
        Self::new(move || async move { bus.shutdown().await })
    }

    /// Stop the channel. Consuming, so it cannot run twice.
    async fn stop(self) {
        (self.shutdown)().await;
    }
}

impl std::fmt::Debug for StartedChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The payload is a closure; there is nothing useful to render, and a
        // `Debug` bound is needed only so `BootOutcome` can derive it.
        f.write_str("StartedChannel")
    }
}

/// What one bring-up attempt produced.
///
/// The distinction that carries the weight is [`Retry`](Self::Retry) versus
/// [`Fatal`](Self::Fatal), and the question to ask is: *could a later attempt
/// succeed with the same process environment?* A refused sandbox cgroup, an
/// unreachable homeserver and a LISTEN/NOTIFY hiccup are all yes. A missing or
/// malformed environment variable is no — the environment cannot change under
/// a running daemon, so the honest response is a loud line telling the
/// operator to fix it and restart.
#[derive(Debug)]
pub enum BootOutcome {
    /// The channel is not configured. Stop, and say nothing: this is the
    /// default for every deployment that does not use this channel.
    NotConfigured,
    /// The channel is up.
    Started(StartedChannel),
    /// Failed in a way a later attempt could plausibly absorb.
    Retry(anyhow::Error),
    /// Failed in a way no retry can fix.
    Fatal(anyhow::Error),
}

/// One durable record of a bring-up event.
#[derive(Debug, Clone)]
pub enum BootAudit {
    /// The channel came up after `attempts` total attempts (`1` = first try).
    Started { attempts: u32 },
    /// An attempt failed. `retry_in_ms` is `None` exactly when `fatal` is
    /// `true` — there is no next attempt to schedule.
    Failed { attempt: u32, retry_in_ms: Option<u64>, fatal: bool, cause: String },
}

/// Where [`BootAudit`] records go. A boxed closure rather than a trait so the
/// supervisor stays database-free and a test can record into a `Vec`;
/// production is [`pg_sink::pg_boot_audit_sink`].
///
/// `Sync` as well as `Send`: the loop holds a *reference* to the sink across
/// the await that writes the row, and a `&T` only crosses threads when `T` is
/// `Sync`. Both real implementations capture `Arc`/`PgPool`/`String`, so this
/// costs nothing.
pub type BootAuditSink = Box<dyn Fn(BootAudit) -> BoxFuture<'static, ()> + Send + Sync>;

/// A supervised channel bring-up: the retry loop, plus the handle that stops
/// both it and whatever it started.
pub struct ChannelSupervisor {
    /// Channel name, for the one log line [`shutdown`](Self::shutdown) can emit.
    label: String,
    /// `None` after [`shutdown`](Self::shutdown) has taken it.
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: JoinHandle<()>,
}

impl ChannelSupervisor {
    /// Start supervising `attempt`, which is called once per try.
    ///
    /// Returns immediately: the first attempt runs inside the spawned task, so
    /// a slow or hanging bring-up no longer delays daemon startup either. (The
    /// old 60-second login timeout was protecting exactly that, and still
    /// bounds a single attempt — this makes it structural.)
    ///
    /// * `label` — channel name; appears in every log line and audit row.
    /// * `backoff` — delay schedule. [`RestartBackoff::default()`] is 1 s → ×2
    ///   → 60 s cap, the same schedule supervised workers use.
    /// * `escalator` — decides when a long outage earns a louder line.
    /// * `audit` — `None` disables audit rows entirely (tests, and any caller
    ///   with no pool).
    /// * `attempt` — one bring-up try. Called fresh each time, so it must own
    ///   or clone everything it needs.
    pub fn spawn<F, Fut>(
        label: impl Into<String>,
        backoff: RestartBackoff,
        escalator: DowntimeEscalator,
        audit: Option<BootAuditSink>,
        attempt: F,
    ) -> Self
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = BootOutcome> + Send + 'static,
    {
        let label = label.into();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let join =
            tokio::spawn(run(label.clone(), backoff, escalator, audit, attempt, shutdown_rx));
        Self { label, shutdown_tx: Some(shutdown_tx), join }
    }

    /// Stop the supervisor and, if the channel came up, the channel.
    ///
    /// Safe at any point in the loop: while retrying it cancels the backoff
    /// sleep rather than waiting it out, and after a successful start it stops
    /// the channel. An attempt already in flight is abandoned rather than
    /// cancelled — identical to the pre-#514 login-timeout arm, which already
    /// left its `spawn_blocking` task draining against the SDK's own HTTP
    /// timeouts, and harmless because every worker is spawned
    /// `--die-with-parent`.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            // An `Err` means the loop already returned (unconfigured, or
            // fatal) and dropped the receiver — nothing left to signal.
            let _ = tx.send(());
        }
        if let Err(e) = self.join.await {
            warn!(channel = %self.label, error = %e, "channel supervisor task did not join cleanly");
        }
    }
}

/// The retry loop. Split out of [`ChannelSupervisor::spawn`] so the generic
/// bounds sit in one place and the body reads top to bottom.
async fn run<F, Fut>(
    label: String,
    backoff: RestartBackoff,
    mut escalator: DowntimeEscalator,
    audit: Option<BootAuditSink>,
    attempt: F,
    mut shutdown: oneshot::Receiver<()>,
) where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = BootOutcome> + Send + 'static,
{
    // Failed attempts so far. Doubles as the `RestartBackoff` exponent, so the
    // first retry waits `base` rather than `base * factor`.
    let mut failures: u32 = 0;

    loop {
        // Never *start* an attempt once shutdown has arrived: a fresh attempt
        // spawns a worker (and possibly a sidecar) that would immediately be
        // abandoned.
        if !matches!(shutdown.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
            return;
        }

        let outcome = tokio::select! {
            // `biased`, attempt first: a bring-up that has ALREADY completed
            // must never be discarded in favour of shutdown, or a `Started`
            // channel would be dropped without being stopped — leaking the
            // bus's detached tasks. A still-pending attempt is not ready, so
            // shutdown wins that case as intended.
            biased;
            outcome = attempt() => outcome,
            _ = &mut shutdown => return,
        };

        match outcome {
            BootOutcome::NotConfigured => return,

            BootOutcome::Started(channel) => {
                let attempts = failures + 1;
                info!(channel = %label, attempts, "channel bus running");
                emit(&audit, BootAudit::Started { attempts }).await;
                // Park until the daemon shuts down, then stop the channel.
                // A dropped sender means the handle went away, which we treat
                // as shutdown — hence ignoring the result.
                let _ = (&mut shutdown).await;
                channel.stop().await;
                return;
            }

            BootOutcome::Fatal(e) => {
                error!(
                    channel = %label,
                    error = %format!("{e:#}"),
                    "CHANNEL DISABLED — it did NOT start and will NOT be retried, because no \
                     retry can fix what `error` names. The rest of the daemon is running \
                     normally. Fix what `error` names, then restart the daemon."
                );
                emit(
                    &audit,
                    BootAudit::Failed {
                        attempt: failures + 1,
                        retry_in_ms: None,
                        fatal: true,
                        cause: cap_chars(&format!("{e:#}"), AUDIT_CAUSE_CAP_CHARS),
                    },
                )
                .await;
                return;
            }

            BootOutcome::Retry(e) => {
                let delay = backoff.next_delay(failures);
                failures += 1;
                warn!(
                    channel = %label,
                    attempt = failures,
                    retry_in_ms = delay.as_millis() as u64,
                    error = %format!("{e:#}"),
                    "channel bring-up failed; retrying"
                );
                emit(
                    &audit,
                    BootAudit::Failed {
                        attempt: failures,
                        retry_in_ms: Some(delay.as_millis() as u64),
                        fatal: false,
                        cause: cap_chars(&format!("{e:#}"), AUDIT_CAUSE_CAP_CHARS),
                    },
                )
                .await;
                if let Some(down) = escalator.record_failure(Instant::now()) {
                    error!(
                        channel = %label,
                        down_secs = down.as_secs(),
                        attempts = failures,
                        "CHANNEL STILL DOWN — nothing sent to this channel has been received for \
                         this long, and bring-up is still failing. The daemon is otherwise \
                         healthy; the cause is on the preceding attempts' `error` field."
                    );
                }
                tokio::select! {
                    // Shutdown first here: there is nothing to lose by
                    // abandoning a sleep.
                    biased;
                    _ = &mut shutdown => return,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

/// Call the audit sink when there is one. Awaited rather than spawned so rows
/// land in attempt order and a test can assert on them deterministically.
async fn emit(audit: &Option<BootAuditSink>, event: BootAudit) {
    if let Some(sink) = audit {
        sink(event).await;
    }
}
