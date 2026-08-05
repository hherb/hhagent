//! Sliding-window respawn-rate alarm for supervised channel workers (#348 item 3).
//!
//! The `PersistentWorker` supervisor (`worker_lifecycle::persistent`) silently
//! respawns a dead worker with capped backoff. A *single* crash is benign, but a
//! worker that dies-and-respawns repeatedly ("churn") is a real fault — historically
//! the bwrap-PDEATHSIG bug (#348) produced exactly this, and it was only
//! diagnosed after deploying death-report observability. This module turns that
//! churn into an *up-front* warning: it counts respawns in a sliding time window
//! and signals once per storm when the rate crosses a caller-supplied threshold (the
//! supervisor wires in its compile-time `ALARM_THRESHOLD` / `ALARM_WINDOW`
//! defaults), unless a repeat interval is configured.
//!
//! Two consumers: `PersistentWorker` uses it to notice worker death churn,
//! and the channel supervisor (`boot_supervisor`) uses it to notice channel
//! death churn.
//!
//! The type is deliberately a **pure state machine over caller-supplied
//! [`Instant`]s** — it owns no clock and spawns nothing — so the driver decides
//! when "now" is and the alarm logic is unit-testable without threads or sleeps.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Tracks worker respawn instants in a sliding window and fires once per storm
/// when the in-window respawn count reaches a threshold, or repeatedly if a
/// repeat interval is configured.
///
/// Firing once per storm is the default: while a storm keeps the count at or
/// above the threshold the alarm stays *armed* and [`record`](Self::record)
/// returns `None`, so a sustained churn warns a single time rather than on
/// every respawn. With a repeat interval, the alarm speaks again after the
/// interval elapses (measured from the previous firing), avoiding log silence
/// for multi-day flaps. The alarm re-arms automatically once enough time passes
/// that the in-window count falls back below the threshold (the storm cleared).
pub struct RespawnRateAlarm {
    /// Length of the sliding window. Respawns older than this (relative to the
    /// most recent `record`) are pruned before counting.
    window: Duration,
    /// Respawn count (within the window) that trips the alarm. `record` fires
    /// when the count reaches this value.
    threshold: usize,
    /// Respawn instants currently within the window, oldest-first.
    recent: VecDeque<Instant>,
    /// `true` while the current storm has already fired, suppressing repeats
    /// until the in-window count drops below `threshold` again.
    armed: bool,
    /// Minimum gap between firings while a storm persists. `None` — the
    /// default — means "fire once per storm", which is what `PersistentWorker`
    /// has always done and must keep doing.
    ///
    /// The channel supervisor sets one because its alarm also gates a durable
    /// audit row: without a repeat, a flap lasting days would leave a handful
    /// of rows in total and the rotating daemon log as the only record.
    repeat: Option<Duration>,
    /// When the current storm last fired. `Some` exactly while `armed`.
    last_fired: Option<Instant>,
}

impl RespawnRateAlarm {
    /// Create an alarm that fires when `threshold` respawns occur within
    /// `window`.
    pub fn new(window: Duration, threshold: usize) -> Self {
        Self {
            window,
            threshold,
            recent: VecDeque::new(),
            armed: false,
            repeat: None,
            last_fired: None,
        }
    }

    /// Speak again every `repeat` while a storm persists, instead of once per
    /// storm.
    ///
    /// Additive on purpose: the default is today's behaviour exactly, so the
    /// existing `PersistentWorker` caller is unchanged. Same shape as
    /// `DowntimeEscalator::with_stable_uptime`, and the same `threshold` +
    /// `repeat` pairing that type already uses, so the two policies read alike.
    pub fn with_repeat(mut self, repeat: Duration) -> Self {
        self.repeat = Some(repeat);
        self
    }

    /// Is the alarm currently latched on a storm it has already reported?
    ///
    /// Read by the channel supervisor's audit gate, which records an event
    /// unless the alarm is latched and did not speak for that event. **Must be
    /// read AFTER [`record`](Self::record)**: `record` is what clears the latch
    /// when a storm has cleared, so a read taken beforehand can suppress the
    /// first event of a fresh storm — the one event that most deserves a row.
    ///
    /// `pub(crate)` rather than `pub`, deliberately: that read-after-record
    /// contract lives in this comment, not in the type system, so a bare latch
    /// read is an attractive nuisance to any caller that has not absorbed it
    /// (#523 documents the one in-crate temptation). Keeping it off the
    /// published API means the only callers are ones this repo's review can
    /// see.
    pub(crate) fn in_storm(&self) -> bool {
        self.armed
    }

    /// Record a respawn that happened at `now`.
    ///
    /// Returns `Some(count)` — where `count` is the number of respawns in the
    /// window including this one — the first time the in-window count reaches
    /// the threshold for a given storm; or again once `repeat` (if set) has
    /// elapsed since the previous firing; returns `None` otherwise (below
    /// threshold, or already fired for the ongoing storm without a repeat, or
    /// repeat interval not yet elapsed).
    ///
    /// `now` is expected to be monotonically non-decreasing across calls (it is
    /// in the driver, where it is always `Instant::now()`); out-of-order
    /// timestamps are tolerated but pruning uses each call's `now` as the
    /// window's right edge.
    pub fn record(&mut self, now: Instant) -> Option<usize> {
        // Drop respawns that have aged out of the window (right edge = `now`).
        while let Some(&front) = self.recent.front() {
            if now.saturating_duration_since(front) > self.window {
                self.recent.pop_front();
            } else {
                break;
            }
        }
        self.recent.push_back(now);

        let count = self.recent.len();
        if count < self.threshold {
            // Storm cleared (or never started): re-arm for the next one.
            self.armed = false;
            self.last_fired = None;
            None
        } else if self.armed {
            // Threshold met and already reported for the ongoing storm. Speak
            // again only if a repeat interval was configured AND it has
            // elapsed since the previous line; otherwise stay silent.
            match (self.repeat, self.last_fired) {
                (Some(repeat), Some(last))
                    if now.saturating_duration_since(last) >= repeat =>
                {
                    self.last_fired = Some(now);
                    Some(count)
                }
                _ => None,
            }
        } else {
            self.armed = true;
            self.last_fired = Some(now);
            Some(count)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(60);

    /// A respawn count below the threshold never fires the alarm.
    #[test]
    fn fewer_respawns_than_threshold_do_not_alarm() {
        let base = Instant::now();
        let mut alarm = RespawnRateAlarm::new(WINDOW, 3);

        assert_eq!(alarm.record(base), None);
        assert_eq!(alarm.record(base + Duration::from_secs(1)), None);
    }

    /// Reaching the threshold inside the window fires exactly once; further
    /// respawns within the same window stay silent (no log spam).
    #[test]
    fn reaching_threshold_within_window_alarms_once() {
        let base = Instant::now();
        let mut alarm = RespawnRateAlarm::new(WINDOW, 3);

        assert_eq!(alarm.record(base), None);
        assert_eq!(alarm.record(base + Duration::from_secs(5)), None);
        // Third respawn within the window trips the alarm, reporting the count.
        assert_eq!(alarm.record(base + Duration::from_secs(10)), Some(3));
        // A fourth, still within the window, must NOT re-fire.
        assert_eq!(alarm.record(base + Duration::from_secs(15)), None);
    }

    /// Respawns older than the window are pruned, so a slow trickle never trips
    /// the alarm.
    #[test]
    fn respawns_outside_window_are_pruned() {
        let base = Instant::now();
        let mut alarm = RespawnRateAlarm::new(WINDOW, 3);

        assert_eq!(alarm.record(base), None);
        assert_eq!(alarm.record(base + Duration::from_secs(30)), None);
        // 200s after the first two: both are now outside the 60s window, so the
        // in-window count is just this one respawn — well below threshold.
        assert_eq!(alarm.record(base + Duration::from_secs(200)), None);
    }

    /// After a storm fires and then clears (window empties), a fresh storm fires
    /// again — the alarm re-arms rather than latching forever.
    #[test]
    fn alarm_rearms_after_storm_clears() {
        let base = Instant::now();
        let mut alarm = RespawnRateAlarm::new(WINDOW, 2);

        // First storm: two respawns trip the alarm.
        assert_eq!(alarm.record(base), None);
        assert_eq!(alarm.record(base + Duration::from_secs(1)), Some(2));

        // Long quiet gap empties the window. The first respawn of the second
        // storm is alone in the window → no fire yet (and it re-arms).
        let t = base + Duration::from_secs(500);
        assert_eq!(alarm.record(t), None);
        // Second respawn of the new storm trips it again.
        assert_eq!(alarm.record(t + Duration::from_secs(1)), Some(2));
    }

    /// A threshold of 1 fires on the very first respawn.
    #[test]
    fn threshold_one_fires_immediately() {
        let base = Instant::now();
        let mut alarm = RespawnRateAlarm::new(WINDOW, 1);

        assert_eq!(alarm.record(base), Some(1));
    }

    /// Without a repeat interval the alarm speaks once per storm — today's
    /// behaviour, and the one `PersistentWorker` relies on. Asserted rather
    /// than trusted, because the repeat field is what could silently change it.
    #[test]
    fn without_a_repeat_a_sustained_storm_fires_exactly_once() {
        let base = Instant::now();
        let mut alarm = RespawnRateAlarm::new(WINDOW, 2);

        assert_eq!(alarm.record(base), None);
        assert_eq!(alarm.record(base + Duration::from_secs(1)), Some(2));
        // Hours later, still storming: still silent, because no repeat was set.
        assert_eq!(alarm.record(base + Duration::from_secs(2)), None);
        assert_eq!(alarm.record(base + Duration::from_secs(3)), None);
    }

    /// With a repeat interval a storm that will not clear speaks again, instead
    /// of a multi-day flap producing one line on the first day and silence
    /// after. Mirrors `DowntimeEscalator`'s threshold/repeat pair.
    #[test]
    fn a_repeat_interval_makes_a_sustained_storm_speak_again() {
        let base = Instant::now();
        let mut alarm =
            RespawnRateAlarm::new(WINDOW, 2).with_repeat(Duration::from_secs(30));

        assert_eq!(alarm.record(base), None);
        assert_eq!(alarm.record(base + Duration::from_secs(1)), Some(2));
        // Inside the repeat interval: still one line, not one per event.
        assert_eq!(alarm.record(base + Duration::from_secs(10)), None);
        assert_eq!(alarm.record(base + Duration::from_secs(30)), None);
        // 30 s after the LAST firing (t=1), so this one speaks.
        assert_eq!(alarm.record(base + Duration::from_secs(31)), Some(5));
    }

    /// The repeat is measured from the previous firing, not from the storm's
    /// start — otherwise the second line would arrive at a fixed offset and
    /// every one after it would be a burst.
    #[test]
    fn the_repeat_interval_is_measured_from_the_last_firing() {
        let base = Instant::now();
        let mut alarm =
            RespawnRateAlarm::new(WINDOW, 2).with_repeat(Duration::from_secs(30));

        alarm.record(base);
        assert_eq!(alarm.record(base + Duration::from_secs(1)), Some(2));
        assert_eq!(alarm.record(base + Duration::from_secs(31)), Some(3));
        // 31 s after the first firing but only 0 s after the second: silent.
        assert_eq!(alarm.record(base + Duration::from_secs(32)), None);
        assert_eq!(alarm.record(base + Duration::from_secs(61)), Some(4));
    }

    /// `in_storm` is the latch the audit gate reads. It must be false before
    /// the alarm trips, true after, and false again once the storm clears —
    /// the last of those is what lets the first death of a FRESH storm be
    /// recorded instead of being suppressed by a stale latch.
    #[test]
    fn in_storm_tracks_the_latch_across_a_storm_that_clears() {
        let base = Instant::now();
        let mut alarm = RespawnRateAlarm::new(WINDOW, 2);

        assert!(!alarm.in_storm(), "a fresh alarm is not in a storm");
        alarm.record(base);
        assert!(!alarm.in_storm(), "below threshold is not a storm");
        alarm.record(base + Duration::from_secs(1));
        assert!(alarm.in_storm(), "the alarm latches when it fires");

        // A long gap empties the window, so the next event is alone in it.
        alarm.record(base + Duration::from_secs(500));
        assert!(!alarm.in_storm(), "the latch clears once the storm has cleared");
    }
}
