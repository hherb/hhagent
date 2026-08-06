//! ---------------------------------------------------------------------------
//! #518/#522/#523: what gets said, and what gets stored, about a recurring channel
//! event.
//!
//! Two halves. The first drives `note_outage` — the pure half of
//! `ReportingPolicy::note_failed_attempt`/`note_death` — with scripted
//! `Instant`s; escalation is a log line and nothing else (#517 review), so
//! this is the only seam from which the sequence that matters here (died →
//! recovered → worked for hours → flapped) is observable. The second runs the
//! real retry loop end to end through `ChannelSupervisor::spawn`, because the
//! #522 fix is specifically about state surviving actual restarts, which a
//! pure `ReportingPolicy` test cannot exercise.
//! ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::channel::respawn_alarm::RespawnRateAlarm;

use super::*;
// `Outage`/`note_outage` are `pub(super)` in `reporting` (visible to
// `boot_supervisor` and every descendant, this module included) but are no
// longer re-exported at the `boot_supervisor` top level, so `use super::*`
// above no longer brings them in — import them directly from where they live.
use super::super::reporting::{note_outage, Outage};

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

/// #522, end to end and through the real loop: the alarm must accumulate
/// ACROSS restarts.
///
/// This is the test that fails if the alarm is ever moved inside the retry
/// loop. `PersistentWorker` does not make that mistake for worker respawns —
/// its own alarm lives on the driver thread and correctly accumulates them —
/// but a *channel* restart tears down the whole `PersistentWorker`, so an
/// alarm living at that level could never see a channel-restart pattern
/// either; see the `deaths` field doc on `ReportingPolicy` for the full
/// argument. Three stable deaths with a threshold of two: an alarm rebuilt
/// per iteration would see a count of one, every time, and the third death's
/// row would still be written.
#[tokio::test]
async fn the_flap_alarm_accumulates_across_restarts() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let sink = RecordingSink::default();

    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        // Every death is "stable", which is the #522 band exactly: the
        // escalator resets on each one and can never fire.
        ReportingPolicy::default()
            .with_stable_uptime(Duration::ZERO)
            .with_flap_alarm(RespawnRateAlarm::new(Duration::from_secs(3600), 2)),
        Some(sink.sink()),
        scripted(vec![
            dying(&stopped),
            dying(&stopped),
            dying(&stopped),
            healthy(&stopped),
        ]),
    );

    // `stopped` counts channel stops, and the loop stops a dead channel BEFORE
    // deciding on its row — so `>= 3` means all three deaths have happened and,
    // by program order, cycle 2's row was already emitted. The fourth scripted
    // outcome is healthy and is only stopped by `shutdown()` below, which is
    // why waiting for 4 here would hang.
    //
    // Bare wait rather than `RecordingSink::wait_for`: it is `stopped`, not the
    // sink, being polled. Panics on fall-through rather than falling silently
    // out of the loop — a `for` loop with no assertion of its own gives no
    // signal if a future regression stops the three deaths from ever
    // happening, which is exactly the shape of bug #518/#522's review found in
    // this file's sibling test (since deleted).
    let mut all_three_died = false;
    for _ in 0..500 {
        if stopped.load(Ordering::SeqCst) >= 3 {
            all_three_died = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(all_three_died, "expected 3 scripted deaths within 5s, saw {}", stopped.load(Ordering::SeqCst));
    sup.shutdown().await;

    let deaths = sink
        .events()
        .into_iter()
        .filter(|e| matches!(e, BootAudit::Died { .. }))
        .count();
    assert_eq!(
        deaths, 2,
        "the first two deaths are durable and the third is suppressed by the latch; \
         an alarm rebuilt per restart would record all three: {:?}",
        sink.events()
    );
}

/// The mid-branch reversal, pinned: `channel.started` is NEVER gated. The flap
/// latch is only ever cleared by a LATER death, so a start recorded while the
/// alarm is latched is the only durable evidence that a storm ended — a gate on
/// this arm suppresses exactly the row that says "it is up again", leaving
/// `channel.died` as the last durable event for a healthy channel.
///
/// Threshold 2, so the second death latches the alarm and the third start
/// lands while the latch is armed. A `Started` arm gated on that latch records
/// 2 starts, not 3 — and no other test catches that mutation, because every
/// other test that counts `Started` rows runs below the default threshold of
/// five deaths and never latches the alarm. (Checked by mutation, not assumed:
/// gating the `Started` emit on the latch fails this test and nothing else.)
#[tokio::test]
async fn a_start_during_a_latched_storm_is_still_recorded() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let sink = RecordingSink::default();

    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        ReportingPolicy::default()
            .with_flap_alarm(RespawnRateAlarm::new(Duration::from_secs(3600), 2)),
        Some(sink.sink()),
        scripted(vec![dying(&stopped), dying(&stopped), healthy(&stopped)]),
    );

    // Five sink events: Started, Died, Started, Died (this one latches the
    // alarm), Started. Under the gated mutation the fifth never arrives, so
    // `wait_for` itself fails with the event dump.
    sink.wait_for(5).await;
    sup.shutdown().await;

    let starts = sink
        .events()
        .into_iter()
        .filter(|e| matches!(e, BootAudit::Started { .. }))
        .count();
    assert_eq!(
        starts, 3,
        "every start must be durable, latch or no latch — the third lands inside \
         the storm and is the row that says the channel came back: {:?}",
        sink.events()
    );
}

/// A fatal failure is never gated. It is terminal, it is one row, and it is the
/// row that says why the channel will not be retried — the gate exists for
/// events that repeat, and this one cannot.
#[tokio::test]
async fn a_fatal_failure_is_always_recorded() {
    let sink = RecordingSink::default();

    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        // A zero threshold escalates on the very first failure, so the
        // escalator is latched before the fatal outcome is reached.
        ReportingPolicy::new(DowntimeEscalator::new(Duration::ZERO, Duration::from_secs(1800))),
        Some(sink.sink()),
        scripted(vec![
            BootOutcome::Retry(anyhow::anyhow!("transient")),
            BootOutcome::Fatal(anyhow::anyhow!("KASTELLAN_EMAIL_ADDRESS is not set")),
        ]),
    );

    sink.wait_for(2).await;
    sup.shutdown().await;

    let events = sink.events();
    assert!(
        events.iter().any(|e| matches!(e, BootAudit::Failed { fatal: true, .. })),
        "the fatal row must survive a latched escalator: {events:?}"
    );
}

/// #518 through the real loop: once the outage has been reported, identical
/// attempts stop being written. A zero threshold escalates on the first
/// failure, so attempts two onward are inside the repeat interval and silent.
#[tokio::test]
async fn failed_attempts_stop_being_recorded_once_the_outage_is_reported() {
    let sink = RecordingSink::default();

    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        ReportingPolicy::new(DowntimeEscalator::new(Duration::ZERO, Duration::from_secs(1800))),
        Some(sink.sink()),
        scripted(vec![
            BootOutcome::Retry(anyhow::anyhow!("first")),
            BootOutcome::Retry(anyhow::anyhow!("second")),
            BootOutcome::Retry(anyhow::anyhow!("third")),
            BootOutcome::Started(StartedChannel::new(|| async {})),
        ]),
    );

    sink.wait_for_started().await;
    sup.shutdown().await;

    let failed = sink
        .events()
        .into_iter()
        .filter(|e| matches!(e, BootAudit::Failed { .. }))
        .count();
    assert_eq!(
        failed, 1,
        "the first attempt escalates and is recorded; the next two are inside the repeat \
         interval and say nothing new: {:?}",
        sink.events()
    );
}

/// #523: in the flap band each stable death clears the escalator
/// (`Outage::Ends` → `record_success`), so without its own gate the attempt
/// stream writes one ungated `boot_failed` row per cycle, forever. The rate
/// gate bounds it: the first attempts of the episode are recorded in full
/// (with `cause`), later ones are latched-and-silent, and the repeat interval
/// brings one cause-bearing row back per period.
#[test]
fn flap_band_boot_failed_rows_are_gated_once_the_rate_alarm_latches() {
    let base = std::time::Instant::now();
    let mut policy = ReportingPolicy::default().with_attempt_alarm(
        RespawnRateAlarm::new(Duration::from_secs(3600), 2)
            .with_repeat(Duration::from_secs(1800)),
    );

    // Each cycle: a stable death (which clears the escalator — the #523
    // premise), then one transiently-failing restart attempt.
    let mut verdicts = Vec::new();
    for i in 0..4u64 {
        let t = base + Duration::from_secs(61 * i);
        policy.note_death(true, t);
        verdicts.push(policy.note_failed_attempt(t + Duration::from_secs(1)));
    }

    assert!(
        verdicts.iter().all(|v| v.still_down.is_none()),
        "each stable death resets the downtime clock, so the escalator never speaks: {verdicts:?}"
    );
    assert!(verdicts[0].record, "the first failed attempt of the episode is recorded");
    assert!(verdicts[1].record, "the attempt that trips the rate gate is itself recorded");
    assert!(!verdicts[2].record, "latched and silent: this row says nothing new");
    assert!(!verdicts[3].record, "still latched: the unbounded-rows defect, gated");

    // The repeat interval brings one cause-bearing row back per period.
    let t = base + Duration::from_secs(2000);
    policy.note_death(true, t);
    let v = policy.note_failed_attempt(t + Duration::from_secs(1));
    assert!(v.record, "the repeat keeps one row per interval reaching the table: {v:?}");
}

/// The sampling-order trap, pinned for the ATTEMPT stream — the mirror of
/// `the_first_death_of_a_fresh_storm_is_recorded`. When an attempt storm
/// clears, `record` prunes the window and re-arms, so a latch read taken
/// BEFORE the recording call still shows the old storm and would silently
/// suppress the first failed attempt of the new one.
#[test]
fn the_first_failed_attempt_after_the_attempt_storm_clears_is_recorded() {
    let base = std::time::Instant::now();
    let mut policy = ReportingPolicy::default()
        .with_attempt_alarm(RespawnRateAlarm::new(Duration::from_secs(60), 2));

    policy.note_failed_attempt(base);
    policy.note_failed_attempt(base + Duration::from_secs(1));
    // Latched: a third attempt inside the window is gated.
    assert!(!policy.note_failed_attempt(base + Duration::from_secs(2)).record);

    // The restart then succeeds and the channel works: the outage ends
    // (`record_success`), so the escalator cannot speak for the attempt below
    // and the rate gate's own recovery is the only thing under test.
    policy.note_death(true, base + Duration::from_secs(400));

    // Long enough that the attempt window is empty: a NEW episode, and its
    // first failed attempt is the one an operator most needs in the table.
    let v = policy.note_failed_attempt(base + Duration::from_secs(500));
    assert!(v.record, "the first attempt of a fresh episode must be recorded: {v:?}");
    assert!(
        v.still_down.is_none(),
        "recorded by the rate gate's recovery, not by escalation: {v:?}"
    );
}

/// The deferral, pinned: once the escalator has escalated an outage, the rate
/// gate's repeat must NOT keep rows on its own schedule — the escalator owns
/// that regime, and two alarms repeating on independent clocks would write
/// near-duplicate rows every interval.
#[test]
fn a_rate_alarm_repeat_defers_to_an_escalated_outage() {
    let base = std::time::Instant::now();
    // A zero-threshold escalator escalates on the very first attempt; its
    // repeat (1800 s) then stays quiet for the rest of the test. The rate
    // gate's much shorter repeat would keep firing if it were allowed to.
    let mut policy = ReportingPolicy::new(DowntimeEscalator::new(
        Duration::ZERO,
        Duration::from_secs(1800),
    ))
    .with_attempt_alarm(
        RespawnRateAlarm::new(Duration::from_secs(3600), 1).with_repeat(Duration::from_secs(10)),
    );

    // First attempt: the escalation itself — recorded, and it latches both
    // alarms (threshold 1 trips the rate gate on the same event).
    let v = policy.note_failed_attempt(base);
    assert!(v.record && v.still_down.is_some());

    // 11 s later: the rate repeat has elapsed, the escalator repeat has not.
    // The rate voice alone must not keep the row.
    let v = policy.note_failed_attempt(base + Duration::from_secs(11));
    assert!(
        !v.record,
        "an escalated outage's rows follow the escalator's schedule, not the rate gate's: {v:?}"
    );
}

/// #523 through the real loop: a flapping channel whose restarts also fail
/// transiently. Without the rate gate every such attempt wrote an ungated
/// `boot_failed` row — one per cycle, unbounded; with it the attempt stream
/// is bounded exactly the way the death stream already is.
#[tokio::test]
async fn a_flapping_channel_with_failing_restarts_stops_writing_boot_failed_rows() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let sink = RecordingSink::default();

    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        // Every death is stable — the #523 premise: the escalator resets on
        // each one and can never latch, so only the rate gate stands between
        // the attempt stream and one ungated row per cycle.
        ReportingPolicy::default()
            .with_stable_uptime(Duration::ZERO)
            .with_attempt_alarm(RespawnRateAlarm::new(Duration::from_secs(3600), 2)),
        Some(sink.sink()),
        scripted(vec![
            dying(&stopped),
            BootOutcome::Retry(anyhow::anyhow!("first restart attempt")),
            dying(&stopped),
            BootOutcome::Retry(anyhow::anyhow!("second restart attempt")),
            dying(&stopped),
            BootOutcome::Retry(anyhow::anyhow!("third restart attempt")),
            healthy(&stopped),
        ]),
    );

    // Nine events: Started, Died, Failed, Started, Died, Failed, Started,
    // Died, Started — the third Retry is latched-and-silent, so it emits no
    // Failed row (a regression that records it makes the counts below fail).
    sink.wait_for(9).await;
    sup.shutdown().await;

    let events = sink.events();
    let failed = events.iter().filter(|e| matches!(e, BootAudit::Failed { .. })).count();
    assert_eq!(
        failed, 2,
        "the first two restart attempts are durable and the third is gated: {events:?}"
    );
    let died = events.iter().filter(|e| matches!(e, BootAudit::Died { .. })).count();
    assert_eq!(died, 3, "the death stream is untouched by the attempt gate: {events:?}");
}
