//! Behaviour tests for the bring-up supervisor.
//!
//! Hermetic: no network, no database, no sandbox. Every attempt is a scripted
//! [`BootOutcome`] and the "channel" is a probe that records whether it was
//! shut down — which is what makes the retry *policy* (the thing #514 is
//! about) testable at all, independently of anything a channel does.
//!
//! Split by concern: [`bringup`] is #514 (a channel that will not start),
//! [`liveness`] is #517 (one that started and then stopped), and
//! [`reporting`] is #518/#522/#523 (what gets said and stored about either). The
//! shared scaffolding lives here because all three drive the same loop.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;

mod bringup;
mod liveness;
mod reporting;

// Helpers are defined here and used from the submodules; `pub(super)` on each
// item is what makes `use super::*;` work in them.

/// 1 ms base and cap, so the retry loop spins fast enough for a test without
/// any test waiting on a realistic backoff.
pub(super) fn fast_backoff() -> RestartBackoff {
    RestartBackoff {
        base: Duration::from_millis(1),
        factor_num: 1,
        factor_den: 1,
        cap: Duration::from_millis(1),
    }
}

/// Records every audit event the supervisor emits, in order.
#[derive(Clone, Default)]
pub(super) struct RecordingSink(Arc<Mutex<Vec<BootAudit>>>);

impl RecordingSink {
    pub(super) fn sink(&self) -> BootAuditSink {
        let events = Arc::clone(&self.0);
        Box::new(move |ev| {
            events.lock().expect("audit sink mutex").push(ev);
            Box::pin(async {})
        })
    }

    pub(super) fn events(&self) -> Vec<BootAudit> {
        self.0.lock().expect("audit sink mutex").clone()
    }

    /// Poll until at least `n` events have been recorded. Polling rather than
    /// sleeping a fixed time keeps the test fast when the loop is fast and
    /// non-flaky when the machine is loaded.
    pub(super) async fn wait_for(&self, n: usize) {
        for _ in 0..500 {
            if self.events().len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected at least {n} audit events, saw {:?}", self.events());
    }

    /// Poll until a `Started` row appears; returns its attempt count.
    pub(super) async fn wait_for_started(&self) -> u32 {
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
pub(super) fn scripted(
    outcomes: Vec<BootOutcome>,
) -> impl Fn() -> futures::future::BoxFuture<'static, BootOutcome> + Send + 'static {
    let queue = Arc::new(Mutex::new(VecDeque::from(outcomes)));
    move || {
        let next = queue.lock().expect("script mutex").pop_front();
        Box::pin(async move { next.expect("attempted more times than the script allows") })
    }
}

/// A backoff whose delays actually grow, so a test can tell "backing off" from
/// "spinning". 20 ms doubling, cap far away.
pub(super) fn growing_backoff() -> RestartBackoff {
    RestartBackoff {
        base: Duration::from_millis(20),
        factor_num: 2,
        factor_den: 1,
        cap: Duration::from_secs(10),
    }
}

/// A channel that is dead as soon as the supervisor looks at it — the scripted
/// stand-in for a bus whose pump returned.
pub(super) fn dying(stopped: &Arc<AtomicUsize>) -> BootOutcome {
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
pub(super) fn healthy(stopped: &Arc<AtomicUsize>) -> BootOutcome {
    let probe = Arc::clone(stopped);
    BootOutcome::Started(StartedChannel::new(move || {
        probe.fetch_add(1, Ordering::SeqCst);
        async {}
    }))
}

/// Every `retry_in_ms` from the `Died` rows recorded so far, in order.
pub(super) fn death_delays(events: &[BootAudit]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|e| match e {
            BootAudit::Died { retry_in_ms, .. } => Some(*retry_in_ms),
            _ => None,
        })
        .collect()
}
