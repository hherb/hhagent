# Channel bring-up is supervised, not one-shot — design

**Issue:** [#514](https://github.com/hherb/kastellan/issues/514) · **Date:** 2026-08-04 ·
**Branch:** `fix/514-supervise-channel-boot`

## Problem

`core/src/main/matrix_boot.rs` tries to bring the Matrix channel up exactly once.
Every failure arm logs and returns `None`:

```rust
Ok(Ok(Err(e)))   => error!(… "matrix worker spawn/login failed; channel not started"),
Ok(Err(join_err))=> error!(… "matrix worker spawn task panicked; channel not started"),
Err(_elapsed)    => error!(   "matrix worker login timed out (60s); channel not started"),
```

So a **transient** failure in the first seconds of daemon startup disables the
channel for the lifetime of the process. `PersistentWorker` supervises the
worker *after* a successful login; nothing supervises the login itself.

Observed live on the DGX 2026-08-03 19:07 → 2026-08-04 07:22 — **12 hours deaf**
with every unit `active` and Postgres healthy. Three messages sent in that
window sat queued on the homeserver and were ingested within one second of a
manual `systemctl --user restart kastellan-core`. The trigger was a
restart-window race, *not* the homeserver: the user manager was itself shutting
down (`exit.target has 'start' job queued`), so `systemd-run --scope` could not
create the egress sidecar's cgroup; the replacement manager then started
`kastellan-core` before the proxy path was usable and the worker's initial sync
failed its CONNECT.

Not a one-off — `channel not started` appears in the daemon log on 2026-06-21
(×3), 07-02 (×2), 07-12 and 08-03. Every occurrence is a startup-window failure
a retry would have absorbed. `email_boot.rs` has the identical one-shot shape.

The detection path is the worst part: "channel down" is indistinguishable from
"nobody messaged me" until a human notices silence.

## Goals

1. A transient failure at startup must not permanently disable a channel.
2. A **static** misconfiguration must *not* become an unbounded retry loop —
   it must stop, loudly, exactly as today.
3. Downtime must be visible after the fact, without reading the whole log.
4. The daemon must still shut down cleanly at any point — mid-retry, or after
   a channel came up late.

**Non-goals** (both written up on #514):

- `After=network-online.target` on the generated unit (issue item 3). Verified
  on the DGX that a `systemd --user` manager has **no** `network-online.target`
  at all (`systemctl --user list-unit-files | grep -i network` → nothing), so
  the ordering would be against a unit nothing activates. It also would not
  have helped the observed trigger, which was a manager-shutdown transaction.
- A channel-state section in `kastellan-cli status` (issue item 2, second
  half). The CLI is a separate process, so this needs DB-backed state — its
  own slice. The audit rows below are the durable record in the meantime.

## Architecture

One reusable supervisor; both boot modules become pure-ish "attempt" functions
that describe *what happened* and leave the retry policy to the supervisor.

```
main.rs
  ├── ChannelSupervisor::spawn("matrix", backoff, audit, || matrix_boot::attempt(…))
  └── ChannelSupervisor::spawn("email",  backoff, audit, || email_boot::attempt(…))
                       │
                       ▼
        channel/boot_supervisor.rs        ← retry loop, owns the running channel
          ├── BootOutcome                 ← the taxonomy the attempt returns
          ├── StartedChannel              ← opaque "running + how to stop it"
          └── boot_supervisor/downtime.rs ← pure DowntimeEscalator (no clock, no I/O)
```

Rejected alternatives:

- **Inline the retry loop in each boot module.** No new module, but the same
  loop duplicated across two structurally-identical files — the duplication
  trap PR #511 had just finished undoing in the supervisor crate.
- **Push retry into `PersistentWorker`.** Wrong layer: the failures include
  `PgCompletedTasks::connect` and `ChannelBus` construction, which the worker
  lifecycle layer knows nothing about.

## Components

### `BootOutcome` — the taxonomy

```rust
pub enum BootOutcome {
    /// Not configured (env unset). Stop; say nothing — this is the default
    /// for a daemon built without that channel.
    NotConfigured,
    /// Came up. The supervisor parks until shutdown, then stops it.
    Started(StartedChannel),
    /// Failed for a reason a later attempt could plausibly absorb.
    Retry(anyhow::Error),
    /// Failed for a reason no retry can fix. Stop; log loudly.
    Fatal(anyhow::Error),
}
```

`Fatal` is what keeps goal 2. The mapping:

| Outcome | Matrix | Email |
| --- | --- | --- |
| `NotConfigured` | `KASTELLAN_MATRIX_HOMESERVER_URL` unset | `KASTELLAN_EMAIL_ENDPOINT` unset |
| `Fatal` | `forced_localhost_homeserver` — a `localhost`-NAME homeserver is statically dead once egress is force-routed, and retrying it *is* the respawn-loop that check (#459) exists to prevent | `EmailConfig::from_env` partial-config `Err` — the process environment is fixed for its lifetime, so the existing "fix what `error` names, then restart the daemon" message is already the literal truth |
| `Retry` | spawn/login `Err`, join panic, 60 s login timeout, `PgCompletedTasks::connect` `Err` | `spawn_email_worker` `Err`, `PgCompletedTasks::connect` `Err` |

`spawn_email_worker` failing is a *sandbox* failure, which is exactly the
observed #514 trigger (`systemd-run --scope` refused during a manager restart)
— so it is retryable even though email's config errors are not.

### `StartedChannel` — an opaque running channel

The supervisor never names `ChannelBus`: `StartedChannel` wraps a boxed
`FnOnce() -> BoxFuture<'static, ()>` shutdown thunk, with a `from_bus`
constructor for production and a generic one for tests. Two reasons: the
supervisor stays independent of the channel layer's types, and tests can hand
it a probe that records whether it was shut down exactly once.

### `ChannelSupervisor` — the retry loop

```rust
ChannelSupervisor::spawn(label, backoff, audit, attempt) -> ChannelSupervisor
ChannelSupervisor::shutdown(self)                        // async, idempotent-by-move
```

The loop, in one tokio task:

1. Run `attempt()`, `select!`ed against the shutdown signal.
2. `Started(ch)` → record success (reset the escalator, one audit row), then
   await the shutdown signal and call the channel's shutdown thunk. Done.
3. `NotConfigured` → return silently.
4. `Fatal(e)` → log the loud "channel is OFF, daemon is fine" line, one audit
   row, return.
5. `Retry(e)` → warn with attempt number + next delay, audit row, ask the
   escalator whether this is an escalation point, then sleep the backoff
   `select!`ed against the shutdown signal and loop.

Attempts are **unbounded** — a homeserver can be down for an hour, and the
daemon should reconnect when it returns rather than need a human. Delays come
from the existing pure `RestartBackoff::default()` (1 s base, ×2, 60 s cap),
reused from `worker_lifecycle` rather than reinvented.

Shutdown is a `oneshot`; `shutdown()` sends it and awaits the task. An attempt
already in flight is **abandoned rather than cancelled** — identical to today's
60 s-timeout arm, which already leaves its `spawn_blocking` task draining
against the SDK's own HTTP timeouts, and harmless because workers are spawned
`--die-with-parent`.

### `DowntimeEscalator` — pure

Deliberately shaped like the existing `channel::respawn_alarm::RespawnRateAlarm`:
a state machine over caller-supplied `Instant`s that owns no clock and spawns
nothing, so it is unit-testable without threads or sleeps.

```rust
record_failure(now) -> Option<Duration>   // Some(downtime) ⇒ log loudly now
reset()                                   // on success
```

Fires the first time downtime passes a threshold, then at most once per repeat
interval, so a long outage neither goes silent nor spams. Defaults: escalate
after 5 minutes down, repeat every 30 minutes.

### Audit rows

Two new `channel::actions` constants, written through a boxed-closure sink (the
existing `AckOnlyAudit` idiom) so the supervisor itself stays DB-free and
hermetically testable:

- `channel.started` — payload `{channel, attempts}`.
- `channel.boot_failed` — payload `{channel, attempt, retry_in_ms, fatal, cause}`.

`cause` is capped before it becomes a durable payload value (the same
defence-in-depth reasoning as `email_boot::cap_reason`, which moves next to the
supervisor so both callers share it). Never message content — only the channel
name, counters and the error text.

## Data flow

Unchanged once a channel is up: the supervisor hands `ChannelBus::spawn` the
same arguments today's boot modules do, and the bus owns the inbound/outbound
pumps exactly as before. The only structural change in `main.rs` is that it
holds two `ChannelSupervisor`s instead of two `Option<ChannelBus>`, and calls
`shutdown()` on each unconditionally in the same place, in the same order.

## Error handling

Every failure remains fail-soft for the daemon: no arm aborts startup, and
`BootOutcome` has no `Err` variant, so no future `?` can reintroduce the abort
(the property `email_boot`'s docs already call out and rely on).

Log levels: `warn!` per failed attempt (with attempt number and next delay),
`error!` at each escalation point and for `Fatal`, `info!` on success naming
how many attempts it took.

## Testing

TDD, hermetic, no network and no DB:

**`DowntimeEscalator`** (synthetic `Instant`s): below threshold → `None`; first
crossing fires with the elapsed downtime; a second failure inside the repeat
interval stays silent; past the repeat interval fires again; `reset()` clears
the first-failure mark so the next outage starts fresh.

**`ChannelSupervisor`** (scripted `BootOutcome`s, a probe `StartedChannel`, a
1 ms backoff):

- retries until success, and the attempt count is what the success audit says;
- `Fatal` stops the loop (no further attempts) and audits `fatal: true`;
- `NotConfigured` stops the loop and audits nothing;
- shutdown while backing off returns promptly, without waiting out the delay;
- shutdown after a successful start invokes the channel's shutdown thunk
  exactly once;
- the audit sink sees one `boot_failed` row per failed attempt and one
  `started` row on success.

**Boot-module mapping** is covered where it is cheap and honest to do so: the
`Fatal`-vs-`Retry` classification for the two static-misconfiguration cases
(`forced_localhost_homeserver`, a partial `EmailConfig`) is unit-testable
without spawning anything, and those are the two arms goal 2 depends on.

The existing hermetic channel e2es (`email_channel_e2e`, `email_mitm_e2e`) must
keep passing unchanged — they exercise the channel loop below this layer.

## Verification

Full-workspace `cargo test` + `cargo clippy --workspace --all-targets -D warnings`
on the DGX (native bwrap + live PG). No Linux-only or macOS-only code is added,
so the Mac's structural blind spot does not apply here; the supervisor and the
escalator compile and run identically on both.

Live confirmation is deliberately *not* part of this slice's gate: reproducing
the trigger means restarting the user manager mid-boot. The honest live check
is a deploy plus a `channel.started` row in `audit_log` with `attempts: 1`.
