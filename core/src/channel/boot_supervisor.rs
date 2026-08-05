//! Supervised channels: bring-up (#514) and staying up (#517).
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
//! ## Staying up (#517)
//!
//! Coming up once is not the same as working. The [`ChannelBus`]'s pumps each
//! have a terminal exit — the completed-task pump returning when its listener
//! gives up, a per-channel task breaking on a closed `recv`, a panic in either
//! — and the first version of this module parked on the shutdown oneshot as
//! soon as an attempt returned [`BootOutcome::Started`], so none of them was
//! ever noticed. The result was *the same symptom by a different route*: a
//! silent bot, `active` units, a healthy database and an empty log.
//!
//! So a [`StartedChannel`] now carries a liveness signal as well as a shutdown
//! closure, and a death re-enters this same loop — same backoff, same
//! escalator, same audit sink, plus a distinct [`BootAudit::Died`] row. Two
//! things keep *that* from becoming its own failure:
//!
//! * **The flap guard** ([`DowntimeEscalator::ran_long_enough`]). Only a
//!   channel that stayed up resets the failure counter; one that dies on
//!   arrival stays inside the current outage and keeps backing off. Without it
//!   a channel that cannot survive its first second would restart at full
//!   speed forever, spawning a sandboxed worker per iteration.
//! * **Stopping the corpse.** A channel that lost one pump still has the
//!   others, and it is the per-channel task's drop that tears down the worker
//!   and its sidecar. The loop therefore stops the channel on *both* exits from
//!   the wait, which is what keeps a restart from leaking what #502 fixed.
//!
//! The loop itself is database-free and network-free: audit rows go out
//! through a boxed closure ([`BootAuditSink`] — the idiom
//! [`crate::channel::polled_driver::AckOnlyAudit`] already uses), so the whole
//! module is testable with scripted outcomes and a probe channel. The
//! Postgres implementation lives in [`pg_sink`].

use std::future::Future;
use std::time::Instant;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::channel::audit_text::cap_chars;
use crate::worker_lifecycle::RestartBackoff;

pub mod downtime;
pub mod pg_sink;
pub mod reporting;
pub mod types;

pub use downtime::DowntimeEscalator;
pub use reporting::{note_outage, Outage, ReportingPolicy, Verdict, CHANNEL_FLAPPING_LOG_PHRASE};
pub use types::{BootAudit, BootAuditSink, BootOutcome, StartedChannel};

#[cfg(test)]
mod tests;

/// Cap on the `cause` string before it becomes a durable `audit_log` value.
/// See [`crate::channel::audit_text`] for why a sink bounds this itself rather
/// than trusting the value's producer.
///
/// Shared with the email channel's skipped-id sink (`main::email_boot`) so both
/// durable text fields are bounded identically — one cap, one set of edge cases.
pub const AUDIT_CAUSE_CAP_CHARS: usize = 256;

/// The phrase the [`BootOutcome::Fatal`] `error!` line opens with, and
/// therefore the string an operator greps for.
///
/// A `const` rather than a literal typed twice, because operator-facing help
/// text (`crate::install::plan::render_email_help`) tells the operator to grep
/// for exactly this — and that pairing has already drifted once: the line used
/// to read `EMAIL CHANNEL DISABLED` from `email_boot`, and #514 moved it here,
/// dropping the channel name out of the *message* and into a structured
/// `channel` field. The help text kept naming the old string, so the documented
/// grep matched nothing. Interpolating one const in both places makes that
/// particular drift unrepresentable.
pub const CHANNEL_DISABLED_LOG_PHRASE: &str = "CHANNEL DISABLED";

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
    /// * `policy` — decides when an event earns a louder line and when it
    ///   earns a durable row. [`ReportingPolicy::default()`] is the production
    ///   configuration.
    /// * `audit` — `None` disables audit rows entirely (tests, and any caller
    ///   with no pool).
    /// * `attempt` — one bring-up try. Called fresh each time, so it must own
    ///   or clone everything it needs.
    pub fn spawn<F, Fut>(
        label: impl Into<String>,
        backoff: RestartBackoff,
        policy: ReportingPolicy,
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
            tokio::spawn(run(label.clone(), backoff, policy, audit, attempt, shutdown_rx));
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
    mut policy: ReportingPolicy,
    audit: Option<BootAuditSink>,
    attempt: F,
    mut shutdown: oneshot::Receiver<()>,
) where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = BootOutcome> + Send + 'static,
{
    // Restart-worthy events since the channel was last healthy: failed
    // bring-up attempts, plus (since #517) the deaths of channels that never
    // stayed up long enough to count as having worked. Doubles as the
    // `RestartBackoff` exponent, so the first retry waits `base` rather than
    // `base * factor`, and as the `attempts` figure in the next `Started` row.
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

            BootOutcome::Started(mut channel) => {
                let attempts = failures + 1;
                info!(channel = %label, attempts, "channel bus running");
                // Deliberately NOT gated, unlike `Died`/`Failed`: the latch a
                // gate would read is only ever cleared by a LATER death, so
                // the start that ends a storm for good would be suppressed
                // right along with the ones inside it — leaving `channel.died`
                // as the last durable row for a channel that is, in fact,
                // healthy again. A gate here could make "is it up?" wrong,
                // which is a worse failure than a few extra rows during a
                // flap that is already loud (`CHANNEL FLAPPING` fires
                // independently, every `FLAP_ALARM_REPEAT`).
                emit(&audit, BootAudit::Started { attempts }).await;

                let started_at = Instant::now();
                // Wait for whichever comes first: the daemon shutting down, or
                // the channel reporting that it stopped working (#517). Before
                // this the loop parked on shutdown alone, so a pump that ended
                // afterwards left the supervisor watching a corpse — every unit
                // `active`, the log quiet, #514's signature reached after boot
                // instead of during it.
                //
                // `biased` with SHUTDOWN first — the opposite of the attempt
                // select above, where a COMPLETED attempt must beat shutdown or
                // a `Started` channel gets dropped without being stopped.
                //
                // What it buys here is narrower than it looks, and worth
                // stating exactly: it does NOT prevent a restart, because
                // `wait_or_shutdown` below sees the same signal and returns
                // before the next attempt either way. It prevents the *record*
                // of a death that never mattered — a `channel.died` row and its
                // `warn!` written while the daemon is on its way out, which is
                // a real sequence (Postgres and the daemon restarting together
                // kills the listener at about the moment shutdown is signalled)
                // and would leave an audit trail asserting an outage that was
                // just a shutdown. It also keeps an extra audit write off the
                // shutdown path, which #515 shows can be slow.
                let died = tokio::select! {
                    biased;
                    // A dropped sender means the handle went away, which we
                    // treat as shutdown — hence ignoring the result.
                    _ = &mut shutdown => false,
                    _ = channel.wait_for_death() => true,
                };

                // Both paths: the pump that died has already returned, but its
                // siblings have not, and the per-channel task's drop is what
                // tears the worker (and its sidecar) down. Abandoning a
                // half-dead bus here would leak exactly what #502 fixed.
                channel.stop().await;
                if !died {
                    return;
                }

                let ran = started_at.elapsed();
                // Did it actually work, or is it flapping?
                //
                // `failures` counts **restart-worthy events since the channel
                // was last healthy** — failed bring-up attempts and the deaths
                // of channels that never got going. It drives both the backoff
                // and the `attempts` figure in the next `Started` row, so the
                // two arms below have to say different things:
                //
                //   * A channel that STAYED UP and then died ends the outage —
                //     its death is not a failed attempt — so the counter resets
                //     and is NOT bumped, and the restart is attempt 1 at the
                //     base delay. Counting it would make a clean first-try
                //     recovery read, in `audit_log`, exactly like one that
                //     needed a retry. Whether it *opens* the next outage is a
                //     separate question, and the answer is no: see
                //     [`Outage::Ends`].
                //   * A channel that died on arrival is flapping: it never
                //     became healthy, so the death belongs to the outage
                //     already in progress and must be counted. Otherwise the
                //     backoff resets on every iteration and the supervisor
                //     spins, spawning a sandboxed worker each time round.
                let stable = policy.ran_long_enough(ran);
                let delay = if stable {
                    failures = 0;
                    backoff.next_delay(failures)
                } else {
                    let delay = backoff.next_delay(failures);
                    failures += 1;
                    delay
                };
                let ran_ms = ran.as_millis() as u64;
                let retry_in_ms = delay.as_millis() as u64;
                warn!(
                    channel = %label,
                    ran_ms,
                    retry_in_ms,
                    "channel stopped working after running; restarting it"
                );
                let verdict = policy.note_death(stable, Instant::now());
                if verdict.record {
                    emit(&audit, BootAudit::Died { ran_ms, retry_in_ms }).await;
                }
                // `failures` is the figure to report: the flapping arm has just
                // bumped it, and the stable arm cannot escalate at all
                // ([`Outage::Ends`] never yields a downtime), so the zero it
                // passes is never rendered.
                report(&label, &verdict, failures);
                if !wait_or_shutdown(delay, &mut shutdown).await {
                    return;
                }
            }

            BootOutcome::Fatal(e) => {
                error!(
                    channel = %label,
                    error = %format!("{e:#}"),
                    "{CHANNEL_DISABLED_LOG_PHRASE} — it did NOT start and will NOT be retried, \
                     because no retry can fix what `error` names. The rest of the daemon is \
                     running normally. Fix what `error` names, then restart the daemon."
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
                let verdict = policy.note_failed_attempt(Instant::now());
                if verdict.record {
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
                }
                report(&label, &verdict, failures);
                if !wait_or_shutdown(delay, &mut shutdown).await {
                    return;
                }
            }
        }
    }
}

/// Emit whichever loud lines this event earned.
///
/// Two independent alarms with two independent claims, kept separate on
/// purpose: "nothing has been received for `down_secs`" and "it has restarted
/// `deaths` times in the last hour" are different facts with different
/// remedies, and folding a flapping channel's up-time into the downtime clock
/// is the defect #521's review round removed.
fn report(label: &str, verdict: &Verdict, attempts: u32) {
    if let Some(down) = verdict.still_down {
        error!(
            channel = %label,
            down_secs = down.as_secs(),
            attempts,
            "CHANNEL STILL DOWN — nothing sent to this channel has been received for this long, \
             and it is still not staying up. The daemon is otherwise healthy; the cause is on \
             the preceding attempts' `error` field."
        );
    }
    if let Some(deaths) = verdict.flapping {
        error!(
            channel = %label,
            deaths,
            window_secs = reporting::FLAP_ALARM_WINDOW.as_secs(),
            "{CHANNEL_FLAPPING_LOG_PHRASE} — this channel keeps coming up and dying again. \
             Each cycle costs a sandboxed worker, its egress sidecar and a full login, and \
             a channel that restarts this often is not usefully up. The per-death cause is \
             in the preceding `channel stopped working after running` lines."
        );
    }
}

/// Sleep out the backoff. Returns `false` if shutdown arrived instead, meaning
/// the caller should stop rather than start another attempt.
///
/// `biased` toward shutdown: there is nothing to lose by abandoning a sleep,
/// and a daemon that is shutting down should not spend up to a minute waiting
/// to spawn a worker it will immediately abandon.
async fn wait_or_shutdown(delay: std::time::Duration, shutdown: &mut oneshot::Receiver<()>) -> bool {
    tokio::select! {
        biased;
        _ = &mut *shutdown => false,
        _ = tokio::time::sleep(delay) => true,
    }
}

/// Call the audit sink when there is one. Awaited rather than spawned so rows
/// land in attempt order and a test can assert on them deterministically.
async fn emit(audit: &Option<BootAuditSink>, event: BootAudit) {
    if let Some(sink) = audit {
        sink(event).await;
    }
}
