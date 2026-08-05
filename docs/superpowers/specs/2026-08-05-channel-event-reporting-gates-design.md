# Channel event reporting gates — design

**Date:** 2026-08-05
**Closes:** [#518](https://github.com/hherb/kastellan/issues/518), [#522](https://github.com/hherb/kastellan/issues/522)
**Builds on:** [#516](https://github.com/hherb/kastellan/pull/516) (supervised bring-up), [#521](https://github.com/hherb/kastellan/pull/521) (supervised liveness)

## The problem

`ChannelSupervisor` retries a channel forever, and since #517 it also restarts one
that dies after coming up. Both loops are unbounded by design — a homeserver can be
down for an hour and the daemon should recover without a human. What is *not*
bounded, and should be, is what each iteration says.

Two independent symptoms, one shared cause.

### #518 — the durable row is not gated the way the log line is

In `boot_supervisor::run`'s `Retry` arm the loud line is escalation-gated and the
audit row is not:

```rust
emit(&audit, BootAudit::Failed { .. }).await;                 // EVERY attempt
if let Some(down) = escalator.record_failure(Instant::now()) { // gated
    error!(… "CHANNEL STILL DOWN — …");
}
```

`DowntimeEscalator` exists precisely because one identical line per minute is noise
rather than signal. Once the backoff caps at 60 s, a sustained outage writes
**~1440 `channel.boot_failed` rows per day per channel**, every one carrying the
same `cause` and saying what the first one said.

It bites hardest on a condition classified `Retry` that is in fact permanent — a
missing worker binary being the clearest example. The classification stays right
(a binary genuinely could appear on a reinstall, and hard-failing would reintroduce
#514's posture), but the write amplification is unbounded and carries no new
information after the first few rows.

### #522 — a channel dying just past `STABLE_UPTIME` never escalates at all

Two thresholds sit either side of a regime nothing covers:

- `DowntimeEscalator::STABLE_UPTIME` = **60 s** — how long a channel must run for
  its death to count as "it had been working".
- `DowntimeEscalator::DEFAULT_THRESHOLD` = **300 s** — how long an outage must last
  before `CHANNEL STILL DOWN` fires.

A channel cycling "up 61 s → dead" is *stable* on every death, so each one resets
`failures` to 0 (next restart uses the base 1 s delay, never backing off) and calls
`record_success()`, clearing both `first_failure` and `last_escalated`. The loud
line therefore never fires, at any point, however long this runs. The whole
60 s–300 s uptime band is a channel that is visibly broken and silently restarting.

The cost per cycle is a sandboxed worker, its 1:1 egress sidecar under
force-routing, and a Matrix login plus initial sync — roughly **1400 restarts/day**
with nothing louder than a per-cycle `warn!`.

`RespawnRateAlarm` is the right shape and already exists, but the instance that
exists is the wrong one for this. `PersistentWorker` builds its alarm at
`worker_lifecycle/persistent.rs:167` — on the driver thread, *before* the job loop,
where it correctly accumulates across **worker respawns** for that thread's life
(`:220`'s `std::mem::replace` swaps the *transport*, not the alarm). What it cannot
see is a **channel** restart, which tears down the whole `PersistentWorker`, driver
thread and alarm together. So the channel's death-rate alarm has to be owned one
level up, above the thing a restart replaces.

> **Correction, 2026-08-05.** An earlier draft of this spec — and #522's own text —
> said `PersistentWorker` "builds its alarm inside the object a restart replaces, so
> the window is discarded every cycle and can never accumulate." That is false as
> stated, and a reviewer caught it by reading `persistent.rs`. The *conclusion* was
> always right; only the attribution was wrong. `PersistentWorker` does not make a
> mistake here.

### One correction to both issues

Neither issue notices that the flap band also writes a **`channel.started`** row per
cycle. The real amplification there is ~2800 rows/day, not ~1400.

## The shape of the fix

Both issues ask the same question about two different event streams: *when does a
recurring channel event deserve an operator's attention?* One policy object already
answers it for the "still down" regime; nothing answers it for "keeps dying".

So: add the missing alarm, and make **one predicate** gate both the loud line and
the durable row for every recurring stream.

### One predicate, three streams

```rust
/// A recurring event earns a durable row unless its alarm is already latched
/// on this episode and did not speak for this particular event.
fn should_record(alarm_latched: bool, alarm_spoke_now: bool) -> bool {
    !alarm_latched || alarm_spoke_now
}
```

| stream | `alarm_latched` | `alarm_spoke_now` |
| --- | --- | --- |
| `channel.boot_failed` | `escalator.has_escalated()` | `record_failure(now).is_some()` |
| `channel.died` | `deaths.in_storm()` | `deaths.record(now).is_some()` |
| `channel.started` | — **never gated**, see below | — |

**Both inputs are sampled *after* the recording call, and that is load-bearing.**
Sampling `in_storm()` *before* `record()` reads more naturally and is wrong in one
case that matters: when a storm clears, `record()` prunes the window, finds the count
below threshold and re-arms — so the first death of the *next* storm has
`in_storm() == false` afterwards (correctly recorded) but `true` beforehand (silently
suppressed, and the first evidence of a fresh storm is exactly the row you want).
The same argument applies to `has_escalated()` across a `record_success()`. A test
pins it.

`channel.boot_failed` with `fatal: true` is **never** gated: it is terminal, it is
one row, and it is the row that says why the channel will not be retried.

Two properties this has that #518's own sketch
(`failures <= FIRST_ATTEMPTS_ALWAYS_AUDITED || escalated`) does not:

1. **No new knob.** "The first N events" becomes "until the alarm speaks", where N
   is already determined by the escalation threshold and the backoff schedule.
   A constant nobody can derive from anything is a constant that drifts.
2. **The row and the line become the same decision, structurally.** Splitting an
   operator-facing policy across two call sites is how the two diverge — the class
   of defect #516 found in `render_email_help` and #521 found again in the same
   file's documented `audit_log` query.

The death that *trips* the alarm is itself recorded, not the first one suppressed:
`record()` returns `Some(count)` on that call, so `alarm_spoke_now` carries it.

### Why `channel.started` is not gated

An earlier version of this design gated it on the death alarm, reasoning that inside
a storm the `started` row is redundant because the paired `channel.died` row carries
`ran_ms`. **That was wrong, and it was caught during implementation.** The latch is
cleared only by a *later death*, so the start that **ends** a storm is suppressed
too — and a channel that recovers for good never writes another row at all. The last
durable event stays a `channel.died`, which reads as "still broken" for a channel
that is healthy again. There is no other durable row carrying "it recovered".

Gating it on its own counter does not work either: `failures` resets to 0 on every
stable death, so in the #522 band a start-count gate would never engage.

So `channel.started` is written unconditionally. It is the row an operator actually
queries, and its absence is now trustworthy.

### Resulting row counts

| scenario | today | after |
| --- | --- | --- |
| 24 h sustained bring-up outage | ~1440 `boot_failed` | **~57** (≈10 before the backoff-plus-threshold escalates, then 47 repeats) |
| 24 h flap at 61 s cycles, restarts succeeding first try | ~1416 `died` + ~1416 `started` | **~1470** (~1416 ungated `started` + ~53 `died`: 5 at storm onset, then 48 repeats) |
| transient blip (the common case) | fully recorded | **unchanged** |

The last row is the one that matters most: the gate only engages once an alarm has
already spoken, so the ordinary transient failure that resolves in seconds is
recorded exactly as it is today.

**The flap figure carries an assumption, stated here because it is easy to miss.**
It holds only when each restart succeeds on its first attempt. A stable death takes
`Outage::Ends` → `record_success()`, which clears the escalator's latch — so if a
restart's first attempt fails transiently before a later one succeeds, that failed
attempt writes an **ungated** `boot_failed` row every cycle, because the latch was
just cleared and `CHANNEL STILL DOWN` never fires in this band at all. Closing that
is deliberately **not** done here: OR-ing `deaths.in_storm()` into the bring-up gate
would be a bare latch read with no preceding `record()` — the stale-latch hazard
`the_first_death_of_a_fresh_storm_is_recorded` exists to forbid — and it would
suppress the `cause` string, the only forensic field in the whole row set. Filed as
[#523](https://github.com/hherb/kastellan/issues/523).

This also halves the exposure to [#515](https://github.com/hherb/kastellan/issues/515):
`emit` is awaited (deliberately, for row ordering and test determinism) and the
production sink inherits sqlx's 30 s pool-acquire timeout, so fewer awaited writes
is strictly less shutdown delay in an already-broken state. It does not close #515.

## The flapping alarm

`RespawnRateAlarm` (`core/src/channel/respawn_alarm.rs`) is already a pure sliding
window over caller-supplied `Instant`s that owns no clock and spawns nothing — #522's
option (1) verbatim. Two **additive** changes:

- **`with_repeat(Duration)`** — a new `repeat: Option<Duration>` field defaulting to
  `None`, which preserves today's exact "fire once per storm" semantics, so
  `PersistentWorker` is byte-identical. Same builder shape as
  `DowntimeEscalator::with_stable_uptime`, and the same `threshold`/`repeat` pairing
  the escalator already has, so the two policy types read alike.
- **`in_storm() -> bool`** — a read-only accessor over the existing `armed` field.

The supervisor constructs one **outside** the retry loop. That placement *is* the
fix: it is the single structural difference from `PersistentWorker`'s alarm, and it
is what a mutation test pins.

### Constants

```rust
const FLAP_ALARM_WINDOW:    Duration = Duration::from_secs(3600);
const FLAP_ALARM_THRESHOLD: usize    = 5;
const FLAP_ALARM_REPEAT:    Duration = Duration::from_secs(1800);
```

- **Window 3600 s, not 300 s.** A longer window costs *nothing* in detection latency
  for a fast flap — five deaths at 67 s spacing trip the threshold at ~4.5 min under
  either window, because the window only governs pruning. It is the only thing that
  catches the slow half of the band: "up 200 s → dead" is ~430 restarts/day and a
  300 s window never holds more than two of them.
- **Threshold 5** matches `PersistentWorker::ALARM_THRESHOLD` for the same failure
  shape. Five channel deaths inside an hour is not a benign maintenance sequence.
- **Repeat 1800 s** is `DowntimeEscalator::DEFAULT_REPEAT`. Since the alarm now also
  gates the rows, the repeat is what keeps the durable trail alive: without it a
  three-day flap leaves ~6 rows in total and the rotating daemon log is the only
  record.

A repeat can only fire on a `record()` call, and `record()` is only called on a
death — so a storm that ends produces no further lines, without needing a timer.

### The line

```rust
pub const CHANNEL_FLAPPING_LOG_PHRASE: &str = "CHANNEL FLAPPING";
```

A `const` from the outset rather than a literal typed twice, because #516's finding
was precisely that an operator-facing phrase typed in two places drifts and the test
that pinned the literal stayed green through it. Emitted at `error!` alongside the
in-window death count and the window length.

### Deliberately not done

**Coupling the flap into `DowntimeEscalator`'s clock.** Making a stable death
`Outage::Continues` while a storm is in progress would fold healthy time into
`down_secs` — in the one line whose text asserts that nothing sent to the channel has
been received for that long. That is exactly the defect #521's review round removed;
reintroducing it from the other direction is not an improvement. The two alarms
answer different questions and keep separate clocks and separate wording.

**Raising `STABLE_UPTIME`** (#522's option 3) — it moves the band rather than closing
it, and it would make genuine short outages back off harder than they should.

## Files

| file | lines today | change |
| --- | --- | --- |
| `core/src/channel/respawn_alarm.rs` | 163 | `+ with_repeat`, `+ in_storm`; module doc gains its second consumer |
| `core/src/channel/boot_supervisor/downtime.rs` | 315 | `+ has_escalated()` |
| `core/src/channel/boot_supervisor/reporting.rs` | **new** | all reporting policy in one place |
| `core/src/channel/boot_supervisor.rs` | 451 | holds the policy; the two recurring arms gate their `emit` |
| `core/src/install/plan.rs` | — | operator help correction, see below |

`reporting.rs` takes `Outage` and `note_outage` from the parent (they are reporting
policy, and `boot_supervisor.rs` hit the 500-line cap last session) and adds
`should_record`, the flap constants and the flapping phrase.

**As built, the split landed one notch differently from this table** and the shipped
version is the better one: the *emission* stayed in the parent as
`boot_supervisor::report`, because that is where the channel label lives. Only the
deciding moved. `reporting.rs`'s own module doc states the boundary — it owns the
deciding, the parent owns the logging. `Outage` and `note_outage` are `pub(super)`,
not `pub`: `kastellan-core` is published to crates.io, and neither is anything an
external caller should be able to drive.

It also owns a small `ReportingPolicy` that holds *both* alarms and answers one
question per event (`Verdict { record, still_down, flapping }`). That replaces the
`DowntimeEscalator` parameter of `ChannelSupervisor::spawn` rather than adding a
sixth one, so the supervisor never touches either alarm directly and the spec's
"one place the policy lives" property is enforced by visibility rather than by
convention. `with_stable_uptime` is re-exposed as a delegating builder, so the
existing test call sites change by one type name. The two production call sites
(`main/matrix_boot.rs`, `main/email_boot.rs`) each change one line.

`downtime.rs` stays the pure escalator and `respawn_alarm.rs` stays the pure alarm;
neither learns about the other, and neither learns about `ReportingPolicy`.

`Verdict` carries the two alarms as separate `Option` fields rather than one enum:
on a flapping death both can fire in the same iteration, and inventing a precedence
rule between "still down" and "flapping" would be a policy decision nobody asked
for.

### The operator help is currently wrong after this change

`install::plan::render_email_help` says:

> Every attempt is durable in audit_log as `channel.boot_failed`, and success as
> `channel.started`

That becomes false. The help must name the **two independent** gates separately —
`channel.boot_failed` on the downtime clock (`CHANNEL STILL DOWN`),
`channel.died` on the flap alarm (`CHANNEL FLAPPING`) — say that `channel.started`
is never gated, and say that the daemon log is the per-event record. Naming them as
one mechanism is wrong in exactly the #522 band, where `CHANNEL STILL DOWN` can
never fire yet `channel.died` still goes quiet.

**A prediction in an earlier draft of this spec turned out to be the source of a
defect**, and it is worth recording rather than deleting. It read: *"The existing
test iterates `channel::actions`, so no new action name is introduced and that test
needs no change."* True as far as it goes — and it is why the new `CHANNEL FLAPPING`
phrase shipped into the help as a **bare literal**, alongside four neighbours that
are all interpolated from consts. The guard iterates *actions*; this branch
introduced a new *log phrase*. Caught by the final whole-branch review and fixed:
`@@CHANNEL_FLAPPING@@` is now substituted from `CHANNEL_FLAPPING_LOG_PHRASE`, and
the help test asserts through the const. This is #516's finding recurring inside the
very change that documents itself as immune to it — the lesson being that a guard
protects the category it enumerates and nothing adjacent to it.

A third instance of the same class survives and is filed as
[#524](https://github.com/hherb/kastellan/issues/524): `CHANNEL STILL DOWN` is still
a bare literal in both the help text and `report`'s `error!`, and is now the only
operator-facing phrase in that block that is not const-driven.

## Test file split

`boot_supervisor/tests.rs` is 719 lines and this adds ~20 tests. It becomes a
directory:

```
boot_supervisor/tests/
├── mod.rs         # RecordingSink, scripted(), fast_backoff(), growing_backoff(), death_delays()
├── bringup.rs     # #514: retry, fatal, unconfigured, shutdown ordering
├── liveness.rs    # #517: death → restart, flap guard, death-racing-shutdown
└── reporting.rs   # #518/#522: row gating and the flap alarm
```

The handover already lists this file as the cheapest split in the backlog, and it is
the file this work grows.

## Tests

**Pure layer, first (TDD):**

- `with_repeat` fires again once the interval has elapsed, and stays silent inside it.
- A `None` repeat reproduces today's behaviour exactly (the `PersistentWorker`
  byte-identity claim, asserted rather than trusted).
- `in_storm()` is false before the alarm trips, true after, and false again once the
  storm clears (this is the accessor the sampling-order argument rests on).
- `has_escalated()` tracks `last_escalated` and is cleared by `record_success()`.
- `should_record`'s four-row truth table.

**The loop, against the existing `RecordingSink`:**

- `boot_failed` rows stop once the outage escalates and resume on the next escalation.
- A `fatal` row is never gated.
- `died` rows stop inside a storm and resume when the alarm repeats.
- **The alarm accumulates across restarts** — the #522 regression test.

**Mutation checks, run rather than assumed:**

- Moving the alarm construction inside the loop must fail the accumulation test and
  nothing else.
- Sampling `in_storm()` *before* `record()` instead of after must fail the
  "first death of a fresh storm is recorded" test.

## Gate

Hermetic and `cfg`-free — no `target_os` arms anywhere in the diff, so both hosts see
the same suite and a count landing exactly on the prediction is the cross-check that
neither is blind to the other.

- Mac: targeted `cargo test -p kastellan-core --lib` plus
  `clippy -p kastellan-core --all-targets -D warnings`, under a private
  `CARGO_TARGET_DIR` under `$HOME`.
- DGX (authoritative): full-workspace `cargo test --workspace -- --nocapture`, and
  `clippy --workspace --all-targets -D warnings`. The expected count is the current
  baseline **3028** plus exactly the number of tests this branch adds, stated as a
  prediction *before* the run: a count that lands on it confirms neither host is
  seeing a different suite, and a count that does not is a finding, not a rounding
  error.

No live-host verification is required: nothing here changes when a channel restarts,
only what is said about it. The live behaviour #521 verified on the DGX is unchanged.

## Out of scope

- Retry policy. Unbounded retry stays (#514); there is no circuit breaker here.
- [#515](https://github.com/hherb/kastellan/issues/515) — the unbounded await on the
  audit sink during shutdown. This reduces the exposure by writing fewer rows; the
  5 s `tokio::time::timeout` remains its own slice.
- [#497](https://github.com/hherb/kastellan/issues/497) — unifying the per-family
  `ChannelBus` instances.
