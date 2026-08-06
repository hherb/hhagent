# boot_failed Rate Gate (#523) + STILL DOWN Const Lift (#524) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Gate the flap band's ungated `channel.boot_failed` audit rows behind a silent rate alarm on the failed-attempt stream (#523), and lift the `CHANNEL STILL DOWN` phrase into a shared const (#524).

**Architecture:** `ReportingPolicy` (core/src/channel/boot_supervisor/reporting.rs) gains a second `RespawnRateAlarm` fed by `note_failed_attempt` itself, so its latch is read after a `record()` on the same alarm by construction. The alarm emits no log line; its voice keeps a row only while the `DowntimeEscalator` has not escalated (the deferral). #524 is a mechanical const lift mirroring `CHANNEL_DISABLED_LOG_PHRASE`.

**Tech Stack:** Rust workspace, tokio tests, no new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-06-boot-failed-rate-gate-design.md` — read it first; it holds the full rationale and the row arithmetic every doc edit must match.

## Global Constraints

- Branch: `fix/523-524-boot-failed-rate-gate` (already exists; the spec is committed on it). Work in the primary checkout `/Users/hherb/src/kastellan`.
- No new dependencies (AGPL-compat review not needed — none added).
- Clippy is enforced: the tree must stay `cargo clippy --workspace --all-targets -- -D warnings` clean.
- No `cfg(target_os)` code anywhere in this diff — both hosts must see the same suite.
- Run all cargo commands in the **foreground** — never as background jobs, never piped through `| tail`.
- On the Mac, always prefix cargo with `CARGO_TARGET_DIR=$HOME/.cache/kastellan-sdd-target` (the IDE's rust-analyzer holds `target/debug/.cargo-lock`). Never put a target dir or a run log under `/tmp` — it is scrubbed mid-run.
- `git add` **specific files only**, never `git add -A`.
- Commit messages: conventional (`fix(channel): …`, `test(channel): …`, `docs(channel): …`), each ending with the line `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- New tests go in `core/src/channel/boot_supervisor/tests/reporting.rs`, NOT in `reporting.rs`'s inline `mod tests` (keeps `reporting.rs` under the 500-line cap).

---

### Task 1: #524 — const-lift `CHANNEL STILL DOWN`

**Files:**
- Modify: `core/src/channel/boot_supervisor.rs` (new const after `CHANNEL_DISABLED_LOG_PHRASE` ~line 125; interpolate in `report()` ~line 423)
- Modify: `core/src/install/plan.rs` (help text ~lines 242 and 248; `.replace` chain ~line 327; test `email_help_block_names_the_env_var_and_traps` ~line 929)

**Interfaces:**
- Produces: `pub const CHANNEL_STILL_DOWN_LOG_PHRASE: &str = "CHANNEL STILL DOWN";` in `kastellan_core::channel::boot_supervisor` — Task 4's help-text rewrite uses the `@@CHANNEL_STILL_DOWN@@` placeholder this task wires up.

- [ ] **Step 1: Extend the help test (red first)**

In `core/src/install/plan.rs`, inside `email_help_block_names_the_env_var_and_traps`, directly after the existing `CHANNEL_FLAPPING_LOG_PHRASE` assertion (ends ~line 929), add:

```rust
        // Third instance of the same class (#524): the downtime escalator's
        // phrase, asserted through the const `report()` interpolates rather
        // than a literal typed a second time here — a literal is exactly what
        // stayed green while the help and the log line drifted apart, twice.
        assert!(
            help.contains(crate::channel::boot_supervisor::CHANNEL_STILL_DOWN_LOG_PHRASE),
            "must name the exact still-down log phrase to grep for: {help}"
        );
```

- [ ] **Step 2: Run to verify it fails**

Run (foreground):
```sh
source "$HOME/.cargo/env"
CARGO_TARGET_DIR=$HOME/.cache/kastellan-sdd-target cargo test -p kastellan-core --lib install::plan
```
Expected: **compile error** — `cannot find value CHANNEL_STILL_DOWN_LOG_PHRASE`.

- [ ] **Step 3: Implement the const + both interpolation sites**

(a) In `core/src/channel/boot_supervisor.rs`, directly after the `CHANNEL_DISABLED_LOG_PHRASE` const (after ~line 125), add:

```rust
/// The phrase the downtime escalator's `error!` line opens with, and
/// therefore the string an operator greps for.
///
/// A `const` for the same reason as [`CHANNEL_DISABLED_LOG_PHRASE`] above and
/// `CHANNEL_FLAPPING_LOG_PHRASE` in [`reporting`]: the operator help
/// (`crate::install::plan::render_email_help`) names this phrase, and an
/// operator-facing phrase written in two places drifts — #516 found the first
/// instance of this class, the #518/#522 review found the second, and #524
/// found this one still bare.
pub const CHANNEL_STILL_DOWN_LOG_PHRASE: &str = "CHANNEL STILL DOWN";
```

(b) In the same file, in `report()` (~line 423), change the `error!` opening from:

```rust
            "CHANNEL STILL DOWN — nothing sent to this channel has been received for this long, \
```
to:
```rust
            "{CHANNEL_STILL_DOWN_LOG_PHRASE} — nothing sent to this channel has been received for this long, \
```

(c) In `core/src/install/plan.rs` help text, replace the two bare occurrences:
- Line ~242: `# the outage is first reported as CHANNEL STILL DOWN (5 min of continuous` → `# the outage is first reported as @@CHANNEL_STILL_DOWN@@ (5 min of continuous`
- Line ~248: `# then dies repeatedly) may never produce CHANNEL STILL DOWN at all, yet will` → `# then dies repeatedly) may never produce @@CHANNEL_STILL_DOWN@@ at all, yet will`

(d) In the `.replace` chain at the end of `render_email_help` (after the `@@CHANNEL_FLAPPING@@` replace, ~line 327), add:

```rust
    // Third instance of the same class (#524): the escalator's phrase,
    // substituted for the same reason as the two phrase replaces above.
    .replace(
        "@@CHANNEL_STILL_DOWN@@",
        crate::channel::boot_supervisor::CHANNEL_STILL_DOWN_LOG_PHRASE,
    )
```

(`str::replace` substitutes every occurrence, so one call covers both placeholders.)

- [ ] **Step 4: Run to verify green**

Run (foreground):
```sh
CARGO_TARGET_DIR=$HOME/.cache/kastellan-sdd-target cargo test -p kastellan-core --lib install::plan
CARGO_TARGET_DIR=$HOME/.cache/kastellan-sdd-target cargo test -p kastellan-core --lib channel::boot_supervisor
```
Expected: all PASS (the standing `!help.contains("@@")` assertion proves the substitution happened).

- [ ] **Step 5: Commit**

```sh
git add core/src/channel/boot_supervisor.rs core/src/install/plan.rs
git commit -m "fix(install): interpolate CHANNEL STILL DOWN from a const (#524)

Third instance of #516's drift class; the phrase now has one definition,
interpolated into both the error! line and the operator help.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: #523 — the rate gate in `ReportingPolicy`

**Files:**
- Modify: `core/src/channel/boot_supervisor/reporting.rs` (new field + builder on `ReportingPolicy` ~lines 196–246; rewrite `note_failed_attempt` ~lines 262–275)
- Test: `core/src/channel/boot_supervisor/tests/reporting.rs` (append three tests at end of file)

**Interfaces:**
- Consumes: `RespawnRateAlarm::{new, with_repeat, record, in_storm}` (`core/src/channel/respawn_alarm.rs`, unchanged); `DowntimeEscalator::has_escalated`; `note_outage`/`Outage` (already imported in the test file).
- Produces: `ReportingPolicy::with_attempt_alarm(self, attempts: RespawnRateAlarm) -> Self` (test seam used by Task 3); `note_failed_attempt` keeps its exact signature `(&mut self, now: Instant) -> Verdict`.

- [ ] **Step 1: Write the three failing tests**

Append to `core/src/channel/boot_supervisor/tests/reporting.rs` (the file already has `use super::*;`, `use crate::channel::respawn_alarm::RespawnRateAlarm;`, and `Duration` in scope):

```rust
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
```

- [ ] **Step 2: Run to verify they fail**

Run (foreground):
```sh
CARGO_TARGET_DIR=$HOME/.cache/kastellan-sdd-target cargo test -p kastellan-core --lib channel::boot_supervisor
```
Expected: **compile error** — `no method named with_attempt_alarm found for struct ReportingPolicy`.

- [ ] **Step 3: Implement the gate**

All in `core/src/channel/boot_supervisor/reporting.rs`:

(a) Add a field to `ReportingPolicy` (after the `deaths` field, ~line 211):

```rust
    /// Rate gate for the failed-attempt stream (#523). The escalator's latch
    /// cannot gate this stream in the flap band — each stable death clears it
    /// (`Outage::Ends` → `record_success`) — so a restart whose first attempt
    /// fails transiently would write one ungated row per cycle, forever. Fed
    /// by [`note_failed_attempt`](Self::note_failed_attempt) itself, so its
    /// latch is always read after a `record()` on the SAME alarm: the
    /// read-after-record contract by construction, not by exception.
    /// Deliberately SILENT — no log line — because its firing rate cannot
    /// distinguish an outage from a flap (~59 attempts/hour in both), and
    /// each regime already has its loud line.
    attempts: RespawnRateAlarm,
```

(b) In `ReportingPolicy::new` (~line 223), add to the struct literal:

```rust
            attempts: RespawnRateAlarm::new(FLAP_ALARM_WINDOW, FLAP_ALARM_THRESHOLD)
                .with_repeat(FLAP_ALARM_REPEAT),
```

(c) Add the builder directly after `with_flap_alarm` (~line 246):

```rust
    /// Override the failed-attempt rate gate. Exists so a test can trip it in
    /// two attempts instead of five — the same reason as
    /// [`with_flap_alarm`](Self::with_flap_alarm).
    pub fn with_attempt_alarm(mut self, attempts: RespawnRateAlarm) -> Self {
        self.attempts = attempts;
        self
    }
```

(d) Replace `note_failed_attempt` (its doc comment AND body, currently ~lines 262–275) with:

```rust
    /// Fold a failed bring-up attempt into the bookkeeping.
    ///
    /// Two alarms feed the verdict. The escalator owns the loud line and,
    /// once it has escalated, the row schedule. The rate gate is the fallback
    /// for the regime the downtime clock cannot see (#523: each stable death
    /// resets the clock, so a flap whose restarts also fail transiently never
    /// escalates): it bounds the rows to the first [`FLAP_ALARM_THRESHOLD`]
    /// per episode plus one per [`FLAP_ALARM_REPEAT`], each carrying `cause`.
    /// Its voice counts only while the escalator has NOT escalated —
    /// otherwise a sustained outage would write two near-duplicate rows per
    /// repeat interval, one per alarm's independent clock.
    pub fn note_failed_attempt(&mut self, now: Instant) -> Verdict {
        let still_down = note_outage(&mut self.escalator, Outage::Continues, now);
        // Record first, read after — the same contract `note_death` honors:
        // `record` is what re-arms a cleared storm's latch, so a read taken
        // beforehand could reflect a storm that is already over and suppress
        // the first attempt of the fresh one.
        let rate_fired = self.attempts.record(now);
        let escalated = self.escalator.has_escalated();
        let latched = escalated || self.attempts.in_storm();
        let spoke = still_down.is_some() || (rate_fired.is_some() && !escalated);
        Verdict { record: should_record(latched, spoke), still_down, flapping: None }
    }
```

Note: this deliberately deletes the old "here the order is NOT actually load-bearing" comment — with the rate alarm in the method, the order IS load-bearing now, and the new comment says so.

- [ ] **Step 4: Run to verify green**

Run (foreground):
```sh
CARGO_TARGET_DIR=$HOME/.cache/kastellan-sdd-target cargo test -p kastellan-core --lib channel::boot_supervisor
```
Expected: all PASS, including the four pre-existing unit tests in `reporting.rs`'s inline `mod tests` and every test in `tests/{bringup,liveness,reporting}.rs` — the spec's "what deliberately does not change" section traces why each stays green.

- [ ] **Step 5: Mutation check A — read-before-record**

In `note_failed_attempt`, temporarily move the `let latched = …` line ABOVE `let rate_fired = self.attempts.record(now);`. Run the same test command. Expected: exactly `the_first_failed_attempt_after_the_attempt_storm_clears_is_recorded` fails, nothing else. Revert the mutation. Record the observed failure line for the PR body.

- [ ] **Step 6: Mutation check B — drop the deferral**

Temporarily change `(rate_fired.is_some() && !escalated)` to `rate_fired.is_some()`. Run the same test command. Expected: exactly `a_rate_alarm_repeat_defers_to_an_escalated_outage` fails, nothing else. Revert. Record for the PR body.

- [ ] **Step 7: Mutation check C — revert the gate to escalated-only**

Temporarily change the last two bindings to the pre-#523 gate:
```rust
        let latched = escalated;
        let spoke = still_down.is_some();
```
Run the same test command. Expected: `flap_band_boot_failed_rows_are_gated_once_the_rate_alarm_latches` fails (and `the_first_failed_attempt_after_the_attempt_storm_clears_is_recorded` may fail with it — both exercise the new arm); every pre-existing test still passes, which is the proof the old behavior was a strict subset. Revert. Record for the PR body.

- [ ] **Step 8: Commit**

```sh
git add core/src/channel/boot_supervisor/reporting.rs core/src/channel/boot_supervisor/tests/reporting.rs
git commit -m "fix(channel): rate-gate the flap band's boot_failed rows (#523)

A second RespawnRateAlarm on the failed-attempt stream, fed by
note_failed_attempt itself (read-after-record by construction), silent,
deferring to the escalator once an outage has escalated. Bounds the #523
regime from one ungated row per cycle to the first five plus one
cause-bearing row per 30 min.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: #523 — loop-level test through the real supervisor

**Files:**
- Test: `core/src/channel/boot_supervisor/tests/reporting.rs` (append one test)

**Interfaces:**
- Consumes: `ReportingPolicy::{default, with_stable_uptime, with_attempt_alarm}` (Task 2); test helpers `RecordingSink`, `fast_backoff()`, `scripted()`, `dying()`, `healthy()` from `tests/mod.rs` (in scope via `use super::*;`); `Arc`, `AtomicUsize` already imported at the top of this test file.

- [ ] **Step 1: Write the test**

Append to `core/src/channel/boot_supervisor/tests/reporting.rs`:

```rust
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
```

- [ ] **Step 2: Run to verify it passes (and actually exercises the gate)**

Run (foreground):
```sh
CARGO_TARGET_DIR=$HOME/.cache/kastellan-sdd-target cargo test -p kastellan-core --lib channel::boot_supervisor
```
Expected: PASS. This test is green-on-arrival (Task 2 implemented the gate); its value is pinning the **loop wiring**. Prove it bites: re-apply Task 2's mutation C for a moment and run only this test — expected `failed` = 3, assertion fails. Revert.

- [ ] **Step 3: Commit**

```sh
git add core/src/channel/boot_supervisor/tests/reporting.rs
git commit -m "test(channel): pin the boot_failed rate gate through the real retry loop (#523)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: #523 — the docs that currently document the defect

**Files:**
- Modify: `core/src/channel/boot_supervisor/reporting.rs` (module doc lines ~28–32 and ~45–64; `should_record` doc ~line 177)
- Modify: `core/src/channel/boot_supervisor/downtime.rs` (`has_escalated` doc ~lines 94–103)
- Modify: `core/src/channel/boot_supervisor.rs` (module doc bullet ~line 68)
- Modify: `core/src/install/plan.rs` (help text ~lines 241–254)

**Interfaces:**
- Consumes: the `@@CHANNEL_STILL_DOWN@@` placeholder wired in Task 1.
- Produces: nothing code-visible — prose only; every figure below is copied from the spec's "Row arithmetic" section.

- [ ] **Step 1: reporting.rs module doc**

(a) In the paragraph ending ~line 32, change `into ~57 \`boot_failed\` rows instead of ~1440` to `into ~53 \`boot_failed\` rows instead of ~1440`.

(b) Replace the entire "unstated assumption" paragraph (~lines 45–64, from `//! That ~1470 figure carries an unstated assumption` through `//! Tracked as a follow-up rather than changed here.`) with:

```rust
//! That ~1470 figure once carried an unstated assumption — that every restart
//! in the flap succeeds on its first try — and #523 was the regime where it
//! broke: every death in the #522 band is *stable*, so
//! [`note_death`](ReportingPolicy::note_death) takes [`Outage::Ends`], which
//! clears both halves of the escalator (`record_success()`), and a restart
//! whose first attempt failed transiently was then evaluated against a
//! freshly-cleared latch — one ungated `boot_failed` row per cycle, for as
//! long as the flap lasted. The attempt stream therefore has its own rate
//! gate: a second [`RespawnRateAlarm`] fed by
//! [`note_failed_attempt`](ReportingPolicy::note_failed_attempt) itself, so
//! its latch is always read after a `record()` on the same alarm. It is
//! deliberately SILENT — no log line — because its firing rate cannot
//! distinguish an outage from a flap (~59 attempts/hour in both), and each
//! regime already has its loud line. That weakens "the row and the loud line
//! are the same decision" to: every loud line still has its row, and
//! rate-gate rows are line-less cause samples. Its voice keeps a row only
//! while the escalator has not escalated, so a sustained bring-up outage
//! follows the escalator's row schedule (~53 rows in 24 h: the first five
//! attempts, then each escalation) rather than two near-duplicate rows per
//! repeat interval, and the flap-with-failing-restarts regime is bounded to
//! the first five attempts plus one cause-bearing row per
//! [`FLAP_ALARM_REPEAT`] (#523).
```

(c) In `should_record`'s doc (~line 177), change `unless its alarm is already latched on this episode and did not speak for this particular event` to `unless an alarm owning its regime is already latched on this episode and none spoke for this particular event` (the attempt stream now has a composite latch).

(d) In the inline `mod tests`, the doc comment of `failed_attempts_stop_being_recorded_once_the_outage_escalates` (~line 313) also carries the old figure — change `~57 rows in a day instead of ~1440 (#518)` to `~53 rows in a day instead of ~1440 (#518)`.

- [ ] **Step 2: downtime.rs `has_escalated` doc**

Replace the second paragraph of the doc comment (~lines 96–100, from `The supervisor's audit gate reads this` through `(#518) without inventing a "first N attempts" constant nobody could derive.`) with:

```rust
    /// The supervisor's audit gate reads this as one arm of the decision on
    /// whether a failed attempt still earns a durable row: until the outage
    /// has been escalated, attempts are gated only by the attempt-stream rate
    /// limiter (#523), and after it only the escalations are written. That is
    /// what keeps a 24-hour outage to ~53 rows instead of ~1440 (#518): the
    /// first five attempts, then each escalation — without inventing a
    /// "first N attempts" constant nobody could derive.
```

- [ ] **Step 3: boot_supervisor.rs module doc bullet**

In the third bullet of the "Staying up" section (ends `once enough deaths land inside its window.` ~line 68), append one sentence:

```rust
//!   The failed-attempt stream carries the same shape of gate since #523 — a
//!   silent rate alarm inside the policy — because the flap band resets the
//!   escalator on every stable death, leaving bring-up failures otherwise
//!   ungated exactly there.
```

- [ ] **Step 4: render_email_help rewrite**

In `core/src/install/plan.rs`, replace the block from `# @@BOOT_FAILED@@ is RATE-LIMITED by the downtime clock: every attempt until` (~line 241) through `# that attempt is recorded ungated, every cycle. @@BOOT_STARTED@@` (~line 254) with:

```text
# @@BOOT_FAILED@@ is RATE-LIMITED twice over: by the downtime clock (every
# attempt until the outage is first reported as @@CHANNEL_STILL_DOWN@@ — 5 min
# of continuous downtime — then only on each repeat of that line, every
# 30 min) and by an attempt-rate gate (5 failed attempts inside an hour,
# then one row per 30 min), whichever engages first. A 24-hour outage
# produces ~53 rows instead of ~1440: the first five attempts, then each
# escalation. @@BOOT_DIED@@
# is RATE-LIMITED by a separate flap alarm: every death until 5 deaths within
# an hour first reports @@CHANNEL_FLAPPING@@, then only on each repeat. These are
# independent: a channel that keeps recovering and dying (e.g., cycles up 61s
# then dies repeatedly) may never produce @@CHANNEL_STILL_DOWN@@ at all, yet will
# still stop writing @@BOOT_DIED@@ rows once the flap alarm fires — and in that
# same cycling regime a restart attempt that fails transiently is bounded by
# the attempt-rate gate (the first few rows carry the failure cause, then one
# row per 30 min keeps sampling it), where it used to write one ungated row
# per cycle. @@BOOT_STARTED@@
```

Keep everything after `@@BOOT_STARTED@@` (`# is NOT rate-limited: …` through the CAVEAT lines) exactly as it is.

- [ ] **Step 5: Check nothing else still claims the old figures**

Run:
```sh
grep -rn "57 rows\|NOT rate-limited at all\|recorded ungated" core/src/
```
Expected: no hits (the three sites above were the only ones; a leftover hit means a doc site was missed — fix it the same way).

- [ ] **Step 6: Run to verify green**

Run (foreground):
```sh
CARGO_TARGET_DIR=$HOME/.cache/kastellan-sdd-target cargo test -p kastellan-core --lib channel::boot_supervisor
CARGO_TARGET_DIR=$HOME/.cache/kastellan-sdd-target cargo test -p kastellan-core --lib install::plan
```
Expected: all PASS (`email_help_block_is_entirely_commented_out` proves every new line still starts with `#`; the `!help.contains("@@")` assertion proves both placeholders substitute).

- [ ] **Step 7: Commit**

```sh
git add core/src/channel/boot_supervisor/reporting.rs core/src/channel/boot_supervisor/downtime.rs core/src/channel/boot_supervisor.rs core/src/install/plan.rs
git commit -m "docs(channel): the boot_failed gate arithmetic, updated for the #523 rate gate

The defect was documented in three places (reporting.rs module doc,
has_escalated, render_email_help); all three now describe the gate that
closes it, with the new ~53-rows-per-day figure.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: Two-host verification gate

**Files:** none (verification only).

**Interfaces:**
- Consumes: everything committed in Tasks 1–4, pushed to `origin/fix/523-524-boot-failed-rate-gate`.

- [ ] **Step 1: Mac targeted gate**

Run (foreground, expect ~several minutes each):
```sh
source "$HOME/.cargo/env"
CARGO_TARGET_DIR=$HOME/.cache/kastellan-sdd-target cargo test -p kastellan-core --lib
CARGO_TARGET_DIR=$HOME/.cache/kastellan-sdd-target cargo clippy -p kastellan-core --all-targets -- -D warnings
```
Expected: `core --lib` **1518 passed** (baseline 1514 + 4), 0 failed, 1 ignored; clippy exit 0. If the count is anything other than 1518, STOP and reconcile before proceeding — a surprise delta means a test was added or lost somewhere unplanned.

- [ ] **Step 2: Push the branch**

```sh
git push -u origin fix/523-524-boot-failed-rate-gate
```

- [ ] **Step 3: DGX full-workspace gate**

The `Bash(ssh dgx *)` allow rule is a prefix match — the command must start exactly `ssh dgx '…'`, no flags before the hostname. Logs go to `$HOME` on the DGX, never `/tmp`. Run (foreground; the test run takes ~40 min, use a generous timeout):

```sh
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && git fetch origin && git checkout fix/523-524-boot-failed-rate-gate && git pull --ff-only && git log --oneline -1'
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && (cargo test --workspace -- --nocapture > ~/kastellan-523-gate.log 2>&1; echo TEST_EXIT=$? >> ~/kastellan-523-gate.log); tail -3 ~/kastellan-523-gate.log'
ssh dgx 'grep -E "^test result:" ~/kastellan-523-gate.log | awk "{p+=\$4; f+=\$6; i+=\$8} END {print p\" passed, \"f\" failed, \"i\" ignored\"}"; grep -c "\[SKIP\]" ~/kastellan-523-gate.log'
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && (cargo clippy --workspace --all-targets -- -D warnings > ~/kastellan-523-clippy.log 2>&1; echo CLIPPY_EXIT=$? >> ~/kastellan-523-clippy.log); tail -2 ~/kastellan-523-clippy.log'
```
Expected: **3047 passed, 0 failed, 53 ignored** (baseline 3043 + 4), `TEST_EXIT=0`, exactly **4** `[SKIP]` lines (all the `KASTELLAN_GLINER_RELEX_ENABLE` tier — verify with `grep "\[SKIP\]" ~/kastellan-523-gate.log`), `CLIPPY_EXIT=0`. Any other count: STOP and reconcile.

- [ ] **Step 4: Record the numbers**

Note the exact counts and the `[SKIP]` identities for the PR body and the HANDOVER update — they are the authoritative gate.

---

### Task 6: Session close — handover, PR

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md` (header; new "Current state" entry; remove #523/#524 from Next TODO; add a Test-baseline row)
- Modify: `docs/devel/ROADMAP.md` (tick the matching items with the merge hash once merged; before merge, reference the PR)

**Interfaces:**
- Consumes: gate numbers from Task 5; mutation-check observations from Tasks 2–3.

- [ ] **Step 1: Update HANDOVER.md**

Per the checklist at the bottom of HANDOVER.md: bump the header (`Last updated`, branch, `Last gate` with the Task 5 counts), write the #523/#524 entry into Current state (what shipped, the deferral rationale, the mutation checks and their observed failures, the new row arithmetic), remove both items from Next TODO, add the DGX + Mac rows to the Test baseline table. Prune anything the entry supersedes.

- [ ] **Step 2: Update ROADMAP.md**

Tick/annotate the channel-reporting follow-up line with this branch/PR.

- [ ] **Step 3: Commit and push the docs**

```sh
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover): #523/#524 branch gated on both hosts

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
git push
```

- [ ] **Step 4: Open the PR**

```sh
gh pr create --repo hherb/kastellan --base main \
  --title "fix(channel): rate-gate the flap band's boot_failed rows, and const-lift CHANNEL STILL DOWN (closes #523, closes #524)" \
  --body "$(cat <<'EOF'
## What

Two follow-ups filed from the #518/#522 final review, per the approved spec
(`docs/superpowers/specs/2026-08-06-boot-failed-rate-gate-design.md`):

- **#523:** the `boot_failed` audit gate never engaged in the flap band —
  each stable death clears the escalator, so a transiently-failing restart
  wrote one ungated row per cycle, unbounded. `ReportingPolicy` now carries a
  second `RespawnRateAlarm` fed by `note_failed_attempt` itself
  (read-after-record by construction), silent (no new operator phrase),
  deferring to the escalator once an outage has escalated. Sustained outage:
  ~57 → ~53 rows/day, same shape. Flap with failing restarts: unbounded →
  first 5 + one cause-bearing row per 30 min.
- **#524:** `CHANNEL STILL DOWN` was the last bare operator phrase in
  `render_email_help` (third instance of #516's class); now a shared const
  interpolated into both the `error!` line and the help, asserted through
  the const.

## Mutation checks (run by hand, each reverted)

- read-before-record → exactly `the_first_failed_attempt_after_the_attempt_storm_clears_is_recorded` fails
- deferral dropped → exactly `a_rate_alarm_repeat_defers_to_an_escalated_outage` fails
- gate reverted to escalated-only → `flap_band_boot_failed_rows_are_gated_once_the_rate_alarm_latches` fails; loop-level test records 3 rows instead of 2

## Gates

- DGX full workspace: <fill from Task 5>, clippy `-D warnings` exit 0
- Mac `core --lib`: <fill from Task 5>, clippy exit 0
- No `cfg(target_os)` code in the diff; both hosts see the same suite.

Nothing here changes runtime channel behaviour — only what is logged and
stored — so it rides the next deploy (same reasoning as #525).

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Fill the two `<fill from Task 5>` slots with the real numbers before submitting.
