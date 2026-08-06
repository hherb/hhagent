# Rate-gate the flap band's `boot_failed` rows (#523) + const-lift `CHANNEL STILL DOWN` (#524)

**Date:** 2026-08-06 · **Branch:** `fix/523-524-boot-failed-rate-gate` · **Closes:** [#523](https://github.com/hherb/kastellan/issues/523), [#524](https://github.com/hherb/kastellan/issues/524)

Both defects were filed from the #518/#522 branch's final review, deliberately
unfixed there: #523's obvious patch has two traps in it, and #524 was out of
that branch's scope. One PR closes both, the same shape as the
`fix/518-522-channel-event-reporting-gates` branch.

## Part 1 — #523: the `boot_failed` gate never engages in the flap band

### The defect

`ReportingPolicy::note_failed_attempt`
(`core/src/channel/boot_supervisor/reporting.rs`) gates the durable
`channel.boot_failed` row on `DowntimeEscalator::has_escalated()`. In the #522
flap band every death is *stable* (uptime ≥ `STABLE_UPTIME`), so `note_death`
takes `Outage::Ends` → `record_success()`, which clears **both** the downtime
clock and the escalated latch. A restart whose first bring-up attempt then
fails transiently is evaluated against a freshly-cleared escalator: downtime
0 s, nothing latched, `should_record(false, false) = true`. One ungated
`boot_failed` row per flap cycle, indefinitely — roughly #518's original
figure, in #522's regime. `CHANNEL STILL DOWN` correctly never fires there
(that is #522 working as intended), so nothing ever latches and nothing ever
gates.

### The two traps any fix must avoid (from the issue)

1. **No bare latch read.** A latch may only be read *after* a `record()` on
   the same alarm — `record` is what re-arms an alarm when its storm clears,
   so a read with no preceding record can reflect a storm that is already
   over. `the_first_death_of_a_fresh_storm_is_recorded` pins this for the
   death stream; OR-ing `deaths.in_storm()` into the attempt gate would
   violate it (no death is recorded on the attempt path).
2. **`cause` must keep reaching the table.** It is the only forensic field in
   the whole row set (`channel.died` carries `ran_ms`/`retry_in_ms`,
   `channel.started` carries `attempts`); a gate that silences the attempt
   stream outright for the length of a flap loses the one place the actual
   error text lands.

### Chosen design: a silent rate gate on the attempt stream, deferring to the escalator

Approaches considered and rejected: a non-mutating `is_storming(now)` read of
the death alarm (smallest change, but once the flap latches the attempt
stream writes **zero** rows — trap 2, named disqualifying by the issue), and
the same rate gate without the deferral below (simpler predicate, but a
sustained outage writes ~2 near-duplicate rows per 30 min because both
alarms repeat on independent clocks anchored ~4.5 min apart).

`ReportingPolicy` gains a second `RespawnRateAlarm` field, `attempts`, fed by
`note_failed_attempt` itself — so the latch read is preceded by a `record()`
on the *same* alarm, satisfying trap 1 **by construction** rather than by
exception. It is built from the same three constants the flap alarm uses —
`FLAP_ALARM_WINDOW` (1 h), `FLAP_ALARM_THRESHOLD` (5), `FLAP_ALARM_REPEAT`
(30 min) — one rate policy for both recurring streams; diverge only when a
reason exists.

```rust
pub fn note_failed_attempt(&mut self, now: Instant) -> Verdict {
    let still_down = note_outage(&mut self.escalator, Outage::Continues, now);
    // Record first, read after — the same contract note_death honors.
    let rate_fired = self.attempts.record(now);
    let escalated = self.escalator.has_escalated();
    let latched = escalated || self.attempts.in_storm();
    // The rate alarm is the fallback voice for the regime the downtime
    // clock cannot see (#523): its firings keep rows only while the
    // escalator has not already claimed this stream.
    let spoke = still_down.is_some() || (rate_fired.is_some() && !escalated);
    Verdict { record: should_record(latched, spoke), still_down, flapping: None }
}
```

Load-bearing details, each deliberate:

- **No new log line, no new operator phrase.** The alarm's firing rate cannot
  distinguish an outage from a flap (~59 attempts/hour in both: capped
  backoff emits one attempt per ~60 s, a ~61 s flap cycle emits one failed
  attempt per cycle), so any line it emitted would be noise in one regime or
  the other — and each regime already has its loud line (per-attempt `warn!`
  + `CHANNEL STILL DOWN`; per-death `warn!` + `CHANNEL FLAPPING`). This
  deliberately weakens #518's "the row and the loud line are the same
  decision" to: **every loud line still has its row; rate-gate rows are
  line-less cause samples.** The module doc must own that asymmetry. A
  side-benefit: no new phrase means no new #524-class drift surface.
- **The deferral (`&& !escalated`).** The rate alarm's voice keeps a row only
  while the escalator has not escalated this outage. Without it, a sustained
  outage writes two near-duplicate rows per 30 min (escalator repeat +
  rate-alarm repeat). With it, whichever alarm owns the regime is the only
  one driving rows: the escalator in an outage, the rate gate in the flap
  band. `escalated` is read *after* `note_outage` has run, so an attempt that
  itself escalates discards the rate voice but keeps its row via
  `still_down`.
- **The `attempts` window is never cleared on recovery** — pure sliding
  window, pruned only by `record`, exactly like the death alarm. Two short
  outages inside one window can therefore gate each other's later attempts;
  that is intended rate-limiter semantics (≥5 cause samples already landed
  within the hour), and the fresh-storm test below pins the recovery path
  (window empties → next attempt recorded).
- **Test seam:** `with_attempt_alarm(RespawnRateAlarm)` builder, mirroring
  `with_flap_alarm`. No new latch accessor unless a test turns out to need
  one; the planned tests all assert through `Verdict::record`.

### What deliberately does not change

`note_death`, the death/flap alarm, `Verdict`'s shape, `report()`, the
`Started` arm's ungated emit, `Outage::Ends` semantics, and the `Fatal` arm
(which bypasses the policy entirely). All four existing pinned behaviors were
traced against the new predicate and pass unchanged:

- `failed_attempts_stop_being_recorded_once_the_outage_escalates` (unit):
  events at t = 0, 100, 301, 400, 2101 s against the production attempt alarm
  (threshold 5) — counts 1–4 never latch the rate gate, the escalator latch
  drives every assertion exactly as today, and the 5th event's rate-fire
  coincides with an escalator repeat that already keeps the row.
- `failed_attempts_stop_being_recorded_once_the_outage_is_reported`
  (loop-level, zero-threshold escalator): attempt 1 escalates (row kept),
  attempts 2–3 are gated by the escalated latch — rate count ≤ 3 never
  interferes.
- `a_fatal_failure_is_always_recorded`: `Fatal` never consults the policy.
- `the_first_death_of_a_fresh_storm_is_recorded`: the death path is untouched.

### Row arithmetic (docs must be updated to these figures)

- **Sustained 24 h bring-up outage:** ~57 → **~53**. Attempts 1–5 (t ≤ 15 s)
  recorded in full; attempts 6–10 (t = 31–243 s, between the rate latch and
  the 5-min escalation) are the only rows lost — near-duplicates of a cause
  already sampled five times in the first fifteen seconds; then the
  escalation and its ~47 repeats, each with `cause`, exactly as today.
- **Flap whose restarts also fail transiently (the #523 regime):**
  unbounded → **first 5 + ~48/day**, every row carrying `cause`, while
  `CHANNEL FLAPPING` stays loud on its own 30-min repeat from the death
  stream.
- **Plain flap (restarts succeed first try):** zero `boot_failed` rows, as
  today.

### Tests (TDD — each written red first)

New tests live in `core/src/channel/boot_supervisor/tests/reporting.rs`
(keeps `reporting.rs` under the 500-line cap; the split-tests layout is the
#518/#522 branch's pattern):

1. `flap_band_boot_failed_rows_are_gated_once_the_rate_alarm_latches` —
   policy-level, scripted `Instant`s, small threshold: alternating stable
   death + failed attempt cycles; first N attempts recorded, later ones
   gated, a repeat brings one `cause`-bearing row back, and `still_down`
   never fires throughout.
2. `the_first_failed_attempt_after_the_attempt_storm_clears_is_recorded` —
   the fresh-storm mirror for the attempt stream: latch the rate gate, let
   the window empty, next failed attempt must be recorded.
   **Mutation check:** moving the `in_storm` read above `record` fails
   exactly this test.
3. `a_rate_alarm_repeat_defers_to_an_escalated_outage` — escalated outage,
   rate-alarm repeat elapses, the rate voice alone must NOT keep a row (rows
   follow the escalator's schedule). **Mutation check:** dropping
   `!escalated` fails exactly this test.
4. `a_flapping_channel_with_failing_restarts_stops_writing_boot_failed_rows`
   — loop-level, scripted
   `[dying, Retry, dying, Retry, dying, Retry, healthy]` with attempt
   threshold 2: exactly 2 `BootAudit::Failed` rows (the second fires the
   gate; the third is latched-and-silent).

Expected count delta: **+4**, no `cfg(target_os)` code anywhere in the diff,
so both hosts see the same suite and the DGX count must land exactly on the
prediction.

### Documentation that is part of the fix (the defect is *documented* today)

- `reporting.rs` module doc, the "unstated assumption" paragraph (currently
  lines 45–64): rewritten — the assumption is now enforced by the rate gate;
  state the new arithmetic and the deliberate loud-line asymmetry.
- `downtime.rs::has_escalated` doc: the "~57 rows" claim → ~53, and note the
  gate now has a second arm.
- `install::plan::render_email_help`: the "NOT rate-limited at all" caveat
  block (currently lines 250–254) → `boot_failed` is rate-limited in both
  regimes; keep the Postgres-outage caveat that follows it.
- `boot_supervisor.rs` module doc's reporting-gate bullet: mention the
  attempt stream's rate gate alongside the death stream's.

## Part 2 — #524: `CHANNEL STILL DOWN` is a bare literal, twice

Third instance of #516's class; now the only non-const operator phrase in the
help block. Exactly as the issue specifies:

1. `pub const CHANNEL_STILL_DOWN_LOG_PHRASE: &str = "CHANNEL STILL DOWN";`
   in `boot_supervisor.rs`, beside `CHANNEL_DISABLED_LOG_PHRASE`, with the
   same doc rationale (an operator-facing phrase written in two places
   drifts, and the test pinning the literal stays green through it).
2. `report()`'s `error!` interpolates it:
   `"{CHANNEL_STILL_DOWN_LOG_PHRASE} — nothing sent to this channel …"`.
3. `render_email_help`: both bare occurrences become
   `@@CHANNEL_STILL_DOWN@@`; one `.replace(...)` in the existing chain
   substitutes both (`str::replace` replaces every occurrence).
4. The help test asserts `help.contains(CHANNEL_STILL_DOWN_LOG_PHRASE)`
   through the const, alongside the existing `DISABLED`/`FLAPPING`
   assertions; the standing `!help.contains("@@")` assertion catches a
   missed substitution.

Only *emitted* lines and operator-facing help interpolate the const. Internal
prose references (doc comments and code comments that merely discuss the
phrase) stay prose — doc comments cannot interpolate, and they are not grep
targets.

## Verification

- Mac (targeted, private `CARGO_TARGET_DIR` under `$HOME`):
  `cargo test -p kastellan-core --lib` (expect baseline 1514 + 4 = **1518**)
  and `cargo clippy -p kastellan-core --all-targets -- -D warnings`.
- DGX (authoritative): full `cargo test --workspace -- --nocapture`
  (expect baseline 3043 + 4 = **3047**, `[SKIP]` exactly 4, all GLiNER) and
  `cargo clippy --workspace --all-targets -- -D warnings`.
- Mutation checks run by hand and recorded in the PR: the two named above,
  plus reverting the gate to escalated-only must fail test 1.
- **No live DGX gate needed:** like #525, nothing changes runtime channel
  behavior — only what is logged and stored — so it rides the next deploy.

## Out of scope

- Any change to the death stream, `channel.started` gating, or
  `Outage::Ends` (settled in #525, relitigated nowhere).
- Per-service keying, #515's sink timeout, #497's bus unification.
- New operator log lines or audit actions.
