# Channel Boot Supervision Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A transient failure while bringing a channel up must not disable that channel for the life of the daemon ([#514](https://github.com/hherb/kastellan/issues/514)).

**Architecture:** A reusable `ChannelSupervisor` owns an unbounded retry loop over a caller-supplied `attempt()` future. The two boot modules (`main/matrix_boot.rs`, `main/email_boot.rs`) stop deciding policy and instead classify what happened into a `BootOutcome{NotConfigured|Started|Retry|Fatal}`. Delays come from the existing pure `RestartBackoff`; a pure `DowntimeEscalator` decides when a long outage deserves a louder line; two new audit actions make downtime answerable after the fact.

**Tech Stack:** Rust 2021 (workspace `rust-version = 1.78`), tokio, `futures::future::BoxFuture`, `tracing`, sqlx/Postgres (audit sink only).

**Spec:** `docs/superpowers/specs/2026-08-04-channel-boot-supervision-design.md`

## Global Constraints

- Cross-platform: no `cfg(target_os)` code is added by this plan. Every new file compiles and every new test runs on both Linux and macOS.
- The supervisor loop is DB-free and network-free; the Postgres audit sink lives in its own file behind a boxed-closure seam.
- No new dependency. `futures`, `tokio`, `tracing`, `anyhow` are already workspace deps of `kastellan-core`.
- Files stay under 500 lines; tests live in a sibling `tests.rs` where a module already has one.
- `cargo clippy --workspace --all-targets -- -D warnings` must stay clean.
- Cargo is not on the non-interactive PATH: `source "$HOME/.cargo/env"` first. Iterate on the DGX (`ssh dgx '<cmd>'`) — the Mac IDE's rust-analyzer holds `target/debug/.cargo-lock`.

## File Structure

| File | Responsibility |
| --- | --- |
| `core/src/channel/boot_supervisor.rs` (create) | `BootOutcome`, `StartedChannel`, `BootAudit`/`BootAuditSink`, `ChannelSupervisor` + the retry loop. |
| `core/src/channel/boot_supervisor/downtime.rs` (create) | Pure `DowntimeEscalator` — no clock, no I/O. |
| `core/src/channel/boot_supervisor/pg_sink.rs` (create) | `pg_boot_audit_sink(pool, channel)` — the only DB-aware part. |
| `core/src/channel/boot_supervisor/tests.rs` (create) | Supervisor behaviour tests with scripted outcomes. |
| `core/src/channel/audit_text.rs` (create) | Pure `cap_chars(text, cap)`, lifted from `email_boot::cap_reason` so both callers share one bound. |
| `core/src/channel/mod.rs` (modify) | `pub mod boot_supervisor; pub mod audit_text;` + `BOOT_STARTED` / `BOOT_FAILED` action constants. |
| `core/src/main/matrix_boot.rs` (modify) | `attempt()` → `BootOutcome`; `supervise()` builds the supervisor. |
| `core/src/main/email_boot.rs` (modify) | Same; `cap_reason` delegates to `audit_text::cap_chars`. |
| `core/src/main.rs` (modify) | Hold two `ChannelSupervisor`s; shut both down unconditionally. |

---

### Task 1: Pure `DowntimeEscalator`

**Files:**
- Create: `core/src/channel/boot_supervisor/downtime.rs`
- Modify: `core/src/channel/mod.rs` (add `pub mod boot_supervisor;` — a stub `boot_supervisor.rs` containing only `pub mod downtime;` until Task 3 fills it in)

**Interfaces:**
- Consumes: nothing.
- Produces: `DowntimeEscalator::new(threshold: Duration, repeat: Duration) -> Self`, `DowntimeEscalator::default() -> Self` (5 min / 30 min), `record_failure(&mut self, now: Instant) -> Option<Duration>`.

- [ ] **Step 1: Write the failing tests** (in the same file, `#[cfg(test)] mod tests`)

```rust
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
    /// been down — measured from the FIRST failure, not this one.
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
    /// interval elapses — a 60s backoff must not produce a line per minute.
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
        assert_eq!(
            esc.record_failure(base + Duration::from_secs(301 + 1800)),
            Some(Duration::from_secs(301 + 1800))
        );
    }

    /// A threshold of zero escalates on the very first failure — the degenerate
    /// case a caller might configure for a channel that must never be down.
    #[test]
    fn zero_threshold_escalates_immediately() {
        let base = Instant::now();
        let mut esc = DowntimeEscalator::new(Duration::ZERO, REPEAT);
        assert_eq!(esc.record_failure(base), Some(Duration::ZERO));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `ssh dgx 'source $HOME/.cargo/env && cd ~/src/kastellan && cargo test -p kastellan-core --lib channel::boot_supervisor::downtime'`
Expected: FAIL — `cannot find type DowntimeEscalator`.

- [ ] **Step 3: Write the implementation**

```rust
//! Pure escalation policy for a channel that will not come up.
//!
//! The supervisor retries forever with capped backoff, which is right for
//! availability but wrong for attention: once the delay caps out, "still
//! failing" would produce one identical `warn!` per minute for as long as the
//! outage lasts, and an operator who tunes that out has learned to ignore the
//! one signal that says the bot is deaf (#514).
//!
//! This type decides *when* a failure deserves a louder line. It is
//! deliberately shaped like [`crate::channel::respawn_alarm::RespawnRateAlarm`]:
//! a state machine over caller-supplied [`Instant`]s that owns no clock and
//! spawns nothing, so the driver decides when "now" is and the policy is
//! unit-testable without threads or sleeps.

use std::time::{Duration, Instant};

/// Escalate after this much continuous downtime. Comfortably longer than a
/// restart-window blip (the #514 trigger, absorbed within seconds by the
/// backoff) so the loud line means "this is not resolving by itself".
pub const DEFAULT_THRESHOLD: Duration = Duration::from_secs(300);

/// Once escalated, repeat at most this often.
pub const DEFAULT_REPEAT: Duration = Duration::from_secs(1800);

/// Tracks how long a channel has been failing and answers one question:
/// should this failure be reported loudly?
pub struct DowntimeEscalator {
    /// Continuous downtime required before the first escalation.
    threshold: Duration,
    /// Minimum gap between escalations once the first has fired.
    repeat: Duration,
    /// When the current outage started (`None` ⇒ no failure recorded yet).
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

    /// Record a failed bring-up attempt at `now`.
    ///
    /// Returns `Some(downtime)` — measured from the *first* failure of this
    /// outage, which is the number an operator actually wants ("deaf for 4
    /// hours", not "failed again") — when this failure should be reported
    /// loudly, and `None` when the caller's ordinary per-attempt `warn!` is
    /// enough.
    ///
    /// `now` is expected to be monotonically non-decreasing across calls (it
    /// is in the supervisor, where it is always `Instant::now()`).
    pub fn record_failure(&mut self, now: Instant) -> Option<Duration> {
        let first = *self.first_failure.get_or_insert(now);
        let downtime = now.saturating_duration_since(first);
        if downtime < self.threshold {
            return None;
        }
        // Past the threshold: fire, unless we already fired recently.
        if let Some(last) = self.last_escalated {
            if now.saturating_duration_since(last) < self.repeat {
                return None;
            }
        }
        self.last_escalated = Some(now);
        Some(downtime)
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `ssh dgx 'source $HOME/.cargo/env && cd ~/src/kastellan && cargo test -p kastellan-core --lib channel::boot_supervisor::downtime'`
Expected: PASS, 5 tests.

- [ ] **Step 5: Commit**

```bash
git add core/src/channel/boot_supervisor.rs core/src/channel/boot_supervisor/downtime.rs core/src/channel/mod.rs
git commit -m "feat(channel): pure DowntimeEscalator for channel bring-up failures (#514)"
```

---

### Task 2: Lift `cap_reason` into a shared pure module

**Files:**
- Create: `core/src/channel/audit_text.rs`
- Modify: `core/src/channel/mod.rs` (add `pub mod audit_text;`)
- Modify: `core/src/main/email_boot.rs` (delegate `cap_reason`, drop the moved tests)

**Interfaces:**
- Produces: `pub fn cap_chars(text: &str, cap: usize) -> String` — returns `text` unchanged when it is at or under `cap` chars, else the first `cap` chars plus `"...(truncated)"`, always cut on a `char` boundary.

- [ ] **Step 1: Write the failing tests** (`core/src/channel/audit_text.rs`, `#[cfg(test)] mod tests` — these are `email_boot`'s four `cap_reason` tests, generalised over the cap)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const CAP: usize = 256;

    #[test]
    fn short_input_passes_through_unchanged() {
        assert_eq!(cap_chars("no usable From address", CAP), "no usable From address");
    }

    #[test]
    fn input_at_exactly_the_cap_passes_through_unchanged() {
        let at_cap = "a".repeat(CAP);
        assert_eq!(cap_chars(&at_cap, CAP), at_cap);
    }

    #[test]
    fn oversized_input_is_truncated_and_marked() {
        let huge = format!("localmail 500: {}", "x".repeat(5_000));
        let capped = cap_chars(&huge, CAP);
        assert!(capped.chars().count() <= CAP + "...(truncated)".len());
        assert!(capped.ends_with("...(truncated)"), "{capped}");
        assert!(huge.len() > capped.len(), "must actually shrink an oversized value");
    }

    #[test]
    fn truncation_lands_on_a_char_boundary_not_mid_utf8_codepoint() {
        let multibyte = "€".repeat(CAP + 10);
        let capped = cap_chars(&multibyte, CAP); // would panic on a mid-codepoint byte slice
        assert!(capped.starts_with('€'));
    }

    #[test]
    fn a_zero_cap_keeps_only_the_marker() {
        assert_eq!(cap_chars("anything", 0), "...(truncated)");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `ssh dgx 'source $HOME/.cargo/env && cd ~/src/kastellan && cargo test -p kastellan-core --lib channel::audit_text'`
Expected: FAIL — `cannot find function cap_chars`.

- [ ] **Step 3: Write the implementation**

```rust
//! Bounding text that is about to become a durable `audit_log` payload value.
//!
//! Audit payloads are capped as a whole by `kastellan_db::audit::truncate_payload`,
//! but that cap replaces the *entire* payload with a hash — which is the right
//! backstop and the wrong outcome for a row whose remaining fields (channel,
//! attempt number) are the useful part. Bounding the one unbounded field first
//! keeps the row readable.
//!
//! Defence in depth, not belt-and-braces: the values that reach here originate
//! outside the core (an upstream HTTP error body, a transport error string),
//! and a sink must not trust the producer to keep bounding them.

/// Marker appended to a value this function shortened, so a reader can tell
/// truncation from a genuinely terse message.
pub const TRUNCATION_MARKER: &str = "...(truncated)";

/// Return `text` unchanged when it is at most `cap` **chars**, else its first
/// `cap` chars followed by [`TRUNCATION_MARKER`].
///
/// Counts and cuts by `char`, never by byte, so a multi-byte codepoint
/// straddling the cap can neither panic nor produce invalid UTF-8.
///
/// Pure: no I/O, no global state. Same input → same output.
pub fn cap_chars(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let mut capped: String = text.chars().take(cap).collect();
    capped.push_str(TRUNCATION_MARKER);
    capped
}
```

- [ ] **Step 4: Point `email_boot::cap_reason` at it**

In `core/src/main/email_boot.rs`, replace the body of `cap_reason` and delete the four now-duplicated tests (they moved to `audit_text`, generalised):

```rust
fn cap_reason(reason: &str) -> String {
    kastellan_core::channel::audit_text::cap_chars(reason, AUDIT_REASON_CAP_CHARS)
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `ssh dgx 'source $HOME/.cargo/env && cd ~/src/kastellan && cargo test -p kastellan-core --lib channel::audit_text && cargo build --workspace'`
Expected: PASS, 5 tests; workspace builds.

- [ ] **Step 6: Commit**

```bash
git add core/src/channel/audit_text.rs core/src/channel/mod.rs core/src/main/email_boot.rs
git commit -m "refactor(channel): lift cap_reason into a shared pure audit_text::cap_chars (#514)"
```

---

### Task 3: `ChannelSupervisor` + `BootOutcome` + `StartedChannel`

**Files:**
- Modify: `core/src/channel/boot_supervisor.rs` (fill in the stub from Task 1)
- Create: `core/src/channel/boot_supervisor/tests.rs`
- Modify: `core/src/channel/mod.rs` (add `BOOT_STARTED` / `BOOT_FAILED` to `pub mod actions`)

**Interfaces:**
- Consumes: `DowntimeEscalator` (Task 1), `cap_chars` (Task 2), `crate::worker_lifecycle::RestartBackoff`, `crate::channel::ChannelBus`.
- Produces:
  - `enum BootOutcome { NotConfigured, Started(StartedChannel), Retry(anyhow::Error), Fatal(anyhow::Error) }`
  - `StartedChannel::new<F, Fut>(shutdown: F) -> StartedChannel` and `StartedChannel::from_bus(bus: ChannelBus) -> StartedChannel`
  - `enum BootAudit { Started { attempts: u32 }, Failed { attempt: u32, retry_in_ms: Option<u64>, fatal: bool, cause: String } }`
  - `type BootAuditSink = Box<dyn Fn(BootAudit) -> BoxFuture<'static, ()> + Send>`
  - `ChannelSupervisor::spawn(label, backoff, escalator, audit, attempt) -> ChannelSupervisor` and `ChannelSupervisor::shutdown(self)` (async)
  - action constants `actions::BOOT_STARTED = "channel.started"`, `actions::BOOT_FAILED = "channel.boot_failed"`

- [ ] **Step 1: Write the failing tests** (`core/src/channel/boot_supervisor/tests.rs`)

```rust
//! Behaviour tests for the bring-up supervisor. Hermetic: no network, no DB,
//! no sandbox — every attempt is a scripted `BootOutcome` and the "channel"
//! is a probe that records whether it was shut down.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;

/// 1 ms base/cap: the retry loop spins fast enough for a test without any
/// test sleeping on a real backoff.
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
}

/// A scripted attempt sequence: each call pops the next outcome.
fn scripted(outcomes: Vec<BootOutcome>) -> impl Fn() -> futures::future::BoxFuture<'static, BootOutcome> {
    let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(outcomes)));
    move || {
        let next = queue.lock().expect("script mutex").pop_front();
        Box::pin(async move { next.expect("attempted more times than the script allows") })
    }
}

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
            BootOutcome::Retry(anyhow::anyhow!("tunnel error")),
            BootOutcome::Started(StartedChannel::new(move || {
                probe.fetch_add(1, Ordering::SeqCst);
                async {}
            })),
        ]),
    );

    // Two failures then success: the started row must report 3 attempts.
    let started = wait_for_started(&sink).await;
    assert_eq!(started, 3);

    sup.shutdown().await;
    assert_eq!(stopped.load(Ordering::SeqCst), 1, "the running channel must be shut down exactly once");
}

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

    // The script holds exactly one outcome, so a second attempt would panic
    // the task. Joining proves the loop stopped rather than retried.
    sup.shutdown().await;
    let events = sink.events();
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], BootAudit::Failed { fatal: true, retry_in_ms: None, .. }));
}

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
    assert!(sink.events().is_empty(), "an absent channel is not an event");
}

#[tokio::test]
async fn shutdown_while_backing_off_returns_promptly() {
    // A 10-minute delay: if shutdown waited for it, this test would hang.
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

    // Wait until the first failure has been recorded, so we are certainly
    // inside the sleep rather than racing the first attempt.
    wait_for_events(&sink, 1).await;
    tokio::time::timeout(Duration::from_secs(5), sup.shutdown())
        .await
        .expect("shutdown must not wait out the backoff delay");
}

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
    wait_for_started(&sink).await;
    sup.shutdown().await;

    let events = sink.events();
    assert_eq!(events.len(), 3);
    match &events[0] {
        BootAudit::Failed { attempt, fatal, cause, retry_in_ms } => {
            assert_eq!(*attempt, 1);
            assert!(!*fatal);
            assert!(cause.contains("first"), "{cause}");
            assert!(retry_in_ms.is_some(), "a retryable failure carries its next delay");
        }
        other => panic!("expected a Failed row, got {other:?}"),
    }
    assert!(matches!(&events[1], BootAudit::Failed { attempt: 2, .. }));
    assert!(matches!(&events[2], BootAudit::Started { attempts: 3 }));
}

/// Poll the sink until a `Started` row appears; returns its attempt count.
async fn wait_for_started(sink: &RecordingSink) -> u32 {
    for _ in 0..500 {
        if let Some(BootAudit::Started { attempts }) = sink
            .events()
            .into_iter()
            .find(|e| matches!(e, BootAudit::Started { .. }))
        {
            return attempts;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("channel never started");
}

/// Poll the sink until at least `n` events have been recorded.
async fn wait_for_events(sink: &RecordingSink, n: usize) {
    for _ in 0..500 {
        if sink.events().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected at least {n} audit events");
}
```

`BootAudit` therefore needs `#[derive(Debug, Clone)]`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `ssh dgx 'source $HOME/.cargo/env && cd ~/src/kastellan && cargo test -p kastellan-core --lib channel::boot_supervisor'`
Expected: FAIL — `cannot find type ChannelSupervisor`.

- [ ] **Step 3: Write the implementation** (`core/src/channel/boot_supervisor.rs`)

```rust
//! Supervised channel bring-up (#514).
//!
//! Bringing a channel up is not one operation but a short chain — spawn a
//! sandboxed worker (and, when egress is force-routed, its 1:1 sidecar), log
//! in, open a LISTEN/NOTIFY connection, start a [`ChannelBus`] — and every
//! link can fail transiently. Before this module each boot module tried the
//! chain exactly once and returned `None`, so a blip in the first seconds of
//! daemon startup left the bot deaf for the life of the process, with every
//! unit `active` and nothing further in the log. That is not hypothetical: it
//! cost 12 hours of missed Matrix messages on 2026-08-03, and the same log
//! line appears on four earlier dates.
//!
//! The fix is the shape already used one layer down, where
//! `worker_lifecycle::PersistentWorker` supervises a worker *after* login:
//! retry with capped exponential backoff, unbounded, because a homeserver can
//! be down for an hour and the daemon should reconnect when it returns rather
//! than need a human.
//!
//! Two things keep that from becoming a different failure:
//!
//! 1. **[`BootOutcome::Fatal`]** — a statically-dead configuration must stop,
//!    not spin. A `localhost`-name homeserver under force-routing (#459) and a
//!    partial `EmailConfig` are both fixed for the lifetime of the process, so
//!    retrying them would be exactly the respawn loop those checks exist to
//!    prevent.
//! 2. **[`downtime::DowntimeEscalator`]** — once the backoff caps out, an
//!    unescalated loop would emit one identical line per minute forever.
//!
//! The loop itself is DB-free and network-free: audit rows go through a boxed
//! closure ([`BootAuditSink`], the idiom
//! [`crate::channel::polled_driver::AckOnlyAudit`] already uses), so the whole
//! module is testable with scripted outcomes and a probe channel.

use std::future::Future;
use std::time::Instant;

use futures::future::BoxFuture;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::channel::audit_text::cap_chars;
use crate::channel::ChannelBus;
use crate::worker_lifecycle::RestartBackoff;

pub mod downtime;
pub mod pg_sink;
pub use downtime::DowntimeEscalator;

#[cfg(test)]
mod tests;

/// Cap on the `cause` string before it becomes a durable `audit_log` value.
/// See [`crate::channel::audit_text`] for why the sink bounds it rather than
/// trusting the producer.
pub const AUDIT_CAUSE_CAP_CHARS: usize = 256;

/// A running channel plus the one thing the supervisor ever does to it: stop
/// it.
///
/// Deliberately opaque. The supervisor never names [`ChannelBus`], which keeps
/// the retry policy independent of the channel layer and lets a test hand it a
/// probe that records whether shutdown ran.
pub struct StartedChannel {
    shutdown: Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>,
}

impl StartedChannel {
    /// Wrap anything whose shutdown is an async, by-value call.
    pub fn new<F, Fut>(shutdown: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self { shutdown: Box::new(move || Box::pin(shutdown())) }
    }

    /// The production case: a running [`ChannelBus`].
    pub fn from_bus(bus: ChannelBus) -> Self {
        Self::new(move || async move { bus.shutdown().await })
    }

    /// Stop the channel. Consuming, so it cannot run twice.
    async fn stop(self) {
        (self.shutdown)().await;
    }
}

impl std::fmt::Debug for StartedChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StartedChannel")
    }
}

/// What one bring-up attempt produced.
///
/// The distinction that matters is [`Retry`](Self::Retry) vs
/// [`Fatal`](Self::Fatal): "could a later attempt plausibly succeed with the
/// same process environment?" A refused sandbox cgroup, an unreachable
/// homeserver and a LISTEN/NOTIFY hiccup are all yes. A missing or malformed
/// env var is no — the environment is fixed for the process's lifetime, so the
/// honest answer is a loud line telling the operator to fix it and restart.
#[derive(Debug)]
pub enum BootOutcome {
    /// The channel is not configured. Stop, and say nothing: this is the
    /// default for every deployment that does not use this channel.
    NotConfigured,
    /// The channel is up.
    Started(StartedChannel),
    /// Failed in a way a later attempt could plausibly absorb.
    Retry(anyhow::Error),
    /// Failed in a way no retry can fix.
    Fatal(anyhow::Error),
}

/// One durable record of a bring-up event.
#[derive(Debug, Clone)]
pub enum BootAudit {
    /// The channel came up, after `attempts` total attempts (1 = first try).
    Started { attempts: u32 },
    /// An attempt failed. `retry_in_ms` is `None` exactly when `fatal`.
    Failed { attempt: u32, retry_in_ms: Option<u64>, fatal: bool, cause: String },
}

/// Where [`BootAudit`] records go. A boxed closure rather than a trait so the
/// supervisor stays DB-free and a test can record into a `Vec`; production is
/// [`pg_sink::pg_boot_audit_sink`].
pub type BootAuditSink = Box<dyn Fn(BootAudit) -> BoxFuture<'static, ()> + Send>;

/// A supervised channel bring-up: the retry loop, plus the handle that stops
/// both it and whatever it started.
pub struct ChannelSupervisor {
    label: String,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: JoinHandle<()>,
}

impl ChannelSupervisor {
    /// Start supervising `attempt`, which is called once per try.
    ///
    /// Returns immediately — the first attempt runs in the spawned task, so a
    /// slow or hanging bring-up never delays daemon startup (the property the
    /// old 60-second login timeout was protecting, now structural).
    ///
    /// * `label` — channel name, used in every log line and audit row.
    /// * `backoff` — delay schedule; `RestartBackoff::default()` is 1 s → ×2 →
    ///   60 s cap, the same schedule supervised workers use.
    /// * `escalator` — decides when a long outage gets a louder line.
    /// * `audit` — `None` disables audit rows entirely (tests, and any caller
    ///   without a pool).
    pub fn spawn<F, Fut>(
        label: impl Into<String>,
        backoff: RestartBackoff,
        escalator: DowntimeEscalator,
        audit: Option<BootAuditSink>,
        attempt: F,
    ) -> Self
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = BootOutcome> + Send + 'static,
    {
        let label = label.into();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let join = tokio::spawn(run(label.clone(), backoff, escalator, audit, attempt, shutdown_rx));
        Self { label, shutdown_tx: Some(shutdown_tx), join }
    }

    /// Stop the supervisor and, if the channel came up, the channel.
    ///
    /// Safe to call at any point in the loop. An attempt already in flight is
    /// abandoned rather than cancelled — identical to the pre-#514 login
    /// timeout, which already left its `spawn_blocking` task draining against
    /// the SDK's own HTTP timeouts, and harmless because every worker is
    /// spawned `--die-with-parent`.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            // An `Err` means the loop already returned (not configured, or
            // fatal) and dropped the receiver — nothing to signal.
            let _ = tx.send(());
        }
        if let Err(e) = self.join.await {
            warn!(channel = %self.label, error = %e, "channel supervisor task did not join cleanly");
        }
    }
}

/// The retry loop. Split out of [`ChannelSupervisor::spawn`] so the generic
/// bounds sit in one place and the body reads top-to-bottom.
async fn run<F, Fut>(
    label: String,
    backoff: RestartBackoff,
    mut escalator: DowntimeEscalator,
    audit: Option<BootAuditSink>,
    attempt: F,
    mut shutdown: oneshot::Receiver<()>,
) where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = BootOutcome> + Send + 'static,
{
    // Number of failed attempts so far; also the `RestartBackoff` exponent, so
    // the first retry waits `base` rather than `base * factor`.
    let mut failures: u32 = 0;

    loop {
        let outcome = tokio::select! {
            // `biased` so a shutdown that arrives together with an outcome
            // wins: at that point the daemon is going away regardless.
            biased;
            _ = &mut shutdown => return,
            outcome = attempt() => outcome,
        };

        match outcome {
            BootOutcome::NotConfigured => return,

            BootOutcome::Started(channel) => {
                let attempts = failures + 1;
                info!(channel = %label, attempts, "channel bus running");
                emit(&audit, BootAudit::Started { attempts }).await;
                // Park until the daemon shuts down; then stop the channel.
                // Ignoring the result is deliberate: a dropped sender means
                // the handle went away, which we treat as shutdown.
                let _ = (&mut shutdown).await;
                channel.stop().await;
                return;
            }

            BootOutcome::Fatal(e) => {
                error!(
                    channel = %label,
                    error = %format!("{e:#}"),
                    "CHANNEL DISABLED — it did NOT start and will NOT be retried, because no \
                     retry can fix what `error` names. The rest of the daemon is running \
                     normally. Fix it, then restart the daemon."
                );
                emit(
                    &audit,
                    BootAudit::Failed {
                        attempt: failures + 1,
                        retry_in_ms: None,
                        fatal: true,
                        cause: cap_chars(&format!("{e:#}"), AUDIT_CAUSE_CAP_CHARS),
                    },
                )
                .await;
                return;
            }

            BootOutcome::Retry(e) => {
                let delay = backoff.next_delay(failures);
                failures += 1;
                warn!(
                    channel = %label,
                    attempt = failures,
                    retry_in_ms = delay.as_millis() as u64,
                    error = %format!("{e:#}"),
                    "channel bring-up failed; retrying"
                );
                emit(
                    &audit,
                    BootAudit::Failed {
                        attempt: failures,
                        retry_in_ms: Some(delay.as_millis() as u64),
                        fatal: false,
                        cause: cap_chars(&format!("{e:#}"), AUDIT_CAUSE_CAP_CHARS),
                    },
                )
                .await;
                if let Some(down) = escalator.record_failure(Instant::now()) {
                    error!(
                        channel = %label,
                        down_secs = down.as_secs(),
                        attempts = failures,
                        "CHANNEL STILL DOWN — no message sent to this channel has been received \
                         for this long, and bring-up is still failing. The daemon is otherwise \
                         healthy; see the `error` on the preceding attempts."
                    );
                }
                tokio::select! {
                    biased;
                    _ = &mut shutdown => return,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

/// Call the audit sink when there is one. Awaited rather than spawned so the
/// rows land in attempt order and a test can assert on them deterministically.
async fn emit(audit: &Option<BootAuditSink>, event: BootAudit) {
    if let Some(sink) = audit {
        sink(event).await;
    }
}
```

- [ ] **Step 4: Add the two action constants** to `core/src/channel/mod.rs`'s `pub mod actions`

```rust
    /// A channel bus came up. Payload carries the channel and how many
    /// bring-up attempts it took, so "did it retry?" is answerable after the
    /// fact (#514).
    pub const BOOT_STARTED: &str = "channel.started";
    /// A channel bring-up attempt failed. Payload carries the channel, the
    /// attempt number, the delay before the next attempt (absent when the
    /// failure is fatal and there will be none), and the capped cause. This
    /// is the durable record of a deaf window — the daemon log rotates and
    /// nobody reads it until they notice silence.
    pub const BOOT_FAILED: &str = "channel.boot_failed";
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `ssh dgx 'source $HOME/.cargo/env && cd ~/src/kastellan && cargo test -p kastellan-core --lib channel::boot_supervisor'`
Expected: PASS — 5 supervisor tests + the 5 downtime tests.

- [ ] **Step 6: Commit**

```bash
git add core/src/channel/boot_supervisor.rs core/src/channel/boot_supervisor/tests.rs core/src/channel/mod.rs
git commit -m "feat(channel): ChannelSupervisor retries channel bring-up with backoff (#514)"
```

---

### Task 4: The Postgres audit sink

**Files:**
- Create: `core/src/channel/boot_supervisor/pg_sink.rs`

**Interfaces:**
- Consumes: `BootAudit`, `BootAuditSink` (Task 3); `kastellan_db::audit::insert`.
- Produces: `pub fn pg_boot_audit_sink(pool: sqlx::PgPool, channel: &str) -> BootAuditSink`.

- [ ] **Step 1: Write the implementation** (no unit test: this function is one `match` over a payload shape plus an insert; it is covered where it matters by the PG-gated audit e2es, and a hermetic test here would assert `serde_json` against itself)

```rust
//! The production [`BootAuditSink`]: one `audit_log` row per bring-up event.
//!
//! Split out of the supervisor so the retry loop itself stays DB-free (and
//! therefore unit-testable without a cluster). Failures to *write* the audit
//! row are logged and swallowed: an unavailable Postgres must not stop the
//! supervisor from retrying the channel, which is the whole point of #514.

use futures::future::BoxFuture;
use sqlx::PgPool;
use tracing::warn;

use super::{BootAudit, BootAuditSink};
use crate::channel::actions;

/// Build the sink for one channel. `channel` is captured, so every row carries
/// it without the supervisor having to thread it through.
pub fn pg_boot_audit_sink(pool: PgPool, channel: &str) -> BootAuditSink {
    let channel = channel.to_string();
    Box::new(move |event: BootAudit| {
        let pool = pool.clone();
        let channel = channel.clone();
        Box::pin(async move {
            let (action, payload) = match event {
                BootAudit::Started { attempts } => (
                    actions::BOOT_STARTED,
                    serde_json::json!({ "channel": channel, "attempts": attempts }),
                ),
                BootAudit::Failed { attempt, retry_in_ms, fatal, cause } => (
                    actions::BOOT_FAILED,
                    serde_json::json!({
                        "channel": channel,
                        "attempt": attempt,
                        "retry_in_ms": retry_in_ms,
                        "fatal": fatal,
                        "cause": cause,
                    }),
                ),
            };
            if let Err(e) = kastellan_db::audit::insert(&pool, "channel", action, payload).await {
                warn!(error = %e, action, "channel bring-up audit insert failed (non-fatal)");
            }
        }) as BoxFuture<'static, ()>
    })
}
```

- [ ] **Step 2: Verify it compiles and clippy is clean**

Run: `ssh dgx 'source $HOME/.cargo/env && cd ~/src/kastellan && cargo clippy -p kastellan-core --all-targets -- -D warnings'`
Expected: exit 0.

- [ ] **Step 3: Commit**

```bash
git add core/src/channel/boot_supervisor/pg_sink.rs
git commit -m "feat(channel): audit_log sink for channel bring-up events (#514)"
```

---

### Task 5: `matrix_boot` becomes an attempt + a supervisor

**Files:**
- Modify: `core/src/main/matrix_boot.rs`

**Interfaces:**
- Consumes: `BootOutcome`, `StartedChannel`, `ChannelSupervisor`, `pg_boot_audit_sink`, `DowntimeEscalator`, `RestartBackoff`.
- Produces: `pub(crate) fn supervise_matrix_channel(pool: &PgPool, sandboxes: &SandboxBackends, force_routing: &Option<Arc<ForceRoutingConfig>>) -> ChannelSupervisor` (replaces `spawn_matrix_channel`).

- [ ] **Step 1: Write the failing test** — the one classification the fix depends on, added at the bottom of the file

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// #459's static-death check must classify as FATAL, not Retry: a
    /// `localhost`-NAME homeserver under force-routing can never succeed (the
    /// proxy resolves the name to loopback and range-denies every CONNECT), so
    /// retrying it forever is precisely the respawn loop that check exists to
    /// prevent.
    #[test]
    fn a_force_routed_localhost_homeserver_is_fatal_not_retryable() {
        let outcome = classify_homeserver("http://localhost:8008", true);
        assert!(matches!(outcome, Some(BootOutcome::Fatal(_))), "{outcome:?}");
    }

    /// The same URL without force-routing is reachable, so nothing is refused.
    #[test]
    fn a_localhost_homeserver_without_force_routing_is_not_refused() {
        assert!(classify_homeserver("http://localhost:8008", false).is_none());
    }

    /// A real homeserver is never refused up front.
    #[test]
    fn a_real_homeserver_is_not_refused() {
        assert!(classify_homeserver("https://matrix.kastellan.dev", true).is_none());
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `ssh dgx 'source $HOME/.cargo/env && cd ~/src/kastellan && cargo test -p kastellan-core --bin kastellan matrix_boot'`
Expected: FAIL — `cannot find function classify_homeserver`.

- [ ] **Step 3: Refactor the module**

Extract the #459 guard into the pure helper the test names, turn the body into `attempt`, and add the supervisor constructor. The `#[cfg]` backend selection, the `MatrixEgress` wiring and the 60-second login timeout are unchanged — only the four `error!(… "channel not started")` arms change, into `BootOutcome` values.

```rust
/// Pure: refuse a homeserver that can never work, before spending an attempt
/// on it. `Some(Fatal)` ⇒ do not start and do not retry; `None` ⇒ proceed.
///
/// Split out of [`attempt`] so the classification — the thing #514's fix
/// depends on being FATAL rather than retryable — is testable without a
/// sandbox, a pool or a homeserver.
fn classify_homeserver(homeserver_url: &str, forced: bool) -> Option<BootOutcome> {
    kastellan_core::channel::matrix::forced_localhost_homeserver(homeserver_url, forced)
        .map(|detail| BootOutcome::Fatal(anyhow::anyhow!("{detail}")))
}

/// One Matrix bring-up attempt. See [`supervise_matrix_channel`] for the
/// retry policy around it.
async fn attempt(
    pool: PgPool,
    sandboxes: SandboxBackends,
    force_routing: Option<Arc<ForceRoutingConfig>>,
) -> BootOutcome {
    let Some(spawn_cfg) = kastellan_core::channel::matrix::daemon_spawn_config_from_env(
        std::env::current_exe().ok().as_deref().and_then(|p| p.parent()),
    ) else {
        return BootOutcome::NotConfigured;
    };

    #[cfg(target_os = "linux")]
    let vm_mode = spawn_cfg.use_microvm;
    #[cfg(not(target_os = "linux"))]
    let vm_mode = false;
    if let Some(fatal) = classify_homeserver(&spawn_cfg.homeserver_url, force_routing.is_some() || vm_mode) {
        return fatal;
    }

    // … unchanged backend/egress selection, using `&sandboxes` / `&force_routing` …

    let spawn = tokio::task::spawn_blocking(move || {
        kastellan_core::channel::matrix::spawn_matrix_worker(
            backend,
            kastellan_core::channel::ChannelId("matrix".to_string()),
            &spawn_cfg,
            egress,
        )
    });
    const MATRIX_LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    let worker = match tokio::time::timeout(MATRIX_LOGIN_TIMEOUT, spawn).await {
        Ok(Ok(Ok(worker))) => worker,
        Ok(Ok(Err(e))) => return BootOutcome::Retry(e.context("matrix worker spawn/login failed")),
        Ok(Err(join_err)) => {
            return BootOutcome::Retry(anyhow::anyhow!("matrix worker spawn task panicked: {join_err}"))
        }
        Err(_elapsed) => {
            return BootOutcome::Retry(anyhow::anyhow!(
                "matrix worker login timed out ({}s)",
                MATRIX_LOGIN_TIMEOUT.as_secs()
            ))
        }
    };

    info!(identity = %worker.identity, "matrix worker logged in; starting channel bus");
    let authorizer = Arc::new(kastellan_core::channel::auth::DbPeerAuthorizer::new(pool.clone()));
    let pairing = Arc::new(kastellan_core::channel::pairing::DbPairingService::new(pool.clone()));
    let events = Arc::new(kastellan_core::channel::bus::PgChannelEvents::new(pool.clone()));
    match kastellan_core::channel::bus::PgCompletedTasks::connect(pool.clone()).await {
        Ok(completed) => BootOutcome::Started(StartedChannel::from_bus(ChannelBus::spawn(
            vec![Box::new(worker.channel)],
            authorizer,
            Some(pairing),
            events,
            Box::new(completed),
        ))),
        Err(e) => BootOutcome::Retry(
            anyhow::Error::new(e).context("matrix: PgCompletedTasks::connect (LISTEN/NOTIFY) failed"),
        ),
    }
}

/// Supervise the Matrix channel: retry [`attempt`] with capped backoff until it
/// comes up, forever, unless it is unconfigured or statically dead.
///
/// Returns immediately — the first attempt runs inside the supervisor task, so
/// a hung homeserver no longer delays daemon startup either.
pub(crate) fn supervise_matrix_channel(
    pool: &PgPool,
    sandboxes: &SandboxBackends,
    force_routing: &Option<Arc<ForceRoutingConfig>>,
) -> ChannelSupervisor {
    let pool = pool.clone();
    let sandboxes = sandboxes.clone();
    let force_routing = force_routing.clone();
    ChannelSupervisor::spawn(
        "matrix",
        RestartBackoff::default(),
        DowntimeEscalator::default(),
        Some(pg_boot_audit_sink(pool.clone(), "matrix")),
        move || attempt(pool.clone(), sandboxes.clone(), force_routing.clone()),
    )
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `ssh dgx 'source $HOME/.cargo/env && cd ~/src/kastellan && cargo test -p kastellan-core --bin kastellan'`
Expected: PASS (3 new tests + the existing `bootstrap_tests`).

- [ ] **Step 5: Commit**

```bash
git add core/src/main/matrix_boot.rs
git commit -m "fix(channel): supervise Matrix bring-up instead of trying once (#514)"
```

---

### Task 6: `email_boot` + `main.rs` wiring

**Files:**
- Modify: `core/src/main/email_boot.rs`
- Modify: `core/src/main.rs:488-512`

**Interfaces:**
- Produces: `pub(crate) fn supervise_email_channel(pool: &PgPool, sandboxes: &SandboxBackends, force_routing: &Option<Arc<ForceRoutingConfig>>) -> ChannelSupervisor` (replaces `spawn_email_channel`).

- [ ] **Step 1: Write the failing test** (bottom of `email_boot.rs`, alongside the surviving `cap_reason` delegation)

```rust
    /// A PARTIAL config must be FATAL: the process environment is fixed for
    /// this daemon's lifetime, so no number of retries can complete it. The
    /// existing message already tells the operator to fix it and restart —
    /// retrying instead would make that message a lie and spin forever.
    #[test]
    fn a_partial_config_is_fatal_not_retryable() {
        let err = anyhow::anyhow!("KASTELLAN_EMAIL_AUTHSERV_ID is not set");
        assert!(matches!(classify_config_error(err), BootOutcome::Fatal(_)));
    }

    /// A worker spawn failure is RETRYABLE — it is the observed #514 trigger
    /// (`systemd-run --scope` refusing to create the sandbox cgroup while the
    /// user manager restarts), which the next attempt absorbs.
    #[test]
    fn a_worker_spawn_failure_is_retryable() {
        let err = anyhow::anyhow!("egress-proxy sidecar exited before becoming ready");
        assert!(matches!(classify_spawn_error(err), BootOutcome::Retry(_)));
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `ssh dgx 'source $HOME/.cargo/env && cd ~/src/kastellan && cargo test -p kastellan-core --bin kastellan email_boot'`
Expected: FAIL — `cannot find function classify_config_error`.

- [ ] **Step 3: Refactor `email_boot`**

```rust
/// Pure: a config error can never be fixed without an operator edit plus a
/// restart, because the process environment is immutable for this daemon's
/// lifetime. Fatal, therefore — and the existing message already says exactly
/// that.
fn classify_config_error(e: anyhow::Error) -> BootOutcome {
    BootOutcome::Fatal(e.context("email channel configuration is incomplete or invalid"))
}

/// Pure: a spawn failure is a sandbox/egress condition, not a config one — the
/// #514 trigger was `systemd-run --scope` refusing a cgroup during a user-manager
/// restart, which the next attempt absorbs.
fn classify_spawn_error(e: anyhow::Error) -> BootOutcome {
    BootOutcome::Retry(e.context("the email worker failed to start"))
}
```

`attempt` mirrors Task 5: `Ok(None)` ⇒ `NotConfigured`; `Err(e)` ⇒ `classify_config_error(e)`; `spawn_email_worker` `Err` ⇒ `classify_spawn_error(e)`; `PgCompletedTasks::connect` `Err` ⇒ `BootOutcome::Retry`; success ⇒ `BootOutcome::Started(StartedChannel::from_bus(bus))`. `log_channel_disabled` is deleted — the supervisor's `Fatal` arm now emits the equivalent loud line, and keeping both would double-log. `supervise_email_channel` mirrors `supervise_matrix_channel` with the label `"email"`.

- [ ] **Step 4: Rewire `main.rs`**

```rust
    // ── Channel bus (comms slice #2 — Matrix). ──
    // Supervised bring-up (#514): unset env ⇒ the supervisor task returns
    // immediately and the daemon is byte-identical to a Matrix-less build.
    // Otherwise it retries with capped backoff until the channel comes up —
    // a transient failure in the startup window no longer deafens the bot for
    // the life of the process. Statically-dead configs still stop, loudly.
    // See `main/matrix_boot.rs`.
    let matrix = matrix_boot::supervise_matrix_channel(&pool, &sandboxes, &force_routing);

    // ── Channel bus (Phase 2 slice #5 — email fallback). ── Same supervision;
    // a partial config is FATAL (the environment cannot change under a running
    // daemon), a worker-spawn failure is retried. See `main/email_boot.rs`.
    let email = email_boot::supervise_email_channel(&pool, &sandboxes, &force_routing);

    bootstrap::wait_for_shutdown().await?;

    // Stop the channel supervisors first — each stops its bus if it started
    // one, so no further inbound messages are enqueued and each worker's stdin
    // closes (clean worker exit). Unconditional now: a supervisor that never
    // started a channel shuts down to a no-op.
    matrix.shutdown().await;
    email.shutdown().await;
```

- [ ] **Step 5: Run the full crate test suite**

Run: `ssh dgx 'source $HOME/.cargo/env && cd ~/src/kastellan && cargo test -p kastellan-core --lib --bins 2>&1 | tail -20'`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add core/src/main/email_boot.rs core/src/main.rs
git commit -m "fix(channel): supervise email bring-up; wire both supervisors into main (#514)"
```

---

### Task 7: Full gate

- [ ] **Step 1: Run the whole workspace on the DGX**, writing the log to `$HOME` (never `/tmp` — it is scrubbed mid-run on both hosts) with an explicit exit-code line and a DONE sentinel

```bash
ssh dgx 'source $HOME/.cargo/env && cd ~/src/kastellan && \
  (cargo test --workspace -- --nocapture > $HOME/514-test.log 2>&1; echo "TEST_EXIT=$?" >> $HOME/514-test.log; \
   cargo clippy --workspace --all-targets -- -D warnings > $HOME/514-clippy.log 2>&1; echo "CLIPPY_EXIT=$?" >> $HOME/514-clippy.log; \
   echo DONE >> $HOME/514-test.log)'
```

Expected: `TEST_EXIT=0`, `CLIPPY_EXIT=0`, `[SKIP]` count exactly 4 (all `KASTELLAN_GLINER_RELEX_ENABLE`), and the passed count up by exactly the number of tests added (5 downtime + 5 supervisor + 5 audit_text − 4 moved out of `email_boot` + 3 matrix classification + 2 email classification = **+16**).

- [ ] **Step 2: Cross-check the count** against the `f57db609`/`fba4102c` baseline of 2965 — expect **2981 / 0 / 53**. A different number means a test was added or lost unintentionally; find out which before proceeding.

- [ ] **Step 3: Verify on the Mac** that nothing platform-specific slipped in (no `cfg` was added, so this is a compile+clippy check, run under a private `CARGO_TARGET_DIR` because rust-analyzer holds the workspace lock)

```bash
source "$HOME/.cargo/env" && CARGO_TARGET_DIR=$HOME/.cache/kastellan-514-target \
  cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: exit 0.
