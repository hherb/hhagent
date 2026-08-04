# A running channel is supervised too, not just its bring-up — design

**Issue:** [#517](https://github.com/hherb/kastellan/issues/517) · **Date:** 2026-08-05 ·
**Branch:** `fix/517-supervise-channel-liveness`

## Problem

[#516](https://github.com/hherb/kastellan/pull/516) made channel **bring-up** a
supervised claim: `ChannelSupervisor` retries a failed attempt with capped
backoff until it succeeds. But once an attempt returns `BootOutcome::Started`,
the loop parks on the shutdown oneshot and never looks at the channel again:

```rust
BootOutcome::Started(channel) => {
    let _ = (&mut shutdown).await;   // park until daemon shutdown
    channel.stop().await;
    return;
}
```

That is correct for **worker** death — `PersistentWorker`'s driver thread
respawns forever underneath a live bus. It is wrong for the bus's own pumps,
which have terminal exits nothing watches:

1. **The outbound pump.** `PgCompletedTasks::next_completed` returns `None` on
   any `PgListener` error, and `ChannelBus::spawn`'s pump is a
   `while let Some(id) = …` — so the pump returns for the life of the process.
   The agent still ingests messages and still completes tasks; the **replies
   never go out**.
2. **A per-channel task.** It `break`s when `ch.recv()` yields `None`, i.e. when
   the polled driver thread has exited. Inbound is dead for the life of the
   process.
3. **A panic in either.** Unenumerated, and therefore the one that will be
   discovered the way #514 was.

In all three cases the supervisor is parked, every unit is `active`, Postgres is
healthy, and the log goes quiet — the **exact** signature of #514, reached by a
different route, and with no `channel.boot_failed` row either.

So the claim carried in the handover and in memory — "a silent bot now means a
fatal-config case, not a restart case" — is true for the boot window only.

### What the failure actually is

`PgListener::recv()` **already auto-reconnects** (sqlx 0.9,
`sqlx-postgres/src/listener.rs`: *"If the connection to PostgreSQL is lost, it is
automatically reconnected on the next call to `recv()`"*). So the `Err` arm is
not a dropped connection — it is a **reconnect that failed**: a pool-acquire
timeout during a real Postgres outage, or `PoolClosed`. Making the pump
"reconnect harder" would therefore fix nothing that sqlx does not already do.

This is what decides the approach below: the reachable death is a *sustained*
Postgres outage, not a blip, and the honest response to it is to take the
channel down and bring it back up when Postgres returns.

## Goals

1. A pump death must not permanently disable a channel — recovery without a
   human, as for bring-up.
2. Recovery must not become a *worse* failure: a channel that dies immediately
   and repeatedly must back off, not spin.
3. A restart must not leak the worker, its sidecar, or the surviving pumps.
4. "Was up, then died" must be distinguishable from "never came up" in
   `audit_log`, after the fact.
5. Daemon shutdown must stay clean, including when it races a death.

## Non-goals

- Restarting an **individual** pump in place. The outbound pump owns the
  `CompletedTasks` seam and the sender map; a per-channel task owns its
  `Channel` by value. Rebuilding either in isolation means a second lifecycle
  shape next to the one #516 just established, for a failure whose cause
  (Postgres is gone) invalidates the whole channel anyway.
- Any change to what a *worker* death means. `PersistentWorker` already owns
  that and is not touched.

## Approach

The issue's primary shape: give `StartedChannel` a **liveness** signal
alongside its shutdown closure, so the `Started` arm selects instead of parking,
and a death falls back into the same retry loop — same backoff, same escalator,
same audit sink.

The alternative sketched in the issue (make `next_completed` reconnect, audit
the inbound break) is smaller but enumerates the two exits we happen to know
about: it cannot cover a panic, and its enumerated case is the one sqlx already
handles. Rejected.

The reason the primary shape is viable at all — and not a repeat of #502's
leaked sidecars — is that channel teardown is **RAII**. `ChannelBus::shutdown`
aborts each pump task; aborting drops the task's future, which drops the boxed
`Channel`; `MatrixChannel`'s drop closes both driver endpoints, the driver thread
exits, and its `PersistentHandle` drop tears down the supervisor, the worker and
the sidecar. A restart therefore starts from nothing left over.

## Design

### 1. `channel/pump_liveness.rs` — a new, pure-ish module

```rust
pub struct DeathBell(Arc<Notify>);   // held by the bus; `ring_on_drop()` mints guards
pub struct PumpLife(Arc<Notify>);    // held by a pump task; rings on Drop
```

Two decisions carry weight:

- **`Drop`, not code after the body.** A guard dropped by unwinding covers a
  **panicking** pump, which post-body code cannot; it also covers abort-cancel,
  which is what shutdown does (harmless — nobody is waiting by then).
- **`Notify::notify_one`, not `notify_waiters`.** `notify_one` stores a permit
  when there is no waiter yet, so a pump that dies *before* the supervisor
  awaits the signal still wakes it. `notify_waiters` drops that wakeup on the
  floor, and the resulting bug — deaf only when the death is fast — would be
  the hardest possible thing to reproduce.

### 2. `ChannelBus`

Holds a `DeathBell`, hands every spawned pump a `PumpLife`, and exposes:

```rust
pub fn death_signal(&self) -> BoxFuture<'static, ()>
```

`'static` (it clones the `Arc`) so the supervisor can hold it across awaits
without borrowing the bus it is also going to stop. `shutdown()` is unchanged.

### 3. `StartedChannel`

Gains a `died: BoxFuture<'static, ()>` next to the shutdown closure.

`StartedChannel::new()` keeps meaning **never dies** (`std::future::pending()`),
so every existing caller and test is byte-identical; `from_bus()` wires the real
signal. Tests script a death with `StartedChannel::new(…).with_death(fut)`.

### 4. The `Started` arm

```rust
let started_at = Instant::now();
let died = tokio::select! {
    biased;
    _ = &mut shutdown => false,
    _ = channel.wait_for_death() => true,
};
channel.stop().await;          // both paths: stop the surviving pumps
if !died { return; }
// … audit + backoff, then round the loop for a fresh attempt
```

The `biased` order is the **opposite** of the attempt select a few lines above,
and both directions are load-bearing:

- **Here, shutdown first** — a death racing daemon shutdown must not start a
  restart, which would spawn a worker (and sidecar) the daemon is about to
  abandon.
- **There, the attempt first** — a bring-up that has already *completed* must
  not be discarded in favour of shutdown, or a `Started` channel gets dropped
  without being stopped.

`channel.stop()` runs on both paths. On the death path the pump that died has
already returned, so aborting it is a no-op; the point is the ones that have
*not* died — including the per-channel task whose drop is what tears the worker
down.

### 5. Flap guard — the part that stops this becoming a worse bug

Resetting the failure counter on every `Started` would let a channel that dies
instantly restart at full speed forever: a hot loop spawning a worker per
iteration. So reset is conditional on the channel having **stayed up**:

```rust
pub const STABLE_UPTIME: Duration = Duration::from_secs(60);
pub fn ran_long_enough(ran: Duration) -> bool { ran >= STABLE_UPTIME }
```

- **Stable** (≥ 60 s): the failure counter resets to 0 and
  `DowntimeEscalator::record_success` clears the outage state, so the next retry
  waits the base delay and a later outage is timed from *its own* first failure
  rather than reporting hours of downtime the channel spent working.
- **Not stable**: the death counts as another failure in the same outage —
  backoff keeps growing to the 60 s cap, the escalator keeps counting, and a
  flapping channel gets the same loud line as one that will not come up.

60 s is the backoff cap, so "stayed up longer than the longest retry delay" is
the same threshold read from either side. Both pieces are pure and live in
`downtime.rs` with the rest of the policy — no clock, no tasks, unit-testable.

### 6. Audit

A new variant, mapped to a new action:

```rust
BootAudit::Died { ran_ms: u64, retry_in_ms: u64 }   // → actions::CHANNEL_DIED
```

Distinct from `boot_failed` on purpose: "never came up" and "was up for six
hours, then died" call for different operator responses, and collapsing them
into one action would throw away the only durable evidence of the second. Rows
are rare by nature (one per death, not one per attempt), so
[#518](https://github.com/hherb/kastellan/issues/518)'s row-spam concern does not
apply — the retries that *follow* a death are ordinary `boot_failed` rows and
are gated by whatever #518 lands.

Log line: `warn!` with `ran_ms` + `retry_in_ms`. Not `error!` — the supervisor is
handling it, and the escalator already owns "this is not resolving by itself".

### 7. Cheap step first in `attempt` (both boot modules)

`attempt` currently spawns and logs in the worker **first** and connects
`PgCompletedTasks` **last**. The dominant restart trigger is a Postgres outage
— so as written, every retry during that outage would spawn a sandboxed worker,
wait through a Matrix login and initial sync, and only then fail on the cheap
step, tearing all of it down again.

Connecting the listener first makes the restart loop cheap in exactly the
failure mode that drives it. It costs holding one pool connection across the
login (bounded by the existing 60 s timeout), which is not a scarce resource
here, and it changes no classification: the connect failure is `Retry` either
way.

## Testing

TDD throughout; everything below is hermetic — no network, no database, no
sandbox.

**`pump_liveness`** — a dropped guard rings; a guard dropped *before* anyone
waits still wakes the waiter (the `notify_one` permit); a panicking task's guard
rings; the bell stays silent while every guard is alive.

**`ChannelBus`** — ending the completed-task stream (the fake `CompletedTasks`
returns `None`) fires `death_signal()`; a healthy bus does not.

**`downtime`** — `ran_long_enough` at, below and above the threshold;
`record_success` clears the outage so a later failure is timed from itself.

**`boot_supervisor`** — a scripted `Started` whose death future fires
re-enters the loop and starts again (two `Started` audit rows with a `Died`
between); a death that is *not* stable does not reset the backoff (successive
`retry_in_ms` values grow); a stable death does reset it; a death racing
shutdown stops rather than restarting (`flavor = "current_thread"`, so the
ordering is deterministic rather than lucky).

## Risks

- **A restart is heavier than a pump restart.** A Matrix re-login and initial
  sync per recovery. Accepted: the reachable trigger is a Postgres outage that
  has already outlasted sqlx's own reconnection, and §7 makes the retry loop
  fail fast during it.
- **Both channels restart together on a Postgres outage.** They are independent
  supervisors with independent backoff; the churn is bounded by the 60 s cap and
  by §7's ordering.
- **`STABLE_UPTIME` is a judgement call.** Too low and a flapping channel
  escapes the backoff; too high and a genuinely healthy channel that dies at 50 s
  is treated as flapping (it waits one extra backoff step — harmless).
