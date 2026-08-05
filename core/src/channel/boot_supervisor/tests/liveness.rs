//! ---------------------------------------------------------------------------
//! Liveness (#517): bring-up is only half of "the channel is up". A pump that
//! ends afterwards leaves exactly #514's signature — every unit `active`, the
//! log quiet — so a death has to re-enter the same retry loop a failed bring-up
//! does.
//! ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;

/// The fix itself: a channel that stops working comes back, instead of leaving
/// the supervisor parked on a corpse for the life of the process.
#[tokio::test]
async fn a_channel_that_dies_is_brought_back_up() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let sink = RecordingSink::default();

    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        ReportingPolicy::default(),
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
        ReportingPolicy::default(),
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
        ReportingPolicy::default().with_stable_uptime(Duration::MAX),
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
        ReportingPolicy::default().with_stable_uptime(Duration::ZERO),
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
        ReportingPolicy::default(),
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
        ReportingPolicy::default().with_stable_uptime(Duration::ZERO),
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
        ReportingPolicy::default().with_stable_uptime(Duration::MAX),
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
