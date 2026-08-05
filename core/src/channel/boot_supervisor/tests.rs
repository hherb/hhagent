//! Behaviour tests for the bring-up supervisor.
//!
//! Hermetic: no network, no database, no sandbox. Every attempt is a scripted
//! [`BootOutcome`] and the "channel" is a probe that records whether it was
//! shut down — which is what makes the retry *policy* (the thing #514 is
//! about) testable at all, independently of anything a channel does.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;

/// 1 ms base and cap, so the retry loop spins fast enough for a test without
/// any test waiting on a realistic backoff.
fn fast_backoff() -> RestartBackoff {
    RestartBackoff {
        base: Duration::from_millis(1),
        factor_num: 1,
        factor_den: 1,
        cap: Duration::from_millis(1),
    }
}

/// Records every audit event the supervisor emits, in order.
#[derive(Clone, Default)]
struct RecordingSink(Arc<Mutex<Vec<BootAudit>>>);

impl RecordingSink {
    fn sink(&self) -> BootAuditSink {
        let events = Arc::clone(&self.0);
        Box::new(move |ev| {
            events.lock().expect("audit sink mutex").push(ev);
            Box::pin(async {})
        })
    }

    fn events(&self) -> Vec<BootAudit> {
        self.0.lock().expect("audit sink mutex").clone()
    }

    /// Poll until at least `n` events have been recorded. Polling rather than
    /// sleeping a fixed time keeps the test fast when the loop is fast and
    /// non-flaky when the machine is loaded.
    async fn wait_for(&self, n: usize) {
        for _ in 0..500 {
            if self.events().len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected at least {n} audit events, saw {:?}", self.events());
    }

    /// Poll until a `Started` row appears; returns its attempt count.
    async fn wait_for_started(&self) -> u32 {
        for _ in 0..500 {
            if let Some(BootAudit::Started { attempts }) =
                self.events().into_iter().find(|e| matches!(e, BootAudit::Started { .. }))
            {
                return attempts;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("channel never started; saw {:?}", self.events());
    }
}

/// A scripted attempt sequence: each call pops the next outcome. Running past
/// the end panics the supervisor task, which is deliberate — it is how a test
/// asserts "the loop stopped" rather than merely "the loop was slow".
fn scripted(
    outcomes: Vec<BootOutcome>,
) -> impl Fn() -> futures::future::BoxFuture<'static, BootOutcome> + Send + 'static {
    let queue = Arc::new(Mutex::new(VecDeque::from(outcomes)));
    move || {
        let next = queue.lock().expect("script mutex").pop_front();
        Box::pin(async move { next.expect("attempted more times than the script allows") })
    }
}

/// The #514 fix itself: two transient failures must not be the end of it.
#[tokio::test]
async fn retries_until_the_channel_comes_up() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let probe = Arc::clone(&stopped);
    let sink = RecordingSink::default();

    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        DowntimeEscalator::default(),
        Some(sink.sink()),
        scripted(vec![
            BootOutcome::Retry(anyhow::anyhow!("sidecar cgroup refused")),
            BootOutcome::Retry(anyhow::anyhow!("tunnel error: unsuccessful")),
            BootOutcome::Started(StartedChannel::new(move || {
                probe.fetch_add(1, Ordering::SeqCst);
                async {}
            })),
        ]),
    );

    assert_eq!(sink.wait_for_started().await, 3, "two failures then success is three attempts");

    sup.shutdown().await;
    assert_eq!(
        stopped.load(Ordering::SeqCst),
        1,
        "the running channel must be shut down exactly once"
    );
}

/// A statically-dead configuration must stop the loop. The script holds one
/// outcome, so a second attempt would panic the task and fail the join —
/// which is what proves it stopped rather than retried.
#[tokio::test]
async fn a_fatal_outcome_stops_the_loop() {
    let sink = RecordingSink::default();
    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        DowntimeEscalator::default(),
        Some(sink.sink()),
        scripted(vec![BootOutcome::Fatal(anyhow::anyhow!("homeserver is statically dead"))]),
    );

    sink.wait_for(1).await;
    sup.shutdown().await;

    let events = sink.events();
    assert_eq!(events.len(), 1, "a fatal outcome is audited once and never retried: {events:?}");
    match &events[0] {
        BootAudit::Failed { fatal, retry_in_ms, cause, .. } => {
            assert!(*fatal);
            assert!(retry_in_ms.is_none(), "there is no next attempt to schedule");
            assert!(cause.contains("statically dead"), "{cause}");
        }
        other => panic!("expected a Failed row, got {other:?}"),
    }
}

/// An unconfigured channel is the default for most deployments: no retries,
/// and nothing written anywhere.
#[tokio::test]
async fn an_unconfigured_channel_stops_silently() {
    let sink = RecordingSink::default();
    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        DowntimeEscalator::default(),
        Some(sink.sink()),
        scripted(vec![BootOutcome::NotConfigured]),
    );

    sup.shutdown().await;
    assert!(sink.events().is_empty(), "an absent channel is not an event: {:?}", sink.events());
}

/// Shutdown must not wait out the backoff delay — with a 60 s production cap,
/// a daemon stop would otherwise hang for up to a minute per channel.
#[tokio::test]
async fn shutdown_while_backing_off_returns_promptly() {
    let slow = RestartBackoff {
        base: Duration::from_secs(600),
        factor_num: 1,
        factor_den: 1,
        cap: Duration::from_secs(600),
    };
    let sink = RecordingSink::default();
    let sup = ChannelSupervisor::spawn(
        "test",
        slow,
        DowntimeEscalator::default(),
        Some(sink.sink()),
        scripted(vec![BootOutcome::Retry(anyhow::anyhow!("down"))]),
    );

    // Wait for the failure to be recorded, so we are certainly inside the
    // 10-minute sleep rather than racing the first attempt.
    sink.wait_for(1).await;
    tokio::time::timeout(Duration::from_secs(5), sup.shutdown())
        .await
        .expect("shutdown must not wait out the backoff delay");
}

/// The durable record: one row per failed attempt, numbered, carrying the
/// delay before the next one, then one row on success.
#[tokio::test]
async fn every_failed_attempt_is_audited_with_its_attempt_number() {
    let sink = RecordingSink::default();
    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        DowntimeEscalator::default(),
        Some(sink.sink()),
        scripted(vec![
            BootOutcome::Retry(anyhow::anyhow!("first")),
            BootOutcome::Retry(anyhow::anyhow!("second")),
            BootOutcome::Started(StartedChannel::new(|| async {})),
        ]),
    );

    sink.wait_for_started().await;
    sup.shutdown().await;

    let events = sink.events();
    assert_eq!(events.len(), 3, "{events:?}");
    match &events[0] {
        BootAudit::Failed { attempt, fatal, cause, retry_in_ms } => {
            assert_eq!(*attempt, 1);
            assert!(!*fatal);
            assert!(cause.contains("first"), "{cause}");
            assert!(retry_in_ms.is_some(), "a retryable failure carries its next delay");
        }
        other => panic!("expected a Failed row, got {other:?}"),
    }
    assert!(matches!(&events[1], BootAudit::Failed { attempt: 2, .. }), "{events:?}");
    assert!(matches!(&events[2], BootAudit::Started { attempts: 3 }), "{events:?}");
}

/// A supervisor with no sink runs identically — the audit seam is optional, so
/// a caller without a pool (or a test) is not forced to invent one.
///
/// Also pins the ordering guarantee the loop's `biased` select exists for: a
/// bring-up that has already completed is never discarded in favour of
/// shutdown, so `stop` is called even when shutdown lands right behind the
/// success. Counting *attempts* (rather than watching an audit row) is what
/// makes that observable with no sink — and it is sound because the loop
/// consumes the outcome in the same poll that produces it, with no await in
/// between.
#[tokio::test]
async fn a_supervisor_without_an_audit_sink_still_starts_and_stops_the_channel() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::new(AtomicUsize::new(0));
    let probe = Arc::clone(&stopped);
    let counter = Arc::clone(&attempts);

    let script = scripted(vec![
        BootOutcome::Retry(anyhow::anyhow!("transient")),
        BootOutcome::Started(StartedChannel::new(move || {
            probe.fetch_add(1, Ordering::SeqCst);
            async {}
        })),
    ]);

    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        DowntimeEscalator::default(),
        None,
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            script()
        },
    );

    // Both scripted outcomes consumed ⇒ the channel is up and parked.
    for _ in 0..500 {
        if attempts.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "the loop must have retried once and started");

    sup.shutdown().await;
    assert_eq!(stopped.load(Ordering::SeqCst), 1, "the channel started and was stopped once");
}

/// Shutdown that lands before the task's first poll must stop the loop
/// **without starting an attempt**. An attempt spawns a sandboxed worker (and,
/// under force-routing, its 1:1 egress sidecar), so one started here would be
/// abandoned the instant it completed — the very shape #502 is about.
///
/// This is the `try_recv` guard at the top of the loop, and nothing else covers
/// it: the `select!` below is `biased` with the attempt FIRST (deliberately, so
/// an already-completed bring-up is never dropped unstopped), which means
/// without the guard the first iteration calls `attempt()` before shutdown is
/// ever looked at.
///
/// `flavor = "current_thread"` is load-bearing, not decoration: it is what
/// makes the ordering deterministic. `tokio::spawn` only *queues* the task, and
/// `shutdown()` sends on the oneshot before its `join().await` — the first
/// thing that can yield to the supervisor. On a multi-thread runtime the task
/// could be polled in between and the test would flake.
#[tokio::test(flavor = "current_thread")]
async fn shutdown_before_the_first_poll_starts_no_attempt() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let sink = RecordingSink::default();

    // A script that would succeed if it ever ran, so a failure here means the
    // guard let an attempt through — not that the attempt happened to error.
    let script = scripted(vec![BootOutcome::Started(StartedChannel::new(|| async {}))]);
    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        DowntimeEscalator::default(),
        Some(sink.sink()),
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            script()
        },
    );

    sup.shutdown().await;

    assert_eq!(
        attempts.load(Ordering::SeqCst),
        0,
        "no worker (and no sidecar) may be spawned once shutdown has arrived"
    );
    assert!(sink.events().is_empty(), "an attempt never made is not an event: {:?}", sink.events());
}

// ---------------------------------------------------------------------------
// Liveness (#517): bring-up is only half of "the channel is up". A pump that
// ends afterwards leaves exactly #514's signature — every unit `active`, the
// log quiet — so a death has to re-enter the same retry loop a failed bring-up
// does.
// ---------------------------------------------------------------------------

/// A backoff whose delays actually grow, so a test can tell "backing off" from
/// "spinning". 20 ms doubling, cap far away.
fn growing_backoff() -> RestartBackoff {
    RestartBackoff {
        base: Duration::from_millis(20),
        factor_num: 2,
        factor_den: 1,
        cap: Duration::from_secs(10),
    }
}

/// A channel that is dead as soon as the supervisor looks at it — the scripted
/// stand-in for a bus whose pump returned.
fn dying(stopped: &Arc<AtomicUsize>) -> BootOutcome {
    let probe = Arc::clone(stopped);
    BootOutcome::Started(
        StartedChannel::new(move || {
            probe.fetch_add(1, Ordering::SeqCst);
            async {}
        })
        .with_death(Box::pin(std::future::ready(()))),
    )
}

/// A channel that never reports a death: the healthy steady state.
fn healthy(stopped: &Arc<AtomicUsize>) -> BootOutcome {
    let probe = Arc::clone(stopped);
    BootOutcome::Started(StartedChannel::new(move || {
        probe.fetch_add(1, Ordering::SeqCst);
        async {}
    }))
}

/// Every `retry_in_ms` from the `Died` rows recorded so far, in order.
fn death_delays(events: &[BootAudit]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|e| match e {
            BootAudit::Died { retry_in_ms, .. } => Some(*retry_in_ms),
            _ => None,
        })
        .collect()
}

/// The fix itself: a channel that stops working comes back, instead of leaving
/// the supervisor parked on a corpse for the life of the process.
#[tokio::test]
async fn a_channel_that_dies_is_brought_back_up() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let sink = RecordingSink::default();

    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        DowntimeEscalator::default(),
        Some(sink.sink()),
        scripted(vec![dying(&stopped), healthy(&stopped)]),
    );

    sink.wait_for(3).await;
    let events = sink.events();
    assert!(matches!(events[0], BootAudit::Started { .. }), "{events:?}");
    assert!(matches!(events[1], BootAudit::Died { .. }), "{events:?}");
    assert!(matches!(events[2], BootAudit::Started { .. }), "{events:?}");

    sup.shutdown().await;
    assert_eq!(
        stopped.load(Ordering::SeqCst),
        2,
        "the DEAD channel must be stopped too, not abandoned — its surviving pumps still \
         hold the worker, and the per-channel task's drop is what tears worker + sidecar down"
    );
}

/// A death is a different event from a failed bring-up and gets its own row,
/// carrying the one fact that separates an outage from a flap: how long the
/// channel actually ran.
#[tokio::test]
async fn a_death_is_audited_separately_from_a_failed_bring_up() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let sink = RecordingSink::default();

    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        DowntimeEscalator::default(),
        Some(sink.sink()),
        scripted(vec![dying(&stopped), healthy(&stopped)]),
    );

    sink.wait_for(3).await;
    let died = sink
        .events()
        .into_iter()
        .find_map(|e| match e {
            BootAudit::Died { ran_ms, retry_in_ms } => Some((ran_ms, retry_in_ms)),
            _ => None,
        })
        .expect("a death must be audited");
    let (ran_ms, retry_in_ms) = died;
    assert_eq!(retry_in_ms, 1, "restart is scheduled on the backoff, not immediately");
    assert!(ran_ms < 60_000, "a channel that died at once cannot have run a minute: {ran_ms}");

    sup.shutdown().await;
}

/// The flap guard. A channel dying the instant it comes up must NOT reset the
/// backoff, or the supervisor spins — spawning a sandboxed worker per
/// iteration, which is worse than the deafness it is fixing.
#[tokio::test]
async fn a_flapping_channel_backs_off_instead_of_spinning() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let sink = RecordingSink::default();

    let sup = ChannelSupervisor::spawn(
        "test",
        growing_backoff(),
        // Nothing counts as having stayed up, so every death is a flap.
        DowntimeEscalator::default().with_stable_uptime(Duration::MAX),
        Some(sink.sink()),
        scripted(vec![
            dying(&stopped),
            dying(&stopped),
            dying(&stopped),
            dying(&stopped),
            dying(&stopped),
        ]),
    );

    // Started + Died, three times over.
    sink.wait_for(6).await;
    let delays = death_delays(&sink.events());
    assert_eq!(
        delays[..3],
        [20, 40, 80],
        "successive flaps must back off exponentially, not restart at full speed"
    );

    sup.shutdown().await;
}

/// The other side of the flap guard: a channel that genuinely worked and later
/// died opens a NEW outage, so it comes back at the base delay rather than
/// inheriting a backoff from whatever happened hours earlier.
#[tokio::test]
async fn a_channel_that_ran_long_enough_restarts_at_the_base_delay() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let sink = RecordingSink::default();

    let sup = ChannelSupervisor::spawn(
        "test",
        growing_backoff(),
        // Every death counts as "it had been up", without waiting a minute.
        DowntimeEscalator::default().with_stable_uptime(Duration::ZERO),
        Some(sink.sink()),
        scripted(vec![
            dying(&stopped),
            dying(&stopped),
            dying(&stopped),
            dying(&stopped),
            dying(&stopped),
        ]),
    );

    sink.wait_for(6).await;
    let delays = death_delays(&sink.events());
    assert_eq!(
        delays[..3],
        [20, 20, 20],
        "a channel that had been running restarts at the base delay every time"
    );

    sup.shutdown().await;
}

/// Ordering: when a death and daemon shutdown are both ready, shutdown wins.
///
/// The assertion that actually distinguishes the two orderings is the **absent
/// `Died` row**, not the attempt count — with the bias flipped the loop still
/// declines to restart, because `wait_or_shutdown` sees the same shutdown
/// signal a few lines later. What flipping it *does* produce is a
/// `channel.died` row and a `warn!` for a channel that was merely being shut
/// down, i.e. an audit trail claiming an outage that never happened. (Checked
/// by mutation: reversing the two arms fails this test on the `Died` assertion
/// and on nothing else.)
///
/// `current_thread` is load-bearing: it is what makes both signals arrive
/// before the supervisor task is polled again, so this tests the `biased`
/// ordering rather than a race the machine happened to win.
#[tokio::test(flavor = "current_thread")]
async fn a_death_racing_shutdown_is_not_recorded_as_a_death() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(AtomicUsize::new(0));
    let sink = RecordingSink::default();
    let (death_tx, death_rx) = tokio::sync::oneshot::channel::<()>();
    let death_rx = Arc::new(Mutex::new(Some(death_rx)));

    let attempt_count = Arc::clone(&attempts);
    let stop_count = Arc::clone(&stopped);
    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        DowntimeEscalator::default(),
        Some(sink.sink()),
        move || {
            attempt_count.fetch_add(1, Ordering::SeqCst);
            let probe = Arc::clone(&stop_count);
            // Only the FIRST attempt gets the death signal; a restart would
            // show up as a second attempt, which is what this test forbids.
            let death = death_rx.lock().expect("death mutex").take();
            Box::pin(async move {
                let channel = StartedChannel::new(move || {
                    probe.fetch_add(1, Ordering::SeqCst);
                    async {}
                });
                let channel = match death {
                    Some(rx) => channel.with_death(Box::pin(async move {
                        let _ = rx.await;
                    })),
                    None => channel,
                };
                BootOutcome::Started(channel)
            }) as futures::future::BoxFuture<'static, BootOutcome>
        },
    );

    sink.wait_for_started().await; // the supervisor is now parked in the select

    // Both signals land before the supervisor task is polled again: on a
    // current_thread runtime this task holds the thread until it awaits, and
    // the first await is inside `shutdown()`.
    let _ = death_tx.send(());
    sup.shutdown().await;

    let events = sink.events();
    assert!(
        !events.iter().any(|e| matches!(e, BootAudit::Died { .. })),
        "a death that ties with shutdown must not be audited as an outage: {events:?}"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1, "no restart during shutdown");
    assert_eq!(stopped.load(Ordering::SeqCst), 1, "the channel is stopped exactly once");
}

/// `attempts` in a `channel.started` row means "restart-worthy events in this
/// outage, plus this success" — so a channel that had been running, died, and
/// came straight back must report **1**, not 2.
///
/// The death of a channel that worked is not a failed bring-up attempt; it is
/// what *opens* the outage. Counting it as an attempt makes a clean first-try
/// recovery read, to the operator querying `audit_log`, exactly like a
/// recovery that needed a retry.
#[tokio::test]
async fn a_first_try_recovery_reports_one_attempt() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let sink = RecordingSink::default();

    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        DowntimeEscalator::default().with_stable_uptime(Duration::ZERO),
        Some(sink.sink()),
        scripted(vec![dying(&stopped), healthy(&stopped)]),
    );

    sink.wait_for(3).await;
    let started: Vec<u32> = sink
        .events()
        .into_iter()
        .filter_map(|e| match e {
            BootAudit::Started { attempts } => Some(attempts),
            _ => None,
        })
        .collect();
    assert_eq!(
        started,
        vec![1, 1],
        "a channel that ran, died, and came back on the first try reports attempts: 1 both times"
    );

    sup.shutdown().await;
}

/// The counterpart: a channel that is *flapping* has not been healthy in
/// between, so its restarts really are successive attempts within one outage
/// and the count must keep climbing. This is what stops the reset above from
/// being a blanket "deaths are free".
#[tokio::test]
async fn a_recovery_after_a_flap_keeps_counting() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let sink = RecordingSink::default();

    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        DowntimeEscalator::default().with_stable_uptime(Duration::MAX),
        Some(sink.sink()),
        scripted(vec![dying(&stopped), dying(&stopped), healthy(&stopped)]),
    );

    sink.wait_for(5).await;
    let started: Vec<u32> = sink
        .events()
        .into_iter()
        .filter_map(|e| match e {
            BootAudit::Started { attempts } => Some(attempts),
            _ => None,
        })
        .collect();
    assert_eq!(started, vec![1, 2, 3], "two flaps in one outage make the third start attempt 3");

    sup.shutdown().await;
}

// ---------------------------------------------------------------------------
// Outage bookkeeping (#517 review). Escalation is a log line and nothing else,
// so these drive `note_outage` — the pure half of `escalate_if_due` — with
// scripted `Instant`s. That is the only seam from which the sequence that
// matters here (died → recovered → worked for hours → flapped) is observable.
// ---------------------------------------------------------------------------

/// The regression: a channel that died after working must NOT leave the
/// escalator timing an outage, because a successful restart never clears it.
///
/// The escalator learns about health only when a *stable* channel dies, so an
/// outage opened eagerly at that death survives the restart that fixed it. The
/// next flap then reports every healthy hour in between as downtime — in the
/// one line whose text asserts that nothing sent to the channel has been
/// received for that long, and past its threshold on the very first event, so
/// it fires immediately rather than after five minutes of real silence.
#[test]
fn a_death_that_recovers_leaves_no_outage_open() {
    let death = std::time::Instant::now();
    let mut esc = DowntimeEscalator::default();

    // A channel that had been working stops. The restart after it succeeds, so
    // nothing else ever touches the escalator.
    assert_eq!(note_outage(&mut esc, Outage::Ends, death), None);

    // Four healthy hours later it flaps. This is the FIRST event of a new
    // outage, which is zero seconds old.
    let flap = death + Duration::from_secs(4 * 3600);
    assert_eq!(
        note_outage(&mut esc, Outage::Continues, flap),
        None,
        "four hours the channel spent WORKING must not be reported as downtime"
    );

    // And it escalates on its own schedule, timed from the flap rather than
    // from the death four hours earlier.
    assert_eq!(
        note_outage(&mut esc, Outage::Continues, flap + Duration::from_secs(301)),
        Some(Duration::from_secs(301)),
        "the new outage is timed from its own first event"
    );
}

/// The price of the guarantee above, stated as a test so it is a decision
/// rather than an oversight: when the restart does NOT succeed, the outage is
/// dated from the first failed attempt instead of from the death — one backoff
/// delay late (1 s for the first restart, 60 s at the cap).
#[test]
fn a_death_whose_restart_fails_times_the_outage_from_that_failure() {
    let death = std::time::Instant::now();
    let mut esc = DowntimeEscalator::default();

    note_outage(&mut esc, Outage::Ends, death);
    // The restart one second later fails, and every attempt after it.
    let first_failure = death + Duration::from_secs(1);
    assert_eq!(note_outage(&mut esc, Outage::Continues, first_failure), None);

    // 300 s after the DEATH is only 299 s after the first failed attempt, so
    // the loud line is still one second away.
    assert_eq!(note_outage(&mut esc, Outage::Continues, death + Duration::from_secs(300)), None);
    assert_eq!(
        note_outage(&mut esc, Outage::Continues, first_failure + Duration::from_secs(300)),
        Some(Duration::from_secs(300)),
        "dated from the first failed restart, not the death"
    );
}

/// A death that was already inside an outage (a flap) extends it rather than
/// re-dating it — otherwise a channel that comes up for a second every minute
/// would reset the clock forever and never escalate at all.
#[test]
fn a_flapping_death_extends_the_outage_it_is_already_in() {
    let base = std::time::Instant::now();
    let mut esc = DowntimeEscalator::default();

    note_outage(&mut esc, Outage::Continues, base);
    assert_eq!(
        note_outage(&mut esc, Outage::Continues, base + Duration::from_secs(301)),
        Some(Duration::from_secs(301)),
        "downtime is measured from the outage's first event, flaps included"
    );
}
