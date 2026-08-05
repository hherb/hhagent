# Channel Event Reporting Gates Implementation Plan

> **As shipped (2026-08-06):** this plan is a historical execution artifact and was
> executed with **one deviation**: gating `channel.started` — the architecture
> paragraph's "`channel.died` / `channel.started`" and the whole "**`Started` arm** —
> gate the row" step below — was a design error, caught mid-branch and **reversed**.
> `channel.started` shipped **ungated**: the latch a gate would read is only cleared
> by a *later* death, so the start that ends a storm would be suppressed with the
> ones inside it, leaving `channel.died` as the last durable row for a healthy
> channel. See the spec's "Why `channel.started` is not gated" section for the full
> rationale, and `a_start_during_a_latched_storm_is_still_recorded` for the pin.
> Everything else shipped as written. Do not transcribe the `Started`-gating text.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop the channel supervisor writing an unbounded stream of identical audit rows during an outage (#518), and give it an alarm that notices a channel flapping in the 60 s–300 s uptime band that nothing currently escalates (#522).

**Architecture:** One predicate — `should_record(alarm_latched, alarm_spoke_now)` — gates the durable row for every recurring event, driven by the alarm that owns that event's regime: the existing `DowntimeEscalator` for `channel.boot_failed`, and a new supervisor-owned `RespawnRateAlarm` for `channel.died` / `channel.started`. Both alarms and the predicate live behind one `ReportingPolicy` in a new `boot_supervisor/reporting.rs`, so the retry loop never touches either alarm directly.

**Tech Stack:** Rust 2021, `tokio` (multi-thread and `current_thread` test runtimes), `tracing`, `anyhow`. No new dependencies.

**Spec:** [`docs/superpowers/specs/2026-08-05-channel-event-reporting-gates-design.md`](../specs/2026-08-05-channel-event-reporting-gates-design.md)

## Global Constraints

- **Read the spec before starting.** Every "why" question this plan does not answer is answered there.
- **Cargo is not on the `PATH` for non-interactive shells.** Every task's commands assume `source "$HOME/.cargo/env"` has been run first.
- **Run cargo in the FOREGROUND. Never background a `cargo test`/`cargo clippy` and wait on it**, and never pipe it through `| tail` (that masks the exit code and buffers output).
- **On the Mac, use a private `CARGO_TARGET_DIR` under `$HOME`** — e.g. `export CARGO_TARGET_DIR="$HOME/.cache/kastellan-518-target"`. The IDE's rust-analyzer holds `target/debug/.cargo-lock`, and a `CARGO_TARGET_DIR` under `/tmp` gets scrubbed mid-run (a test binary vanishes between build and exec and the failure looks like a code defect).
- **Clippy is enforced:** `cargo clippy --workspace --all-targets -- -D warnings` must stay at exit 0. The tree is warning-clean; keep it that way.
- **TDD is mandatory.** Write the failing test, run it, see it fail *for the stated reason*, then implement. A test that passes before the implementation is not a test yet.
- **Keep files under 500 lines.** `boot_supervisor.rs` is at 451 today and this plan keeps it there by moving code out, not by squeezing.
- **Inline documentation is mandatory and must be readable by a junior contributor** — say *why*, not *what*. Match the density of the surrounding code, which is unusually high in this module by deliberate convention.
- **Every commit stages named files.** Use `git add <specific paths>`, never `git add -A`.
- **Branch:** `fix/518-522-channel-event-reporting-gates`, off `main`.
- **No `cfg(target_os)` code anywhere in this diff.** Both hosts must see the identical suite; that is what makes the final test-count prediction a meaningful cross-check.

## File Structure

| file | responsibility after this plan |
| --- | --- |
| `core/src/channel/respawn_alarm.rs` | pure sliding-window rate alarm. Gains an optional repeat interval and a `in_storm()` accessor. Knows nothing about channels. |
| `core/src/channel/boot_supervisor/downtime.rs` | pure continuous-downtime escalator. Gains a `has_escalated()` accessor. Unchanged otherwise. |
| `core/src/channel/boot_supervisor/reporting.rs` | **new.** All reporting policy: `Outage`, `note_outage`, `should_record`, `ReportingPolicy`, `Verdict`, the flap-alarm constants and the two operator-facing log phrases. |
| `core/src/channel/boot_supervisor.rs` | the retry loop only. Holds a `ReportingPolicy`, asks it one question per event, and does the logging (it owns `label`). |
| `core/src/channel/boot_supervisor/tests/{mod,bringup,liveness,reporting}.rs` | the current `tests.rs`, split by concern, plus the new gating tests. |
| `core/src/main/{matrix_boot,email_boot}.rs` | one line each: `DowntimeEscalator::default()` → `ReportingPolicy::default()`. |
| `core/src/install/plan.rs` | operator help text: the rows are rate-limited, so it must stop claiming every attempt is durable. |

---

### Task 1: `RespawnRateAlarm` gains a repeat interval and a storm accessor

**Files:**
- Modify: `core/src/channel/respawn_alarm.rs`
- Test: `core/src/channel/respawn_alarm.rs` (its own `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: `RespawnRateAlarm::with_repeat(self, repeat: Duration) -> Self` and `RespawnRateAlarm::in_storm(&self) -> bool`. `RespawnRateAlarm::new(window: Duration, threshold: usize) -> Self` and `record(&mut self, now: Instant) -> Option<usize>` keep their existing signatures.

- [ ] **Step 1: Write the failing tests**

Append to the existing `mod tests` in `core/src/channel/respawn_alarm.rs`:

```rust
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
        assert_eq!(alarm.record(base + Duration::from_secs(31)), Some(4));
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
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
cargo test -p kastellan-core --lib channel::respawn_alarm
```

Expected: FAIL to **compile**, with `no method named with_repeat` and `no method named in_storm`. A compile failure is the correct RED here — there is nothing to call yet.

- [ ] **Step 3: Implement**

In `core/src/channel/respawn_alarm.rs`, add two fields to the struct (with doc comments matching the surrounding density):

```rust
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
```

Initialise both in `new` (`repeat: None`, `last_fired: None`) and add the builder:

```rust
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
    pub fn in_storm(&self) -> bool {
        self.armed
    }
```

Replace the tail of `record` (everything after `let count = self.recent.len();`) with:

```rust
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
```

Update the struct-level doc comment: "fires once" is now "fires once per storm unless a repeat interval is configured", and note the second consumer in the module doc (the channel `boot_supervisor`, for deaths rather than worker respawns).

- [ ] **Step 4: Run the tests to verify they pass**

```sh
cargo test -p kastellan-core --lib channel::respawn_alarm
cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: all `respawn_alarm` tests PASS (the 5 pre-existing plus the 4 new), clippy exit 0.

- [ ] **Step 5: Commit**

```bash
git add core/src/channel/respawn_alarm.rs
git commit -m "feat(channel): optional repeat interval and in_storm latch on RespawnRateAlarm

Additive: the default None repeat is today's fire-once-per-storm behaviour,
so PersistentWorker is unchanged. Both are for the channel supervisor's
death-rate alarm (#522).

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `DowntimeEscalator` gains `has_escalated()`

**Files:**
- Modify: `core/src/channel/boot_supervisor/downtime.rs`
- Test: `core/src/channel/boot_supervisor/downtime.rs` (its own `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: `DowntimeEscalator::has_escalated(&self) -> bool`.

- [ ] **Step 1: Write the failing tests**

Append to the existing `mod tests` in `core/src/channel/boot_supervisor/downtime.rs`:

```rust
    /// The latch the audit gate reads for the bring-up stream: has this outage
    /// already been reported loudly? Below the threshold it has not, so every
    /// attempt is still worth a durable row.
    #[test]
    fn has_escalated_is_false_until_the_outage_is_reported() {
        let base = Instant::now();
        let mut esc = DowntimeEscalator::new(THRESHOLD, REPEAT);

        assert!(!esc.has_escalated(), "a fresh escalator has said nothing");
        esc.record_failure(base);
        assert!(!esc.has_escalated(), "inside the threshold it is still quiet");
        assert!(esc.record_failure(base + Duration::from_secs(301)).is_some());
        assert!(esc.has_escalated(), "it has now reported this outage");
    }

    /// Recovery clears the latch, so the NEXT outage records its early attempts
    /// in full rather than inheriting the previous outage's silence.
    #[test]
    fn record_success_clears_the_escalated_latch() {
        let base = Instant::now();
        let mut esc = DowntimeEscalator::new(THRESHOLD, REPEAT);

        esc.record_failure(base);
        assert!(esc.record_failure(base + Duration::from_secs(301)).is_some());
        assert!(esc.has_escalated());

        esc.record_success();
        assert!(!esc.has_escalated(), "a recovered channel starts its next outage unreported");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
cargo test -p kastellan-core --lib boot_supervisor::downtime
```

Expected: FAIL to compile with `no method named has_escalated`.

- [ ] **Step 3: Implement**

Add to `impl DowntimeEscalator` in `core/src/channel/boot_supervisor/downtime.rs`:

```rust
    /// Has this outage already been reported loudly?
    ///
    /// The supervisor's audit gate reads this to decide whether a failed
    /// attempt still earns a durable row: until the outage has been escalated,
    /// every attempt does, and after it only the escalations do. That is what
    /// keeps a 24-hour outage to ~57 rows instead of ~1440 (#518) without
    /// inventing a "first N attempts" constant nobody could derive.
    ///
    /// Cleared by [`record_success`](Self::record_success), so a fresh outage
    /// records its early attempts in full.
    pub fn has_escalated(&self) -> bool {
        self.last_escalated.is_some()
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

```sh
cargo test -p kastellan-core --lib boot_supervisor::downtime
cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: all `downtime` tests PASS (13 pre-existing plus the 2 new), clippy exit 0.

- [ ] **Step 5: Commit**

```bash
git add core/src/channel/boot_supervisor/downtime.rs
git commit -m "feat(channel): expose DowntimeEscalator::has_escalated for the audit gate (#518)

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Split `boot_supervisor/tests.rs` by concern

Pure movement, before anything changes, so the split diff is reviewable on its own and the new tests in Task 5 have a home.

**Files:**
- Delete: `core/src/channel/boot_supervisor/tests.rs` (719 lines)
- Create: `core/src/channel/boot_supervisor/tests/mod.rs`
- Create: `core/src/channel/boot_supervisor/tests/bringup.rs`
- Create: `core/src/channel/boot_supervisor/tests/liveness.rs`
- Create: `core/src/channel/boot_supervisor/tests/reporting.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: test helpers `fast_backoff()`, `growing_backoff()`, `RecordingSink` (with `sink()`, `events()`, `wait_for(n)`, `wait_for_started()`), `scripted(Vec<BootOutcome>)`, `dying(&Arc<AtomicUsize>)`, `healthy(&Arc<AtomicUsize>)`, `death_delays(&[BootAudit])` — all `pub(super)`-visible from `tests/mod.rs` to its submodules. **No test body changes in this task**, only their location and `use` lines.

- [ ] **Step 1: Create `tests/mod.rs` with the shared helpers**

Move verbatim from the old `tests.rs`: the module doc (lines 1–6), the `use` block (8–13), `fast_backoff` (15–24), `RecordingSink` + its impl (26–68), `scripted` (73–81), `growing_backoff` (325–334), `dying` (336–347), `healthy` (349–356), `death_delays` (358–367).

Then declare the submodules and re-export the helpers so the submodules can reach them:

```rust
mod bringup;
mod liveness;
mod reporting;

// Helpers are defined here and used from the submodules; `pub(super)` on each
// item is what makes `use super::*;` work in them.
```

Change each helper's visibility from private to `pub(super)` — `fast_backoff`, `growing_backoff`, `scripted`, `dying`, `healthy`, `death_delays`, `RecordingSink`, and `RecordingSink`'s four methods. Keep `use super::*;` at the top so `BootAudit`, `BootOutcome`, `ChannelSupervisor`, `StartedChannel` and `RestartBackoff` resolve exactly as they do today.

Extend the module doc with one sentence naming the split:

```rust
//! Split by concern: [`bringup`] is #514 (a channel that will not start),
//! [`liveness`] is #517 (one that started and then stopped), and
//! [`reporting`] is #518/#522 (what gets said and stored about either). The
//! shared scaffolding lives here because all three drive the same loop.
```

- [ ] **Step 2: Create `tests/bringup.rs`**

Move verbatim (old `tests.rs` lines 83–316): `retries_until_the_channel_comes_up`, `a_fatal_outcome_stops_the_loop`, `an_unconfigured_channel_stops_silently`, `shutdown_while_backing_off_returns_promptly`, `every_failed_attempt_is_audited_with_its_attempt_number`, `a_supervisor_without_an_audit_sink_still_starts_and_stops_the_channel`, `shutdown_before_the_first_poll_starts_no_attempt`.

Header:

```rust
//! Bring-up (#514): a channel that will not start must keep being retried, and
//! a statically-dead configuration must not be.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::*;
```

Prune any `use` that the moved bodies do not need — clippy `-D warnings` will name each unused one.

- [ ] **Step 3: Create `tests/liveness.rs`**

Move verbatim (old `tests.rs` lines 369–636, i.e. everything after the helper block in the liveness section): `a_channel_that_dies_is_brought_back_up`, `a_death_is_audited_separately_from_a_failed_bring_up`, `a_flapping_channel_backs_off_instead_of_spinning`, `a_channel_that_ran_long_enough_restarts_at_the_base_delay`, `a_death_racing_shutdown_is_not_recorded_as_a_death`, `a_first_try_recovery_reports_one_attempt`, `a_recovery_after_a_flap_keeps_counting`.

Header: keep the existing section comment (old lines 318–323) as the module doc, then the same `use` block shape as `bringup.rs` plus `use std::sync::Mutex;` (needed by `a_death_racing_shutdown_is_not_recorded_as_a_death`).

- [ ] **Step 4: Create `tests/reporting.rs`**

Move verbatim (old `tests.rs` lines 638–719): `a_death_that_recovers_leaves_no_outage_open`, `a_death_whose_restart_fails_times_the_outage_from_that_failure`, `a_flapping_death_extends_the_outage_it_is_already_in`.

Header: keep the existing section comment (old lines 638–643) as the module doc, then:

```rust
use std::time::Duration;

use super::*;
```

- [ ] **Step 5: Run the tests to verify nothing changed**

```sh
cargo test -p kastellan-core --lib channel::boot_supervisor
cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: the **same test count as before the split** (17 in this module) and all PASS. A different count means a test was dropped in the move — go find it. Clippy exit 0.

- [ ] **Step 6: Verify the file sizes**

```sh
wc -l core/src/channel/boot_supervisor/tests/*.rs
```

Expected: every file well under 500 lines.

- [ ] **Step 7: Commit**

```bash
git add core/src/channel/boot_supervisor/tests.rs core/src/channel/boot_supervisor/tests/
git commit -m "refactor(channel): split boot_supervisor tests by concern (bringup/liveness/reporting)

Pure movement, no test body changes: the file was 719 lines against the
500-line cap and is about to grow. Same test count before and after.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Extract `reporting.rs` and introduce `ReportingPolicy` (no behaviour change)

A refactor with the existing tests as its safety net. The gating logic arrives in Task 5.

**Files:**
- Create: `core/src/channel/boot_supervisor/reporting.rs`
- Modify: `core/src/channel/boot_supervisor.rs`
- Modify: `core/src/main/matrix_boot.rs:224`
- Modify: `core/src/main/email_boot.rs:287`
- Modify: `core/src/channel/boot_supervisor/tests/{mod,liveness,reporting}.rs` (call sites and imports)

**Interfaces:**
- Consumes: `DowntimeEscalator::{new, default, with_stable_uptime, ran_long_enough, record_failure, record_success}` from Task 2's file.
- Produces:
  - `pub enum Outage { Continues, Ends }`
  - `pub fn note_outage(escalator: &mut DowntimeEscalator, outage: Outage, now: Instant) -> Option<Duration>`
  - `pub struct ReportingPolicy` with `Default`, `new(escalator: DowntimeEscalator) -> Self`, `with_stable_uptime(self, d: Duration) -> Self`, `ran_long_enough(&self, ran: Duration) -> bool`
  - `ChannelSupervisor::spawn`'s third parameter changes type from `DowntimeEscalator` to `ReportingPolicy`.

- [ ] **Step 1: Create `reporting.rs` with the moved code**

Create `core/src/channel/boot_supervisor/reporting.rs`:

```rust
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
```

- [ ] **Step 2: Strip the moved code out of `boot_supervisor.rs`**

Delete `enum Outage` (current lines 355–383), `fn escalate_if_due` (385–405) and `fn note_outage` (407–429). Add `pub mod reporting;` beside the other `pub mod` declarations, and re-export: `pub use reporting::{Outage, ReportingPolicy};`.

Replace the deleted `escalate_if_due` with a version that takes the policy:

```rust
/// Emit the loud line if this event has earned one.
///
/// Shared by the two arms that can fail — a bring-up that will not succeed and
/// a channel that keeps dying — because from an operator's side they are the
/// same event: the channel has been unusable for this long and is not fixing
/// itself.
fn escalate_if_due(
    policy: &mut ReportingPolicy,
    label: &str,
    outage: Outage,
    attempts: u32,
) {
    if let Some(down) = policy.note_outage(outage, Instant::now()) {
        error!(
            channel = %label,
            down_secs = down.as_secs(),
            attempts,
            "CHANNEL STILL DOWN — nothing sent to this channel has been received for this long, \
             and it is still not staying up. The daemon is otherwise healthy; the cause is on \
             the preceding attempts' `error` field."
        );
    }
}
```

Change the `escalator: DowntimeEscalator` parameter of both `ChannelSupervisor::spawn` and `run` to `policy: ReportingPolicy` (and `mut policy` in `run`). Update the three internal uses: `escalator.ran_long_enough(ran)` → `policy.ran_long_enough(ran)`, and both `escalate_if_due(&mut escalator, …)` → `escalate_if_due(&mut policy, …)`. Update `spawn`'s doc bullet for the parameter:

```rust
    /// * `policy` — decides when an event earns a louder line and a durable
    ///   row. [`ReportingPolicy::default()`] is the production configuration.
```

- [ ] **Step 3: Update the two production call sites**

`core/src/main/matrix_boot.rs:224` and `core/src/main/email_boot.rs:287`: replace `DowntimeEscalator::default(),` with `ReportingPolicy::default(),`, and change each file's `use` (`DowntimeEscalator` → `ReportingPolicy`) in the `boot_supervisor::{…}` import list at `matrix_boot.rs:33` / `email_boot.rs:75`.

- [ ] **Step 4: Update the test call sites**

In `tests/mod.rs`, `tests/liveness.rs` and `tests/bringup.rs`: every `DowntimeEscalator::default()` becomes `ReportingPolicy::default()`, and `DowntimeEscalator::default().with_stable_uptime(x)` becomes `ReportingPolicy::default().with_stable_uptime(x)`. In `tests/reporting.rs`, the three `note_outage(&mut esc, …)` calls keep working — `Outage` and `note_outage` are re-exported from `super::super`, which `use super::*;` already reaches.

- [ ] **Step 5: Run the tests to verify nothing changed**

```sh
cargo test -p kastellan-core --lib channel::boot_supervisor
cargo test -p kastellan-core --bins
cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: same counts as after Task 3 — this task adds no tests and changes no behaviour. Clippy exit 0.

- [ ] **Step 6: Verify the line budget**

```sh
wc -l core/src/channel/boot_supervisor.rs core/src/channel/boot_supervisor/reporting.rs
```

Expected: `boot_supervisor.rs` around 400 (down from 451), `reporting.rs` around 150. Both well under 500.

- [ ] **Step 7: Commit**

```bash
git add core/src/channel/boot_supervisor.rs core/src/channel/boot_supervisor/reporting.rs core/src/channel/boot_supervisor/tests/ core/src/main/matrix_boot.rs core/src/main/email_boot.rs
git commit -m "refactor(channel): move reporting policy behind a ReportingPolicy type

No behaviour change. Outage/note_outage/escalate_if_due move out of the
supervisor into boot_supervisor/reporting.rs, and the supervisor takes a
ReportingPolicy rather than a bare DowntimeEscalator so the second alarm
(#522) has an owner the retry loop cannot reach past.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: The gate and the flap alarm

**Files:**
- Modify: `core/src/channel/boot_supervisor/reporting.rs`
- Modify: `core/src/channel/boot_supervisor.rs`
- Test: `core/src/channel/boot_supervisor/reporting.rs` (pure unit tests) and `core/src/channel/boot_supervisor/tests/reporting.rs` (loop-level tests)

**Interfaces:**
- Consumes: `RespawnRateAlarm::{new, with_repeat, record, in_storm}` (Task 1), `DowntimeEscalator::has_escalated` (Task 2), `ReportingPolicy` (Task 4).
- Produces:
  - `pub const CHANNEL_FLAPPING_LOG_PHRASE: &str`
  - `pub const FLAP_ALARM_WINDOW: Duration`, `FLAP_ALARM_THRESHOLD: usize`, `FLAP_ALARM_REPEAT: Duration`
  - `pub struct Verdict { pub record: bool, pub still_down: Option<Duration>, pub flapping: Option<usize> }`
  - `pub fn should_record(alarm_latched: bool, alarm_spoke_now: bool) -> bool`
  - `ReportingPolicy::{with_flap_alarm, note_failed_attempt, note_death, should_record_start}`

- [ ] **Step 1: Write the failing pure tests**

Add a `#[cfg(test)] mod tests` to `core/src/channel/boot_supervisor/reporting.rs`:

```rust
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

    /// A start is suppressed only while the flap alarm is latched. A recovery
    /// from a bring-up outage — the most valuable row there is — always lands.
    #[test]
    fn a_start_is_recorded_unless_the_channel_is_in_a_death_storm() {
        let base = Instant::now();
        let mut policy = ReportingPolicy::default().with_flap_alarm(
            RespawnRateAlarm::new(Duration::from_secs(3600), 2),
        );

        assert!(policy.should_record_start(), "no deaths at all: record the start");

        policy.note_failed_attempt(base);
        assert!(policy.should_record_start(), "a bring-up outage does not suppress its recovery");

        policy.note_death(true, base);
        policy.note_death(true, base + Duration::from_secs(61));
        assert!(!policy.should_record_start(), "inside a death storm the start row is redundant");
    }
}
```

Note the test-only accessor `in_flap_storm()` used above; it is defined in Step 3.

- [ ] **Step 2: Run the pure tests to verify they fail**

```sh
cargo test -p kastellan-core --lib boot_supervisor::reporting
```

Expected: FAIL to compile — `should_record`, `Verdict`, `with_flap_alarm`, `note_failed_attempt`, `note_death`, `should_record_start` and `in_flap_storm` do not exist yet.

- [ ] **Step 3: Implement the policy**

In `core/src/channel/boot_supervisor/reporting.rs`, add the import `use crate::channel::respawn_alarm::RespawnRateAlarm;` and:

```rust
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
/// [`DowntimeEscalator::DEFAULT_REPEAT`]'s value, for the same reason: an
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
```

Add the alarm to the struct and its builder:

```rust
pub struct ReportingPolicy {
    escalator: DowntimeEscalator,
    /// Counts deaths across restarts. **Owned here, not inside the retry
    /// loop** — that placement IS the #522 fix. `PersistentWorker` builds its
    /// alarm inside the object a restart replaces, so the window is discarded
    /// every cycle and can never accumulate; a channel restart would do exactly
    /// the same to an alarm the loop owned.
    deaths: RespawnRateAlarm,
}
```

`new` initialises `deaths: RespawnRateAlarm::new(FLAP_ALARM_WINDOW, FLAP_ALARM_THRESHOLD).with_repeat(FLAP_ALARM_REPEAT)`. Then:

```rust
    /// Override the death-rate alarm. Exists so a test can trip it in two
    /// deaths instead of five, the same reason `DowntimeEscalator`'s thresholds
    /// are parameters rather than constants.
    pub fn with_flap_alarm(mut self, deaths: RespawnRateAlarm) -> Self {
        self.deaths = deaths;
        self
    }

    /// Is the channel currently inside a death storm the alarm has reported?
    /// Test-facing; the loop asks [`should_record_start`](Self::should_record_start).
    #[cfg(test)]
    pub(super) fn in_flap_storm(&self) -> bool {
        self.deaths.in_storm()
    }

    /// Fold a failed bring-up attempt into the bookkeeping.
    pub fn note_failed_attempt(&mut self, now: Instant) -> Verdict {
        let still_down = note_outage(&mut self.escalator, Outage::Continues, now);
        // Both inputs are read AFTER recording. See `should_record_start` for
        // why the order is load-bearing.
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

    /// Does a successful start earn a durable row?
    ///
    /// Gated on the death alarm rather than on a start counter because the
    /// supervisor's `failures` count resets to 0 on every *stable* death — so
    /// in the #522 band a start-count gate would never engage at all. Inside a
    /// storm the row is genuinely redundant: the paired `channel.died` row
    /// carries `ran_ms`, which already proves the channel came up and for how
    /// long. Outside one — a recovery from a bring-up outage — it is the most
    /// valuable row there is, and always lands.
    ///
    /// Reads the latch with no recording call of its own, so it reflects the
    /// state left by the previous cycle's death. That is also why every latch
    /// read in this module happens *after* its recording call: `record` is what
    /// clears the latch when a storm has cleared, and a read taken beforehand
    /// would suppress the first event of a fresh storm.
    pub fn should_record_start(&self) -> bool {
        should_record(self.deaths.in_storm(), false)
    }
```

- [ ] **Step 4: Run the pure tests to verify they pass**

```sh
cargo test -p kastellan-core --lib boot_supervisor::reporting
```

Expected: PASS (the 3 moved outage tests are in `tests/reporting.rs`, so this runs the 5 new ones here).

- [ ] **Step 5: Write the failing loop-level tests**

Append to `core/src/channel/boot_supervisor/tests/reporting.rs`:

```rust
/// #522, end to end and through the real loop: the alarm must accumulate
/// ACROSS restarts.
///
/// This is the test that fails if the alarm is ever moved inside the retry
/// loop, which is the mistake `PersistentWorker` makes and the reason a channel
/// restart could never trip its alarm. Three stable deaths with a threshold of
/// two: an alarm rebuilt per iteration would see a count of one, every time,
/// and the third death's row would still be written.
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
    for _ in 0..500 {
        if stopped.load(Ordering::SeqCst) >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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

/// The `channel.started` half of the same amplification: inside a death storm
/// the start row says nothing the paired `channel.died` row's `ran_ms` does not.
#[tokio::test]
async fn a_start_inside_a_death_storm_is_not_recorded() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let sink = RecordingSink::default();

    let sup = ChannelSupervisor::spawn(
        "test",
        fast_backoff(),
        ReportingPolicy::default()
            .with_stable_uptime(Duration::ZERO)
            .with_flap_alarm(RespawnRateAlarm::new(Duration::from_secs(3600), 2)),
        Some(sink.sink()),
        scripted(vec![dying(&stopped), dying(&stopped), dying(&stopped), healthy(&stopped)]),
    );

    for _ in 0..500 {
        if stopped.load(Ordering::SeqCst) >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    sup.shutdown().await;

    let starts = sink
        .events()
        .into_iter()
        .filter(|e| matches!(e, BootAudit::Started { .. }))
        .count();
    assert_eq!(
        starts, 2,
        "the two starts before the alarm latched are durable; the ones inside the storm \
         are not: {:?}",
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
```

Add to `tests/reporting.rs`'s header:

```rust
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::channel::respawn_alarm::RespawnRateAlarm;

use super::*;
```

`twitchy_flap_alarm` is defined for symmetry with the other helper functions; if the final tests do not use it, **delete it** rather than leaving dead code (clippy `-D warnings` will insist).

- [ ] **Step 6: Run the loop-level tests to verify they fail**

```sh
cargo test -p kastellan-core --lib channel::boot_supervisor::tests::reporting
```

Expected: all four new tests FAIL — the loop still emits every row unconditionally, so the death count is 3 (not 2), the start count is 3+ (not 2), and the failed count is 3 (not 1). The fatal test will PASS at this point; that is expected and fine — it is a regression guard, not a driver.

- [ ] **Step 7: Wire the gate into the loop**

In `core/src/channel/boot_supervisor.rs`:

**`Started` arm** — gate the row, leave the `info!` alone:

```rust
                let attempts = failures + 1;
                // The per-cycle log line is NOT gated: the daemon log is the
                // per-event record, and it is what an operator reads while a
                // channel is misbehaving. Only the durable row is rate-limited.
                info!(channel = %label, attempts, "channel bus running");
                if policy.should_record_start() {
                    emit(&audit, BootAudit::Started { attempts }).await;
                }
```

**Death arm** — replace the `escalate_if_due` call and gate the `Died` row:

```rust
                let stable = policy.ran_long_enough(ran);
                let delay = if stable {
                    failures = 0;
                    backoff.next_delay(failures)
                } else {
                    let delay = backoff.next_delay(failures);
                    failures += 1;
                    delay
                };
                let ran_ms = ran.as_millis() as u64;
                let retry_in_ms = delay.as_millis() as u64;
                warn!(
                    channel = %label,
                    ran_ms,
                    retry_in_ms,
                    "channel stopped working after running; restarting it"
                );
                let verdict = policy.note_death(stable, Instant::now());
                if verdict.record {
                    emit(&audit, BootAudit::Died { ran_ms, retry_in_ms }).await;
                }
                report(&label, &verdict, failures);
```

(Keep the existing long comment block above the `stable` branch — it explains why the two arms differ, and it is still correct. Drop the now-unused `Outage` local and the `(delay, outage)` tuple in favour of the shape above.)

**`Retry` arm** — same shape:

```rust
                let verdict = policy.note_failed_attempt(Instant::now());
                if verdict.record {
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
                }
                report(&label, &verdict, failures);
```

**Replace `escalate_if_due` with `report`:**

```rust
/// Emit whichever loud lines this event earned.
///
/// Two independent alarms with two independent claims, kept separate on
/// purpose: "nothing has been received for `down_secs`" and "it has restarted
/// `deaths` times in the last hour" are different facts with different
/// remedies, and folding a flapping channel's up-time into the downtime clock
/// is the defect #521's review round removed.
fn report(label: &str, verdict: &Verdict, attempts: u32) {
    if let Some(down) = verdict.still_down {
        error!(
            channel = %label,
            down_secs = down.as_secs(),
            attempts,
            "CHANNEL STILL DOWN — nothing sent to this channel has been received for this long, \
             and it is still not staying up. The daemon is otherwise healthy; the cause is on \
             the preceding attempts' `error` field."
        );
    }
    if let Some(deaths) = verdict.flapping {
        error!(
            channel = %label,
            deaths,
            window_secs = reporting::FLAP_ALARM_WINDOW.as_secs(),
            "{CHANNEL_FLAPPING_LOG_PHRASE} — this channel keeps coming up and dying again. \
             Each cycle costs a sandboxed worker, its egress sidecar and a full login, and \
             a channel that restarts this often is not usefully up. The per-death cause is \
             in the preceding `channel stopped working after running` lines."
        );
    }
}
```

**Delete `ReportingPolicy::note_outage`.** Task 4 added it as the seam
`escalate_if_due` used; `report` no longer calls it, so leaving it behind is dead
code and `clippy -D warnings` will say so. The free `note_outage` function stays —
it is what `tests/reporting.rs` drives and what the two `note_*` methods call.

Extend the re-export: `pub use reporting::{Outage, ReportingPolicy, Verdict, CHANNEL_FLAPPING_LOG_PHRASE};`

Extend the module doc with a "## What gets said, and what gets stored (#518, #522)" section explaining the gate in two or three sentences and naming both issues.

- [ ] **Step 8: Run all the tests to verify they pass**

```sh
cargo test -p kastellan-core --lib channel::boot_supervisor
cargo test -p kastellan-core --lib channel::respawn_alarm
cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: everything PASSES, clippy exit 0. If `every_failed_attempt_is_audited_with_its_attempt_number` in `tests/bringup.rs` fails, that is a real signal — check whether the default 300 s threshold really keeps it ungated (it should; the test runs in milliseconds).

- [ ] **Step 9: Rename the now-overstated test in `tests/bringup.rs`**

`every_failed_attempt_is_audited_with_its_attempt_number` is no longer unconditionally true — this is exactly the name-drift class #516 found. Rename it to `failed_attempts_are_audited_with_their_attempt_numbers` and replace its doc comment with:

```rust
/// The durable record while the outage is still young: one row per failed
/// attempt, numbered, carrying the delay before the next one, then one row on
/// success.
///
/// "While young" is the whole caveat: once the outage escalates, identical
/// attempt rows stop being written (#518) — see
/// `tests::reporting::failed_attempts_stop_being_recorded_once_the_outage_is_reported`.
/// This test stays valid because it runs in milliseconds against the default
/// 300 s escalation threshold, so nothing here is ever gated.
```

- [ ] **Step 10: Mutation-check the two claims that matter**

Do these by hand, confirm the stated failure, then revert. Do **not** commit either mutation.

1. Move the flap alarm inside the retry loop (construct a fresh `RespawnRateAlarm` at the top of each iteration instead of holding it in `ReportingPolicy`). Run `cargo test -p kastellan-core --lib channel::boot_supervisor`. **Expected: `the_flap_alarm_accumulates_across_restarts` FAILS (3 deaths recorded, not 2) and nothing else does.** Revert.
2. In `ReportingPolicy::note_death`, read `self.deaths.in_storm()` *before* `self.deaths.record(now)` instead of after. Add a temporary test asserting the first death of a storm that has cleared is still recorded; confirm it fails with the mutation and passes without it. Revert the mutation, and **keep** the test:

```rust
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
```

- [ ] **Step 11: Verify the line budget**

```sh
wc -l core/src/channel/boot_supervisor.rs core/src/channel/boot_supervisor/reporting.rs core/src/channel/boot_supervisor/tests/*.rs
```

Expected: every file under 500. If `reporting.rs` is over, move its `mod tests` into `tests/` — do not shrink the doc comments.

- [ ] **Step 12: Commit**

```bash
git add core/src/channel/boot_supervisor.rs core/src/channel/boot_supervisor/reporting.rs core/src/channel/boot_supervisor/tests/
git commit -m "fix(channel): gate the recurring audit rows, and alarm on a flapping channel

Closes #518, closes #522.

One predicate gates the durable row for every recurring event: record it
unless the alarm that owns its regime is already latched on this episode and
did not speak for this event. DowntimeEscalator owns channel.boot_failed; a
new supervisor-owned RespawnRateAlarm owns channel.died and channel.started.

The alarm lives outside the retry loop, which is the #522 fix: PersistentWorker
builds its alarm inside the object a restart replaces, so a channel restart
discarded the window and it could never accumulate. Deaths in the 60s-300s
uptime band are 'stable' every time, so the escalator resets on each one and
never fires — the whole band was silent.

A 24h outage now leaves ~57 boot_failed rows instead of ~1440; a 24h flap
leaves ~58 rows instead of ~2800. Transient failures are recorded exactly as
before: the gate only engages once an alarm has already spoken.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Correct the operator help text

**Files:**
- Modify: `core/src/install/plan.rs:230-244`

**Interfaces:**
- Consumes: nothing new. The existing test that iterates `channel::actions` (around `plan.rs:916`) needs no change — this task introduces no new action name.

- [ ] **Step 1: Read the current help text and its test**

```sh
sed -n '215,250p' core/src/install/plan.rs
sed -n '900,930p' core/src/install/plan.rs
```

The claim to fix is at line 237: *"Every attempt is durable in audit_log as `@@BOOT_FAILED@@`, and success as `@@BOOT_STARTED@@`"*. That becomes false with Task 5.

- [ ] **Step 2: Replace the paragraph**

Replace the text from "Every attempt is durable" through the end of the SQL example with:

```
# The first attempts of an outage are durable in audit_log as @@BOOT_FAILED@@,
# and success as @@BOOT_STARTED@@:
#   SELECT ts, action, payload FROM audit_log
#    WHERE action IN ('@@BOOT_STARTED@@','@@BOOT_FAILED@@','@@BOOT_DIED@@')
#      AND payload->>'channel' = 'email' ORDER BY ts DESC LIMIT 20;
# The rows are RATE-LIMITED once a channel has been failing (or restarting) for
# a while: writing one identical row a minute for a day-long outage answers
# nothing the first one did not. You get the attempts up to the point the
# problem is first reported loudly, then one row per escalation (every 30 min).
# So a gap between rows means "still broken, still saying the same thing", NOT
# "recovered" — a recovery is a @@BOOT_STARTED@@ row. The daemon log
# (~/.local/state/kastellan/*.out) is the per-event record.
# CAVEAT: a channel most often dies because Postgres went away — and the row
# above needs that same Postgres, so exactly that outage writes no rows until
# it is over. The daemon log is the record for it.
```

- [ ] **Step 3: Run the tests**

```sh
cargo test -p kastellan-core --lib install::
cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: PASS with the same test count as before (this is prose inside an existing template; the `@@…@@` substitutions and their test are unchanged). Clippy exit 0.

- [ ] **Step 4: Commit**

```bash
git add core/src/install/plan.rs
git commit -m "docs(install): the channel audit rows are rate-limited, so stop claiming every attempt is durable

The help told operators every bring-up attempt lands in audit_log, which #518
makes false. It now says the rows are gated and that a gap between them means
'still broken', not 'recovered' — the reading an operator would otherwise get
wrong.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: Two-host gate

**Files:** none — this task runs the suites and records the numbers.

- [ ] **Step 1: Count the tests this branch adds**

```sh
git diff main --stat
git diff main | grep -c '^+.*#\[\(tokio::\)\?test'
```

Write the number down. It is the prediction for Step 3.

- [ ] **Step 2: Mac targeted run**

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR="$HOME/.cache/kastellan-518-target"
cargo test -p kastellan-core --lib 2>&1 | tee "$HOME/518-mac-lib.log" | tail -5
cargo test -p kastellan-core --bins 2>&1 | tee "$HOME/518-mac-bins.log" | tail -5
cargo clippy -p kastellan-core --all-targets -- -D warnings; echo "CLIPPY_EXIT=$?"
```

Expected: `--lib` at 1496 + (the Step 1 count), `--bins` at 87, clippy exit 0. The log goes to `$HOME`, never `/tmp` — `/tmp` is scrubbed mid-run on both hosts and has eaten a finished gate's log before.

- [ ] **Step 3: DGX full-workspace run (authoritative)**

Push the branch first, then drive the DGX as exactly `ssh dgx '<cmd>'` (the allow rule is a prefix match — flags before the hostname get denied):

```sh
git push -u origin fix/518-522-channel-event-reporting-gates
ssh dgx 'cd ~/src/kastellan && git fetch --all && git checkout fix/518-522-channel-event-reporting-gates && git pull && source $HOME/.cargo/env && (cargo test --workspace -- --nocapture > $HOME/518-dgx-test.log 2>&1; echo "TEST_EXIT=$?" >> $HOME/518-dgx-test.log; echo DONE >> $HOME/518-dgx-test.log)'
ssh dgx 'source $HOME/.cargo/env && cd ~/src/kastellan && cargo clippy --workspace --all-targets -- -D warnings; echo "CLIPPY_EXIT=$?"'
ssh dgx 'grep -E "^test result|TEST_EXIT|DONE" $HOME/518-dgx-test.log | tail -20; grep -c "\[SKIP\]" $HOME/518-dgx-test.log'
```

Expected: `TEST_EXIT=0`, `CLIPPY_EXIT=0`, exactly **4** `[SKIP]` lines (all the `KASTELLAN_GLINER_RELEX_ENABLE` tier — read them, do not assume), and a passed count of **3028 + the Step 1 number, exactly**.

**A count that does not land on the prediction is a finding, not a rounding error.** This diff contains no `cfg(target_os)` code, so both hosts see the identical suite; a mismatch means a test was double-counted, dropped in the Task 3 split, or is conditionally compiled somewhere unexpected. Investigate before proceeding.

- [ ] **Step 4: Record the numbers**

Note the DGX commit hash, passed/failed/ignored counts, `[SKIP]` count, and both exit codes. They go into HANDOVER's test-baseline table in Task 8.

---

### Task 8: Handover, roadmap, and the PR

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

- [ ] **Step 1: Update HANDOVER.md**

Header first: `Last updated:` → today, `main` HEAD → `git log --oneline -1` after merge, `Active branch:`, `Last gate:` → the Task 7 numbers. Then add a new row at the top of the [Test baseline](#test-baseline-authoritative) table with the branch tip hash and counts, move #518/#522 out of **Next TODO** into **Recently merged** with enough detail to start cold (the gate rule, why "until the alarm speaks" beat a "first N" constant, the sampling-order trap, the two mutation checks, the count delta), and write a fresh Next TODO.

- [ ] **Step 2: Update ROADMAP.md**

Tick the #518/#522 items with the merge commit hash, in the same style as the #516/#517 entries around line 255 and 275.

- [ ] **Step 3: Prune**

Both files must stay under 500 lines where feasible. HANDOVER is at ~434 today; if adding this session pushes it past what a fresh session can absorb, follow the pruning convention at the bottom of the file (snapshot to `archive/` first, in its own commit).

- [ ] **Step 4: Commit and push**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover): channel event reporting gates (#518, #522)

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
git push
```

- [ ] **Step 5: Open the PR**

Write the body to a file first (it contains backticks and newlines that do not survive a shell argument cleanly):

```bash
cat > /tmp/pr-body.md <<'BODY'
Closes #518. Closes #522.

## What was wrong

**#518** — in `boot_supervisor::run` the loud line was escalation-gated and the
audit row was not, so a sustained outage wrote ~1440 identical
`channel.boot_failed` rows per day per channel, every one carrying the same
`cause`.

**#522** — `STABLE_UPTIME` is 60 s and the escalation threshold is 300 s, so a
channel cycling "up 61 s → dead" was *stable* on every death: each one reset the
failure counter and called `record_success()`, clearing both the outage start and
the last-escalated stamp. The loud line therefore never fired at any point, while
each cycle cost a sandboxed worker, its egress sidecar and a full Matrix login —
~1400 restarts/day with nothing above a per-cycle `warn!`. Neither issue noticed
that the same band also writes a `channel.started` row per cycle, so the real
amplification was ~2800 rows/day.

## The fix

One predicate gates the durable row for every recurring event:

```rust
fn should_record(alarm_latched: bool, alarm_spoke_now: bool) -> bool {
    !alarm_latched || alarm_spoke_now
}
```

`DowntimeEscalator` owns `channel.boot_failed`; a new supervisor-owned
`RespawnRateAlarm` owns `channel.died` and `channel.started`. A `fatal` row is
never gated — it is terminal, it is one row, and it is the row that says why.

This is deliberately **not** #518's own sketch (`failures <= FIRST_N || escalated`):
"until the alarm speaks" needs no new constant, and it makes the row and the loud
line the *same* decision rather than two decisions that agree today. That is the
drift class #516 and #521 each found one instance of, in this feature's own docs.

The flap alarm lives **outside** the retry loop, and that placement is the whole
of #522: `PersistentWorker` builds its alarm inside the object a restart replaces,
so a channel restart discarded the window and it could never accumulate.

| scenario | before | after |
| --- | --- | --- |
| 24 h bring-up outage | ~1440 rows | ~57 |
| 24 h flap at 61 s cycles | ~2800 rows | ~58 |
| transient blip (the common case) | fully recorded | **unchanged** |

## Sampling order is load-bearing

Both latch reads happen *after* the recording call. Reading `in_storm()` before
`record()` looks more natural and is wrong in the one case that matters: when a
storm clears, `record()` prunes the window and clears the latch, so a read taken
beforehand still shows the old storm and silently suppresses the first death of
the new one — the single row an operator most needs. Pinned by
`the_first_death_of_a_fresh_storm_is_recorded`.

## Mutation checks (run, not assumed)

- Moving the alarm construction inside the loop fails
  `the_flap_alarm_accumulates_across_restarts` (3 deaths recorded, not 2) and
  nothing else.
- Reading `in_storm()` before `record()` fails
  `the_first_death_of_a_fresh_storm_is_recorded` and nothing else.

## Also in this PR

- `boot_supervisor/tests.rs` (719 lines, over the 500-line cap and about to grow)
  split by concern into `tests/{mod,bringup,liveness,reporting}.rs`. Pure movement
  in its own commit; same test count before and after.
- `install::plan::render_email_help` told operators **"every attempt is durable in
  audit_log"**, which this PR makes false. It now says the rows are rate-limited
  and that a gap between them means "still broken, still saying the same thing",
  not "recovered" — the reading an operator would otherwise get wrong.

## Gate

Fill in from Task 7 before opening: DGX full-workspace passed/failed/ignored,
`TEST_EXIT`, `CLIPPY_EXIT`, `[SKIP]` count and the tier they belong to, plus the
Mac targeted numbers. State the predicted count and that it landed — this diff has
no `cfg(target_os)` code, so both hosts see the identical suite.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
BODY

gh pr create --base main \
  --title "fix(channel): gate the recurring audit rows, and alarm on a flapping channel (closes #518, closes #522)" \
  --body-file /tmp/pr-body.md
```

---

## Notes for the implementer

- **`boot_supervisor.rs`'s doc comments are unusually long by convention, not by accident.** They record *why* a decision was made and what broke last time. Match that density; do not trim them to fit a line budget — move code out instead.
- **The three pre-existing tests in `tests/reporting.rs`** (`a_death_that_recovers_leaves_no_outage_open` and friends) drive `note_outage` directly. They must keep passing untouched through Tasks 4 and 5 — they are the guard on the #521 review's fix, and this plan must not regress it.
- **If a test passes before you write the implementation, it is not a test yet.** #521's review found exactly that: a test that passed in RED and with the mutation applied. Check RED for the stated reason, every time.
