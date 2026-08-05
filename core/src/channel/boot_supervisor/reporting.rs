//! When does a restart-worthy channel event earn an operator's attention, and
//! when does it earn a durable row in `audit_log`?
//!
//! This module is the only place either question is answered for every
//! *recurring* event. (A [`BootOutcome::Fatal`](super::BootOutcome::Fatal) is
//! the one exception, and deliberately lives outside it: it happens at most
//! once per channel lifetime, so there is no rate to police — see
//! `boot_supervisor::report`.) Splitting an operator-facing policy across call
//! sites is how the answer drifts — #516 and #521 each found one instance of
//! exactly that, in this feature's own documentation.
//!
//! The retry loop in [`super`] stays deliberately ignorant of the policy: it
//! reports one event to [`ReportingPolicy::note_failed_attempt`] or
//! [`ReportingPolicy::note_death`], gets back a [`Verdict`], and acts on
//! it — the loud line when an alarm fired, the durable row when
//! [`Verdict::record`] says to keep it. It owns the channel label and
//! therefore the logging; this module owns the deciding.
//!
//! ## What gets said, and what gets stored (#518, #522)
//!
//! Two alarms each own one regime: [`DowntimeEscalator`] times a channel that
//! will not come up at all, and [`RespawnRateAlarm`] counts a channel that
//! keeps coming up and dying again — the band #522 found where every death is
//! "stable" (up longer than [`super::downtime::STABLE_UPTIME`] but nowhere
//! near its escalation threshold), so the escalator resets on every one and
//! can never fire. [`should_record`] is the predicate that gates the durable
//! row for `channel.boot_failed` and `channel.died`: record the event unless
//! the alarm that owns its regime is already latched on this episode and did
//! not speak for this particular event. That is what turns a 24-hour outage
//! into ~57 `boot_failed` rows instead of ~1440, without inventing a "first N
//! events" counter that would drift from the escalation policy the moment
//! either changed.
//!
//! `channel.started` is deliberately **not** gated by either alarm — see
//! [`super::run`]'s `Started` arm for why a flap-alarm latch is the wrong
//! predicate for a row that is, precisely, the one that says a storm has
//! ended. A 24-hour flap at the #522 band's ~61 s cycle is therefore ~1470
//! rows (~1416 unguarded `started` plus ~53 gated `died`), down from ~2832 —
//! smaller than the bring-up-outage win, because a flap is loud on its own
//! terms (`CHANNEL FLAPPING` every [`FLAP_ALARM_REPEAT`]) and short-lived by
//! nature, whereas an unescalated bring-up outage is unbounded: a
//! permanently-failing `Retry` (a missing worker binary, say) can run for
//! weeks.

use std::time::{Duration, Instant};

use super::DowntimeEscalator;
use crate::channel::respawn_alarm::RespawnRateAlarm;

/// How one restart-worthy event relates to the outage the escalator is timing.
///
/// The distinction exists because a death is not always a *failure*: a channel
/// that had been working for hours and then stopped has just ended a period of
/// health, whereas a failed bring-up (or the death of a channel that never got
/// going) extends an outage already in progress.
#[derive(Debug, Clone, Copy)]
pub enum Outage {
    /// A bring-up attempt failed, or a channel died without ever having stayed
    /// up long enough to count as having worked. Extends the outage in
    /// progress, opening one dated from now if there was none.
    Continues,
    /// A channel that HAD been working stopped. Ends the outage the escalator
    /// was timing — and, deliberately, does **not** open the next one.
    ///
    /// Opening it here reads more precise, because the outage really does begin
    /// at the death. But nothing would ever close it: the escalator is only
    /// told about health when a *stable* channel dies, so a channel that came
    /// straight back and then worked for four hours would still be carrying
    /// this instant when it next flapped — and would report those four healthy
    /// hours as downtime, in the one line whose text asserts that nothing sent
    /// to the channel has been received for that long. The next outage is
    /// therefore opened by the first restart attempt that actually fails, which
    /// is the only version that stays correct when the restart SUCCEEDS. The
    /// price is that a real outage is dated one backoff delay late (1 s for the
    /// first restart, 60 s at the cap); the price of the eager version is
    /// unbounded.
    Ends,
}

/// The pure half of the escalation decision [`ReportingPolicy::note_failed_attempt`]
/// and [`ReportingPolicy::note_death`] each make: update the outage
/// bookkeeping for one event and answer "does this one earn the loud line?".
///
/// Split from the logging so the sequence that has no other test seam — a
/// channel that died, came back, worked for hours, and then flapped — can be
/// exercised without a runtime, a channel or a log capture. Escalation is a
/// log line and nothing else, so without this it is unobservable to a test.
pub fn note_outage(
    escalator: &mut DowntimeEscalator,
    outage: Outage,
    now: Instant,
) -> Option<Duration> {
    match outage {
        // Ends the outage and opens nothing — see [`Outage::Ends`] for why the
        // next one has to wait for a restart that actually fails. `now` is
        // deliberately unused on this arm.
        Outage::Ends => {
            escalator.record_success();
            None
        }
        Outage::Continues => escalator.record_failure(now),
    }
}

/// The phrase the flap alarm's `error!` line opens with, and therefore the
/// string an operator greps for.
///
/// A `const` from the outset rather than a literal typed twice: #516's finding
/// was precisely that an operator-facing phrase written in two places drifts,
/// and that the test pinning the literal stayed green through it.
pub const CHANNEL_FLAPPING_LOG_PHRASE: &str = "CHANNEL FLAPPING";

/// How far back the flap alarm counts deaths.
///
/// An hour rather than the escalator's five minutes, and the reasoning is worth
/// keeping: a longer window costs **nothing** in detection latency for a fast
/// flap — five deaths 67 s apart trip the threshold at ~4.5 min under either
/// window, because the window only governs pruning — and it is the only thing
/// that catches the slow half of the band. "Up 200 s, then dead" is ~430
/// restarts a day, and a five-minute window never holds more than two of them.
pub const FLAP_ALARM_WINDOW: Duration = Duration::from_secs(3600);

/// Deaths inside [`FLAP_ALARM_WINDOW`] that make a channel "flapping".
///
/// Matches `PersistentWorker`'s alarm threshold for the same failure shape.
/// Five channel deaths inside an hour is not a benign maintenance sequence.
pub const FLAP_ALARM_THRESHOLD: usize = 5;

/// How often the flap alarm repeats while the storm persists.
///
/// [`super::downtime::DEFAULT_REPEAT`]'s value, for the same reason: an
/// hours-long problem should be a handful of lines rather than one line and
/// then silence. It matters more here than it does there, because this alarm
/// also gates the durable row — without a repeat, a flap lasting days would
/// leave a handful of rows in total.
pub const FLAP_ALARM_REPEAT: Duration = Duration::from_secs(1800);

/// What to say and what to store about one restart-worthy event.
///
/// The two alarms are separate `Option`s rather than one enum because a
/// flapping death can trip both in the same iteration, and inventing a
/// precedence between "still down" and "flapping" would be a policy decision
/// nobody asked for. The caller emits whichever are present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    /// Write the durable `audit_log` row for this event.
    pub record: bool,
    /// The channel has been down this long and is not recovering.
    pub still_down: Option<Duration>,
    /// The channel has died this many times inside [`FLAP_ALARM_WINDOW`].
    pub flapping: Option<usize>,
}

/// The gate, and the whole of #518 in one line.
///
/// A recurring event earns a durable row unless its alarm is already latched on
/// this episode and did not speak for this particular event. Everything else —
/// which alarm, which stream — is the caller's business.
///
/// Note what this deliberately is NOT: a "first N events" counter. N would be a
/// constant nobody could derive from anything, and it would drift from the
/// escalation policy the moment either changed. "Until the alarm speaks" gives
/// the same shape with no new knob, and makes the row and the loud line the
/// *same* decision rather than two decisions that agree today.
pub fn should_record(alarm_latched: bool, alarm_spoke_now: bool) -> bool {
    !alarm_latched || alarm_spoke_now
}

/// Everything the supervisor needs to decide what to say and store about one
/// event.
///
/// Holds the alarms rather than exposing them, so the retry loop cannot reach
/// past the policy and ask one of them directly — which is how the row and the
/// line would drift apart again.
pub struct ReportingPolicy {
    escalator: DowntimeEscalator,
    /// Counts deaths across restarts. **Owned here, not inside the retry
    /// loop** — that placement IS the #522 fix, for the same reason
    /// `PersistentWorker`'s own respawn alarm lives on its driver thread
    /// rather than inside the worker object a respawn replaces: an alarm
    /// scoped to the thing being torn down and rebuilt can never see a
    /// pattern across rebuilds of that thing. `PersistentWorker`'s alarm gets
    /// this right for worker respawns — it correctly accumulates across them,
    /// for the life of the driver thread — but it cannot also see a *channel*
    /// restart, because a channel restart tears down the whole
    /// `PersistentWorker`, driver thread and alarm together. This alarm has
    /// to live one level higher, in the object a channel restart does not
    /// replace: `ReportingPolicy`, which the retry loop holds across every
    /// iteration.
    deaths: RespawnRateAlarm,
}

impl Default for ReportingPolicy {
    fn default() -> Self {
        Self::new(DowntimeEscalator::default())
    }
}

impl ReportingPolicy {
    /// Build a policy over a specific escalator. Tests use this to shorten the
    /// thresholds; production uses [`Default`].
    pub fn new(escalator: DowntimeEscalator) -> Self {
        Self {
            escalator,
            deaths: RespawnRateAlarm::new(FLAP_ALARM_WINDOW, FLAP_ALARM_THRESHOLD)
                .with_repeat(FLAP_ALARM_REPEAT),
        }
    }

    /// Override how long a channel must stay up for its death to count as
    /// having worked. Delegates to [`DowntimeEscalator::with_stable_uptime`];
    /// re-exposed here so a test that used to build the escalator directly
    /// changes by one type name.
    pub fn with_stable_uptime(mut self, stable_uptime: Duration) -> Self {
        self.escalator = self.escalator.with_stable_uptime(stable_uptime);
        self
    }

    /// Override the death-rate alarm. Exists so a test can trip it in two
    /// deaths instead of five, the same reason `DowntimeEscalator`'s thresholds
    /// are parameters rather than constants.
    pub fn with_flap_alarm(mut self, deaths: RespawnRateAlarm) -> Self {
        self.deaths = deaths;
        self
    }

    /// Did a channel that has now died run long enough to count as having
    /// worked? The flap guard — see [`DowntimeEscalator::ran_long_enough`].
    pub fn ran_long_enough(&self, ran: Duration) -> bool {
        self.escalator.ran_long_enough(ran)
    }

    /// Is the channel currently inside a death storm the alarm has reported?
    /// Test-facing only — nothing in production reads this; the durable-row
    /// gate for deaths lives entirely inside [`note_death`](Self::note_death).
    #[cfg(test)]
    pub(super) fn in_flap_storm(&self) -> bool {
        self.deaths.in_storm()
    }

    /// Fold a failed bring-up attempt into the bookkeeping.
    pub fn note_failed_attempt(&mut self, now: Instant) -> Verdict {
        let still_down = note_outage(&mut self.escalator, Outage::Continues, now);
        // Read after recording, matching `note_death` below — but here the
        // order is NOT actually load-bearing: `record_failure` never clears
        // `has_escalated`'s latch mid-call the way `RespawnRateAlarm::record`
        // clears `in_storm`'s, so `!latched || spoke` is invariant either way.
        // Kept read-after-record anyway, so the two methods read alike.
        Verdict {
            record: should_record(self.escalator.has_escalated(), still_down.is_some()),
            still_down,
            flapping: None,
        }
    }

    /// Fold the death of a running channel into the bookkeeping.
    ///
    /// `stable` is the flap-guard verdict the caller has already computed with
    /// [`ran_long_enough`](Self::ran_long_enough); it is passed in rather than
    /// recomputed so the loop and the policy cannot disagree about it.
    pub fn note_death(&mut self, stable: bool, now: Instant) -> Verdict {
        let outage = if stable { Outage::Ends } else { Outage::Continues };
        let still_down = note_outage(&mut self.escalator, outage, now);
        let flapping = self.deaths.record(now);
        // The death stream's alarm is the flap alarm, but a flapping death can
        // also be the one that escalates the outage — either speaking is reason
        // enough to keep the row.
        let spoke = still_down.is_some() || flapping.is_some();
        Verdict {
            record: should_record(self.deaths.in_storm(), spoke),
            still_down,
            flapping,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::respawn_alarm::RespawnRateAlarm;

    /// The whole gate, as a truth table. An event is recorded unless its alarm
    /// is already latched on this episode AND did not speak for this event.
    #[test]
    fn should_record_is_true_unless_the_alarm_is_latched_and_silent() {
        assert!(should_record(false, false), "nothing reported yet: record it");
        assert!(should_record(false, true), "the first alarm of an episode: record it");
        assert!(should_record(true, true), "a repeat alarm: record it");
        assert!(!should_record(true, false), "latched and silent: this row says nothing new");
    }

    /// A failed attempt is recorded in full until the outage escalates, then
    /// only on escalations — ~57 rows in a day instead of ~1440 (#518).
    #[test]
    fn failed_attempts_stop_being_recorded_once_the_outage_escalates() {
        let base = Instant::now();
        let mut policy = ReportingPolicy::new(DowntimeEscalator::new(
            Duration::from_secs(300),
            Duration::from_secs(1800),
        ));

        // Inside the threshold: quiet, but every attempt is durable.
        let v = policy.note_failed_attempt(base);
        assert!(v.record && v.still_down.is_none());
        let v = policy.note_failed_attempt(base + Duration::from_secs(100));
        assert!(v.record && v.still_down.is_none());

        // Past the threshold: the loud line fires, and this row is kept.
        let v = policy.note_failed_attempt(base + Duration::from_secs(301));
        assert_eq!(v.still_down, Some(Duration::from_secs(301)));
        assert!(v.record);

        // Now latched: identical attempts stop being written.
        let v = policy.note_failed_attempt(base + Duration::from_secs(400));
        assert!(!v.record && v.still_down.is_none());

        // ...until the repeat interval brings the line back, and the row with it.
        let v = policy.note_failed_attempt(base + Duration::from_secs(2101));
        assert!(v.still_down.is_some() && v.record);
    }

    /// A death is recorded until the flap alarm latches, then only when it
    /// speaks again (#522). Uses a 2-death threshold so the test does not have
    /// to script five.
    #[test]
    fn deaths_stop_being_recorded_once_the_flap_alarm_latches() {
        let base = Instant::now();
        let mut policy = ReportingPolicy::default().with_flap_alarm(
            RespawnRateAlarm::new(Duration::from_secs(3600), 2)
                .with_repeat(Duration::from_secs(1800)),
        );

        // Every death here is STABLE — the #522 band, where the escalator can
        // never fire and the flap alarm is the only thing counting.
        let v = policy.note_death(true, base);
        assert!(v.record && v.flapping.is_none());

        let v = policy.note_death(true, base + Duration::from_secs(61));
        assert_eq!(v.flapping, Some(2), "the second death in the window trips the alarm");
        assert!(v.record, "the death that trips the alarm is itself recorded");

        let v = policy.note_death(true, base + Duration::from_secs(122));
        assert!(!v.record && v.flapping.is_none(), "latched: this row says nothing new");

        let v = policy.note_death(true, base + Duration::from_secs(1900));
        assert!(v.flapping.is_some() && v.record, "the repeat brings the line and the row back");
    }

    /// The regression #522 is about, at the policy level: a stable death is the
    /// case where the escalator resets and can NEVER escalate, so without the
    /// flap alarm nothing would ever speak.
    #[test]
    fn a_stable_death_never_escalates_but_can_still_flap() {
        let base = Instant::now();
        let mut policy = ReportingPolicy::default().with_flap_alarm(
            RespawnRateAlarm::new(Duration::from_secs(3600), 2),
        );

        for i in 0..5u64 {
            let v = policy.note_death(true, base + Duration::from_secs(61 * i));
            assert!(
                v.still_down.is_none(),
                "a stable death ends the outage, so the downtime line can never fire"
            );
        }
        // But the flap alarm did speak, which is the whole point of #522.
        assert!(policy.in_flap_storm(), "five stable deaths in an hour is a storm");
    }

    /// The sampling-order trap, pinned. When a storm clears, `record` prunes
    /// the window and clears the latch — so a latch read taken BEFORE the
    /// recording call still shows the old storm, and silently suppresses the
    /// first death of the new one: the single row that says a fresh storm has
    /// started.
    #[test]
    fn the_first_death_of_a_fresh_storm_is_recorded() {
        let base = Instant::now();
        let mut policy = ReportingPolicy::default().with_flap_alarm(
            RespawnRateAlarm::new(Duration::from_secs(60), 2),
        );

        policy.note_death(true, base);
        assert!(policy.note_death(true, base + Duration::from_secs(1)).flapping.is_some());
        assert!(policy.in_flap_storm(), "latched on the first storm");

        // Long enough that the window is empty: this is a NEW storm, and its
        // first death is the one an operator most needs in the table.
        let v = policy.note_death(true, base + Duration::from_secs(500));
        assert!(v.record, "the first death of a fresh storm must be recorded");
        assert!(!policy.in_flap_storm(), "and the latch is clear again");
    }
}
