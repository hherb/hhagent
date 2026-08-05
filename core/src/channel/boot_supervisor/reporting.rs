//! When does a restart-worthy channel event earn an operator's attention, and
//! when does it earn a durable `audit_log` row?
//!
//! Those are the same question, and this module is the only place either is
//! answered. Splitting an operator-facing policy across call sites is how the
//! answers drift — #516 and #521 each found one instance of exactly that, in
//! this feature's own documentation.
//!
//! The retry loop in [`super`] stays deliberately ignorant of the policy: it
//! reports one event, receives a [`Verdict`], and does what it says. It owns
//! the channel label and therefore the logging; this module owns the deciding.

use std::time::{Duration, Instant};

use super::DowntimeEscalator;

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

/// Update the outage bookkeeping for one event and answer "does this one earn
/// the loud line?".
///
/// Kept as a free function, and `pub` within the crate, because the sequence
/// that matters here — died, recovered, worked for hours, flapped — has no
/// other test seam: escalation is a log line and nothing else, so without this
/// it is unobservable to a test.
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

/// Everything the supervisor needs to decide what to say and store about one
/// event.
///
/// Holds the alarms rather than exposing them, so the retry loop cannot reach
/// past the policy and ask one of them directly — which is how the row and the
/// line would drift apart again.
pub struct ReportingPolicy {
    escalator: DowntimeEscalator,
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
        Self { escalator }
    }

    /// Override how long a channel must stay up for its death to count as
    /// having worked. Delegates to [`DowntimeEscalator::with_stable_uptime`];
    /// re-exposed here so a test that used to build the escalator directly
    /// changes by one type name.
    pub fn with_stable_uptime(mut self, stable_uptime: Duration) -> Self {
        self.escalator = self.escalator.with_stable_uptime(stable_uptime);
        self
    }

    /// Did a channel that has now died run long enough to count as having
    /// worked? The flap guard — see [`DowntimeEscalator::ran_long_enough`].
    pub fn ran_long_enough(&self, ran: Duration) -> bool {
        self.escalator.ran_long_enough(ran)
    }

    /// Fold one event into the outage bookkeeping and answer whether it earns
    /// the loud line.
    pub(super) fn note_outage(&mut self, outage: Outage, now: Instant) -> Option<Duration> {
        note_outage(&mut self.escalator, outage, now)
    }
}
