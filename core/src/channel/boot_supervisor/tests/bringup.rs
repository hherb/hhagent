//! Bring-up (#514): a channel that will not start must keep being retried, and
//! a statically-dead configuration must not be.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::*;

/// The #514 fix itself: two transient failures must not be the end of it.
#[tokio::test]
async fn retries_until_the_channel_comes_up() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let probe = Arc::clone(&stopped);
    let sink = RecordingSink::default();

    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        ReportingPolicy::default(),
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
        ReportingPolicy::default(),
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
        ReportingPolicy::default(),
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
        ReportingPolicy::default(),
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

/// The durable record while the outage is still young: one row per failed
/// attempt, numbered, carrying the delay before the next one, then one row on
/// success.
///
/// "While young" is the whole caveat: once the outage escalates, identical
/// attempt rows stop being written (#518) — see
/// `tests::reporting::failed_attempts_stop_being_recorded_once_the_outage_is_reported`.
/// This test stays valid because it runs in milliseconds against the default
/// 300 s escalation threshold, so nothing here is ever gated.
#[tokio::test]
async fn failed_attempts_are_audited_with_their_attempt_numbers() {
    let sink = RecordingSink::default();
    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        ReportingPolicy::default(),
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
        ReportingPolicy::default(),
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
        ReportingPolicy::default(),
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
