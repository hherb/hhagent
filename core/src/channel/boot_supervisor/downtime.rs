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

/// How long a channel must stay up before its eventual death is treated as a
/// fresh outage rather than a continuation of the last one (#517).
///
/// Deliberately equal to the backoff cap: "stayed up longer than the longest
/// retry delay" is the same threshold read from either side.
pub const STABLE_UPTIME: Duration = Duration::from_secs(60);

/// Tracks how long a channel has been failing and answers one question:
/// should *this* failure be reported loudly? — plus, since #517, where the
/// current outage begins and ends.
pub struct DowntimeEscalator {
    /// Continuous downtime required before the first escalation.
    threshold: Duration,
    /// Minimum gap between escalations, once the first has fired.
    repeat: Duration,
    /// How long a channel must stay up for its death to open a *new* outage.
    stable_uptime: Duration,
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
    /// per `repeat`. Uses the default [`STABLE_UPTIME`].
    pub fn new(threshold: Duration, repeat: Duration) -> Self {
        Self {
            threshold,
            repeat,
            stable_uptime: STABLE_UPTIME,
            first_failure: None,
            last_escalated: None,
        }
    }

    /// Override how long counts as "stayed up" (see
    /// [`ran_long_enough`](Self::ran_long_enough)). Exists so a test can make a
    /// death stable or flapping without waiting a minute, the same reason
    /// `threshold` and `repeat` are parameters rather than constants.
    pub fn with_stable_uptime(mut self, stable_uptime: Duration) -> Self {
        self.stable_uptime = stable_uptime;
        self
    }

    /// Did a channel that has now died run long enough to count as having
    /// *worked*?
    ///
    /// This is the flap guard, and without it supervising liveness would be a
    /// worse bug than the one it fixes: a channel whose pumps die on startup
    /// would reset the failure counter on every attempt and restart at full
    /// speed forever, spawning a sandboxed worker per iteration. Answering
    /// `false` keeps the death inside the current outage, so the backoff keeps
    /// growing to its cap and the escalator keeps counting — a flapping channel
    /// is treated exactly like one that will not come up, which is what it is.
    ///
    /// Pure, and owns no clock: the caller measures `ran`.
    pub fn ran_long_enough(&self, ran: Duration) -> bool {
        ran >= self.stable_uptime
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

    /// Record that the channel had been working, ending the current outage
    /// (#517).
    ///
    /// Called when a channel that stayed up past [`ran_long_enough`](Self::ran_long_enough)
    /// dies — **not** when one starts. That looks backwards and is not: a start
    /// proves nothing (a flapping channel starts every time round the loop, and
    /// resetting there would silence the escalator exactly when it is needed),
    /// whereas an uptime the caller has already measured is evidence. The death
    /// is simply the first moment that evidence exists.
    ///
    /// Before liveness supervision this could not happen: bring-up succeeded
    /// once and the loop ended, so the escalator only ever counted upward.
    /// Now a channel can come back and later die again, and without this the
    /// second outage would be timed from the *first* one's opening failure —
    /// reporting hours of "downtime" the channel actually spent working, which
    /// is precisely the number an operator would act on.
    ///
    /// Clearing `last_escalated` too is what re-arms the first escalation: a
    /// fresh outage earns a fresh loud line rather than being silenced by a
    /// repeat interval left over from the previous one.
    pub fn record_success(&mut self) {
        self.first_failure = None;
        self.last_escalated = None;
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

    /// A channel that ran past the threshold worked, whatever killed it later.
    #[test]
    fn an_uptime_past_the_threshold_is_stable() {
        let esc = DowntimeEscalator::new(THRESHOLD, REPEAT);
        assert!(esc.ran_long_enough(STABLE_UPTIME + Duration::from_secs(1)));
        assert!(esc.ran_long_enough(Duration::from_secs(3600)));
    }

    /// Exactly at the threshold counts as stable — the boundary is inclusive,
    /// stated here so a later `>` versus `>=` edit is a test failure rather
    /// than a silent policy change.
    #[test]
    fn an_uptime_exactly_at_the_threshold_is_stable() {
        assert!(DowntimeEscalator::new(THRESHOLD, REPEAT).ran_long_enough(STABLE_UPTIME));
    }

    /// The flap: up for a moment, then dead again. Treating this as success
    /// would reset the backoff on every iteration and spin.
    #[test]
    fn an_uptime_below_the_threshold_is_a_flap() {
        let esc = DowntimeEscalator::new(THRESHOLD, REPEAT);
        assert!(!esc.ran_long_enough(Duration::ZERO));
        assert!(!esc.ran_long_enough(STABLE_UPTIME - Duration::from_millis(1)));
    }

    /// The override a test uses to make a death stable (or not) without
    /// waiting out the real minute.
    #[test]
    fn with_stable_uptime_overrides_the_default() {
        let esc = DowntimeEscalator::new(THRESHOLD, REPEAT).with_stable_uptime(Duration::ZERO);
        assert!(esc.ran_long_enough(Duration::ZERO), "a zero threshold makes every death stable");

        let esc = DowntimeEscalator::new(THRESHOLD, REPEAT).with_stable_uptime(Duration::MAX);
        assert!(
            !esc.ran_long_enough(Duration::from_secs(86_400)),
            "an unreachable threshold makes every death a flap"
        );
    }

    /// The channel came back, so the next outage is a NEW one: its downtime is
    /// measured from its own first failure, not from the previous outage's.
    #[test]
    fn record_success_times_the_next_outage_from_itself() {
        let base = Instant::now();
        let mut esc = DowntimeEscalator::new(THRESHOLD, REPEAT);

        esc.record_failure(base);
        esc.record_success();

        // A failure 1000 s after the ORIGINAL outage opened, but the first of
        // this one: well inside the threshold, so still quiet.
        assert_eq!(esc.record_failure(base + Duration::from_secs(1000)), None);
        // And it escalates on ITS own schedule — threshold past 1000, not past 0.
        assert_eq!(
            esc.record_failure(base + Duration::from_secs(1301)),
            Some(Duration::from_secs(301))
        );
    }

    /// Recovery also re-arms the loud line. Without clearing `last_escalated`,
    /// a channel that had already escalated, recovered, and gone down again
    /// would stay silent through the new outage until the old repeat interval
    /// happened to elapse — quietest exactly when something is clearly wrong.
    #[test]
    fn record_success_re_arms_the_first_escalation() {
        let base = Instant::now();
        let mut esc = DowntimeEscalator::new(THRESHOLD, REPEAT);

        esc.record_failure(base);
        assert!(esc.record_failure(base + Duration::from_secs(301)).is_some());
        esc.record_success();

        // New outage, 100 s later; it must escalate after ITS threshold even
        // though the repeat interval since the last escalation has not elapsed.
        esc.record_failure(base + Duration::from_secs(401));
        assert_eq!(
            esc.record_failure(base + Duration::from_secs(702)),
            Some(Duration::from_secs(301))
        );
    }

    /// Recovery with no outage in progress is a no-op, not a panic — the
    /// supervisor calls it on the death of every channel that had been working,
    /// including the first, when there may have been no outage at all.
    #[test]
    fn record_success_on_a_healthy_escalator_is_harmless() {
        let base = Instant::now();
        let mut esc = DowntimeEscalator::new(THRESHOLD, REPEAT);

        esc.record_success();

        assert_eq!(esc.record_failure(base), None);
    }
}
