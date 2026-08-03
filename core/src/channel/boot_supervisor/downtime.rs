//! Pure escalation policy for a channel that will not come up.
//!
//! The supervisor retries forever with capped backoff, which is right for
//! availability but wrong for attention: once the delay caps out, "still
//! failing" would produce one identical `warn!` per minute for as long as the
//! outage lasts, and an operator who learns to tune that out has tuned out the
//! one signal that says the bot is deaf (#514).
//!
//! This type decides *when* a failure deserves a louder line. It is
//! deliberately shaped like [`crate::channel::respawn_alarm::RespawnRateAlarm`]:
//! a state machine over caller-supplied [`Instant`]s that owns no clock and
//! spawns nothing, so the driver decides when "now" is and the policy is
//! unit-testable without threads or sleeps.

use std::time::{Duration, Instant};

/// Escalate after this much continuous downtime. Comfortably longer than a
/// restart-window blip — the observed #514 trigger, which the backoff absorbs
/// within seconds — so the loud line means "this is not resolving by itself".
pub const DEFAULT_THRESHOLD: Duration = Duration::from_secs(300);

/// Once escalated, repeat at most this often. Long enough that an hours-long
/// outage is a handful of lines, short enough that the log never goes quiet
/// while the channel is still down.
pub const DEFAULT_REPEAT: Duration = Duration::from_secs(1800);

/// Tracks how long a channel has been failing and answers one question:
/// should *this* failure be reported loudly?
pub struct DowntimeEscalator {
    /// Continuous downtime required before the first escalation.
    threshold: Duration,
    /// Minimum gap between escalations, once the first has fired.
    repeat: Duration,
    /// When the current outage started. `None` until the first failure.
    first_failure: Option<Instant>,
    /// When the last escalation fired, if any.
    last_escalated: Option<Instant>,
}

impl Default for DowntimeEscalator {
    fn default() -> Self {
        Self::new(DEFAULT_THRESHOLD, DEFAULT_REPEAT)
    }
}

impl DowntimeEscalator {
    /// Escalate after `threshold` of continuous downtime, then at most once
    /// per `repeat`.
    pub fn new(threshold: Duration, repeat: Duration) -> Self {
        Self { threshold, repeat, first_failure: None, last_escalated: None }
    }

    /// Record a failed bring-up attempt that happened at `now`.
    ///
    /// Returns `Some(downtime)` when this failure should be reported loudly,
    /// and `None` when the caller's ordinary per-attempt `warn!` is enough.
    ///
    /// `downtime` is measured from the **first** failure of this outage, not
    /// from the previous one — "deaf for four hours" is the number an operator
    /// acts on; "failed again" is not.
    ///
    /// `now` is expected to be monotonically non-decreasing across calls (it
    /// always is in the supervisor, where it is `Instant::now()`); an
    /// out-of-order value is tolerated via saturating arithmetic and simply
    /// reads as zero elapsed.
    pub fn record_failure(&mut self, now: Instant) -> Option<Duration> {
        let first = *self.first_failure.get_or_insert(now);
        let downtime = now.saturating_duration_since(first);
        if downtime < self.threshold {
            return None;
        }
        // Past the threshold — fire, unless we fired recently enough that
        // another line would be noise rather than news.
        if let Some(last) = self.last_escalated {
            if now.saturating_duration_since(last) < self.repeat {
                return None;
            }
        }
        self.last_escalated = Some(now);
        Some(downtime)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THRESHOLD: Duration = Duration::from_secs(300);
    const REPEAT: Duration = Duration::from_secs(1800);

    /// Failures inside the threshold are the normal transient case: the
    /// backoff loop is doing its job and nobody needs a louder line yet.
    #[test]
    fn failures_within_the_threshold_do_not_escalate() {
        let base = Instant::now();
        let mut esc = DowntimeEscalator::new(THRESHOLD, REPEAT);

        assert_eq!(esc.record_failure(base), None);
        assert_eq!(esc.record_failure(base + Duration::from_secs(120)), None);
    }

    /// The first failure past the threshold reports how long the channel has
    /// been down — measured from the FIRST failure, not from this one.
    #[test]
    fn first_failure_past_the_threshold_escalates_with_total_downtime() {
        let base = Instant::now();
        let mut esc = DowntimeEscalator::new(THRESHOLD, REPEAT);

        assert_eq!(esc.record_failure(base), None);
        assert_eq!(
            esc.record_failure(base + Duration::from_secs(301)),
            Some(Duration::from_secs(301))
        );
    }

    /// Having escalated once, a sustained outage stays quiet until the repeat
    /// interval elapses — a 60 s backoff must not produce a line per minute.
    #[test]
    fn further_failures_inside_the_repeat_interval_stay_silent() {
        let base = Instant::now();
        let mut esc = DowntimeEscalator::new(THRESHOLD, REPEAT);

        esc.record_failure(base);
        assert!(esc.record_failure(base + Duration::from_secs(301)).is_some());
        assert_eq!(esc.record_failure(base + Duration::from_secs(400)), None);
        assert_eq!(esc.record_failure(base + Duration::from_secs(2000)), None);
    }

    /// Past the repeat interval it speaks again, so an hours-long outage never
    /// goes fully silent.
    #[test]
    fn escalation_repeats_once_the_repeat_interval_elapses() {
        let base = Instant::now();
        let mut esc = DowntimeEscalator::new(THRESHOLD, REPEAT);

        esc.record_failure(base);
        assert!(esc.record_failure(base + Duration::from_secs(301)).is_some());
        // 301 + 1800: the repeat interval has now elapsed since that
        // escalation, and the reported downtime is still measured from `base`.
        assert_eq!(
            esc.record_failure(base + Duration::from_secs(2101)),
            Some(Duration::from_secs(2101))
        );
    }

    /// A zero threshold escalates on the very first failure — the degenerate
    /// configuration for a channel that must never be down at all.
    #[test]
    fn zero_threshold_escalates_immediately() {
        let base = Instant::now();
        let mut esc = DowntimeEscalator::new(Duration::ZERO, REPEAT);

        assert_eq!(esc.record_failure(base), Some(Duration::ZERO));
    }
}
