# The ask path — #564 slice 1b

Issue: [#564](https://github.com/hherb/kastellan/issues/564). Base: `main` @ `e8ea4339`.
Predecessor: [`2026-08-16-ask-record-slice-1a-design.md`](2026-08-16-ask-record-slice-1a-design.md)
(merged as [#570](https://github.com/hherb/kastellan/pull/570), `06c65ae7`).
Also closes [#571](https://github.com/hherb/kastellan/issues/571).

## Why this exists

Slice 1a built the durable record and **nothing calls it**. `db::asks::raise`, `resolve`,
`expire_due` and the four `db::tasks` helpers behind them have zero production callers; #571
records the sharpest instance of that. Meanwhile `inner_loop.rs`'s `Verdict::Escalate` arm still
degrades to `Block` behind a `TODO(channel-bus)`, so CASSANDRA's one verdict for *"a human must
decide this"* remains unrepresentable at runtime.

Slice 1b is the caller. After it, an escalated plan suspends its task on a durable question, an
operator answers it, and the task resumes and is re-reviewed — or it expires and fails closed.

**This is a real behaviour change on the live DGX, and it is not strictly an improvement in
throughput.** Today an escalating plan degrades to `Block` and the agent gets another iteration to
revise, which often completes. After this slice the task stops until a human answers. That is the
point — a verdict that means "ask someone" should ask someone — but it means an unattended daemon
will accumulate suspended tasks that a deadline eventually fails.

## What slice 1b delivers

1. `Outcome::AwaitingOperator` + `Outcome::Denied`, and `final_state() -> Option<&'static str>`.
2. New `core/src/scheduler/asks.rs`: the pure `decide` / `deadline_from_env`, plus
   `raise_and_suspend` and `sweep_expired_and_audit`.
3. `db::asks::latest_resolved_for_task` — the one new read, serving both decisions.
4. `drain_lane`'s non-finalize branch.
5. `run_one` seeds the resumed task's plan count and extends its budget.
6. A periodic expiry sweep task in `spawn_scheduler`, plus one call at daemon startup.
7. `ask.raised` / `ask.resolved` / `ask.expired` audit rows.
8. `kastellan-cli inbox list | show <id> | resolve <id> approve|deny [--note …]`.

## Design decisions

### D1 — `final_state()` returns `Option`, rather than a pseudo-terminal string

`Outcome::AwaitingOperator` is the first non-terminal outcome the inner loop can return. The
cheap move is to have `final_state()` answer `"awaiting_operator"` and let `drain_lane` pass it to
`tasks::finalize`. That is wrong twice: `finalize` matches `WHERE state = 'running'` and the row is
already `awaiting_operator`, so the UPDATE would silently no-op (slice 1a's D7 kept `finalize`'s
contract narrow precisely so this stays true); and the terminal lifecycle + finalize audit rows
would assert a task ended when it did not.

So the signature becomes `Option<&'static str>` and every call site is forced to say what it does
with a task that has not finished. That is ~10 sites, all trivial, and the compiler finds them —
where a sentinel string would have been found by an operator reading a wrong audit row.

### D2 — Deny is task-terminal, and the check runs before any planning

`asks::resolve` re-enqueues the task on **any** resolution, so a denied task also returns to
`pending`, gets claimed, and replans from scratch. If denial only bound to the plan digest, the
hole is direct: the operator denies plan P, the agent replans P′, P′ passes review, and the thing
that was just refused executes. An operator saying "deny" means *don't do this*, not *try
differently*.

So `run_one` reads the task's latest resolved ask **before** formulating anything, and a `deny`
terminates immediately. The task still goes through `pending → running` — which is deliberate, not
sloppy: the terminal transition is then written by the lane runner through the same path as every
other terminal transition, rather than by a special case inside `resolve`.

### D3 — `Denied` writes `tasks.state = 'blocked'`, and the honesty lives in the payload

A third terminal state (`denied`) would need a migration to widen `tasks_state_check`, and would
partition every existing observation query that groups on terminal states. Against that: the
operator *is* the review authority of last resort, so `blocked` is not a lie.

What would be a lie is reusing `Outcome::Blocked`'s payload, which carries a `principle: u8` — a
CASSANDRA principle number. An operator denial violates no principle, and inventing one would
fabricate a record in the log whose job is to be trustworthy (the `producer_cancel_suspended`
lesson from slice 1a's own review wave). So `Outcome::Denied { ask_id, reason }` is its own variant
with `final_state() == Some("blocked")` and
`result_payload() == {"kind":"denied","ask_id":…,"reason":…}`, where `reason` is the **ask's
`body`** — the reviewer's escalation concern, i.e. what the operator was answering. The operator's
optional `--note` is not copied here: it lives in `asks.resolution` and in the `ask.resolved` audit
row, which is the D10 boundary. The `ask.resolved` row also carries `choice`, so observation SQL can
still separate operator-denied from CASSANDRA-blocked despite the shared state.

### D4 — The resolved ask is an INPUT to the loop, read once

`run_one` reads `db::asks::latest_resolved_for_task` **once**, before formulating. The deny check
consumes that value locally (D2); if it is not a deny, the ask is threaded into a new
`TaskContext.resolved_ask: Option<Ask>` for the `Escalate` arm's digest comparison.

The alternative — query inside the `Escalate` arm — costs a second round trip and makes the
decision invisible in the loop's type. As a field it is constructible by a test without a live PG,
which matters because the `Escalate` arm is otherwise reachable only through a PG e2e.

"Latest resolved" is the right selector rather than "resolved matching this digest": one read then
answers both questions, and if a task escalated twice (P approved, P′ denied) the latest resolution
is the operative one. Ordering is `resolved_at DESC, id DESC` — `resolved_at` is `now()` at resolve
time and two asks resolved in the same tick would otherwise order nondeterministically, the same
tiebreaker `list_pending` already carries.

**Two pure functions, not one.** The deny check has no plan and therefore no digest, so a single
`decide(ask, digest)` would have to take an `Option` and answer "not for this plan" to a caller that
has no plan. Instead:

```rust
pub enum Choice { Approve, Deny }
pub enum AskDecision { Approved, Denied, NotForThisPlan }

/// The ask's resolution, validated against its own `options`. `None` for
/// a malformed or absent resolution.
pub fn resolution_choice(ask: &Ask) -> Option<Choice>;

/// What a resolved ask means for a specific plan.
pub fn decide(ask: &Ask, plan_digest: &str) -> AskDecision;
```

`decide` is written in terms of `resolution_choice`, so the validation lives once and is the guard
the mutation set aims at.

### D5 — The nonce is dropped unread in this slice, and that is the correct behaviour

`raise` returns the plaintext nonce exactly once. Slice 1b has no transport that needs it — the CLI
resolves by row id, which `resolve`'s doc comment reserves for exactly this trusted-local caller.
So `raise_and_suspend` binds it to `_` and lets `Drop` zeroize it.

It must not be logged. `RaisedAsk`'s `Debug` redacts, but a `tracing::info!(nonce = %…)` would put a
live approval token into `~/.local/state/kastellan/*.out`, a plaintext file with none of
`audit_log`'s role gating — the same exposure `docs/threat-model.md`'s "User data in the daemon log"
subsection was added for. Slice 2 delivers the nonce over Matrix at raise time; it does not need to
recover one raised earlier.

### D6 — The plan count carries forward, and the budget is extended

`resume_from_ask` deliberately leaves `tasks.plan_count` alone and defers the policy call here.
Today `run_one` rebuilds `TaskContext` with `plan_count: 0` and `increment_plan_count` writes the
**absolute** value back, so a task that escalated after 4 plans records 4 → 1. That is not a display
quirk; the column stops being a true record of how many plans a task ran.

Two separable concerns, and the current code conflates them:

- **the column is a historical fact** — `run_one` seeds `TaskContext.plan_count` from
  `task.plan_count`, so it only ever grows;
- **the budget is a policy** — `max_plans` becomes `carried + override`, so an approved plan still
  gets a full allowance to execute.

Strict carry-forward (seed but do not extend) was considered and rejected: a task that escalates on
its last allowed plan would resume with zero budget and die at the cap immediately, making the
operator's approval useless. Runaway is not the risk it looks like, because every additional
allowance costs one human interaction.

**A consequence to state rather than bury.** The single `task.finalize` row a resumed task writes
will report `total_llm_calls` as the task's **lifetime** total while `total_duration_ms` covers only
the post-resume run, because `claim_one` restamps `started_at` on the resume claim. The field is
named *total*, so the lifetime reading is the more defensible of the two, but the row mixes scopes
and an observation query that divides one by the other will be wrong for resumed tasks. The `ask.*`
rows carry enough to reconstruct both. Widening the finalize payload is deliberately left out of
this slice.

### D7 — The sweep is periodic, not startup-only

Slice 1a's spec put the expiry sweep at daemon startup, beside `crash_recovery::sweep_and_audit`.
On a daemon that runs for weeks that is not a deadline: an unanswered ask holds its task in
`awaiting_operator` until the next restart, which is the permanent wedge the deadline exists to
prevent.

The security half already holds without any sweep — both resolvers carry `AND deadline_at > now()`,
so an expired nonce is dead on time regardless. What the sweep owns is the **task** side.

So: one call at startup, plus a third task in `spawn_scheduler` sweeping on its own 60 s interval
under the same shutdown watch. It is not folded into `drain_lane`, which is the per-lane claim hot
path and has nothing to do with a pool-wide sweep.

### D8 — A raise failure fails the task; it does not fall back to `Block`

`raise` errors when the task is not `running` — cancelled out from under the loop, or already
suspended. Falling back to the old degrade-to-`Block` would reinstate exactly the silent degradation
this slice deletes, and it would do so on the path where the reviewer said a human must decide. So
the outcome is `Outcome::Failed` naming the cause. If the row really was cancelled, `finalize` is a
no-op for it anyway and the lifecycle row records what the scheduler observed, which is the existing
audit-vs-DB divergence convention.

### D9 — The CLI subcommand is `inbox`, and it resolves by id

`kastellan-cli ask` already means *submit a task*. An `asks` subcommand differing from it by one
letter is a trap for exactly the operator who is tired enough to be answering an escalation. `inbox`
is also what #564 and the ROADMAP call this surface.

It calls `asks::resolve` (by id), not `resolve_with_nonce`. That is the split slice 1a's D3 argued:
an id has no unforgeability property, which is safe only because this caller is the operator's own
local binary. Any caller reachable from an untrusted transport must use the nonce form, and slice 2
will.

### D10 — Free text reaches the record and never the planner, structurally

The issue's constraint is that a resolution's free text is stored and shown but never interpolated
into a plan. In slice 1b that holds by construction rather than by discipline: `approve` carries
nothing into the resumed run except the digest comparison, and `deny` is terminal, so there is no
live plan for text to reach. The `--note` lands in `asks.resolution` and in the `ask.resolved` audit
row. The property needs a real guard only when slice 2 lets a channel peer supply it.

## Control flow

```
claim → run_one
          │
          ├── latest_resolved_for_task(task) ─── deny ──→ Outcome::Denied   (no planning)
          │                                  └── approve ─→ ctx.resolved_ask
          │
          └── formulate P → review
                             │
                             └── Verdict::Escalate, plan.refused.is_none()
                                   │
                                   ├── ctx.resolved_ask approves digest(P) ──→ proceed
                                   └── otherwise → raise_and_suspend
                                                     │
                                                     ├── Ok  → Outcome::AwaitingOperator
                                                     └── Err → Outcome::Failed          (D8)

drain_lane: final_state() == None → no finalize, no terminal row, no finalize row

inbox resolve → asks::resolve → task 'pending' + NOTIFY tasks_resumed → claim → run_one
sweep (60 s) → asks::expire_due → task 'failed' {kind:error, detail:"ask_timeout"} + ask.expired
```

`Verdict::Escalate` on a **refusal** plan keeps its current behaviour unchanged: the refusal is
terminal, nothing is raised, and the existing `info!` stays.

## Audit rows

| action | actor | payload | written by |
| --- | --- | --- | --- |
| `ask.raised` | `scheduler` | `{ask_id, task_id, kind, plan_digest, severity, deadline_at}` | `raise_and_suspend` |
| `ask.resolved` | `cli` | `{ask_id, task_id, choice, resolved_by, free_text}` | the `inbox` command |
| `ask.expired` | `scheduler` | `{ask_id, task_id}` | `sweep_expired_and_audit` |

All three are best-effort inserts, matching `write_lifecycle_row` and `crash_recovery`'s posture: a
transient `audit_log` failure must not roll back a state transition that already committed. The
constants live beside the existing `ACTION_TASK_*` in `scheduler/audit.rs` so nothing greps for a
literal.

## Testing

TDD — tests first, in this order.

**Unit (no PG) — `scheduler/asks.rs`:**
- `resolution_choice`: `{"choice":"approve"}` → `Approve`; `{"choice":"deny"}` → `Deny`; absent,
  non-object, missing `choice`, or a `choice` outside that ask's own `options` → `None`. `resolve`
  already enforces the closed set on the way in, so these arms are defence in depth against a
  direct-SQL writer or a future resolver — which is exactly why they must answer `None` and never
  `Approve`
- `decide`: approve + matching digest → `Approved`; approve + different digest →
  `NotForThisPlan`; deny → `Denied` regardless of digest; an ask with `plan_digest: None` →
  `NotForThisPlan`; a malformed resolution → `NotForThisPlan`, i.e. the safe arm, never `Approved`
- `deadline_from_env`: default 86400; a parsed override; a non-numeric, zero, or negative value
  falls back to the default rather than minting an ask the `asks_deadline_after_created` CHECK
  rejects

**Unit — `Outcome`:**
- `final_state()` is `None` for `AwaitingOperator` and `Some("blocked")` for `Denied`
- `Denied`'s `result_payload` carries `kind: "denied"` and the `ask_id`

**PG e2e — `core/tests/`, with a scripted review stage returning `Escalate`:**
- escalate → task lands `awaiting_operator`, an `asks` row exists, **no** `task.finalize` row and
  **no** terminal lifecycle row were written
- resolve approve → task resumes, replans, and the same plan **proceeds** rather than re-escalating
- approve then a *different* replan → a **second** ask is raised, not a silent proceed
- resolve deny → task terminates `blocked` with a `denied` payload, and the formulator is **never
  called** on the resumed run
- expiry → `failed` with `detail == ask_timeout`, plus the `ask.expired` row
- a raise against a task cancelled mid-plan → `Failed`, not `Blocked` (D8)
- plan count is monotonic across a suspend/resume cycle (D6)

**CLI e2e:** `inbox list` shows a pending ask; `inbox resolve` writes `ask.resolved` and returns the
task to `pending`; a second `resolve` of the same ask exits non-zero (first-responder-wins is
already a DB property — this pins that the CLI **reports** it rather than printing success).

**Mutations to run.** The #573 lesson is that a self-chosen mutation set measures the author, not
the coverage, so these aim at the shared guards: delete the deny check in `run_one`; make `decide`
return `Approved` on its malformed-resolution arm; drop the digest comparison so any approval
matches any plan; delete the `else` in `drain_lane`'s `let … else` so a suspended task finalizes;
drop the `carried +` from the budget extension; revert `raise_and_suspend`'s `Err` arm to
`ctx.blocks.push(...) + continue`.

## What this slice deliberately excludes

- **Matrix delivery and inbound nonce correlation** (slice 2). `channel/bus.rs` is strictly inbound
  task → outbound reply-on-completion; core-initiated outbound is its own piece of work.
- an `ask_user` planner tool and `propose_plan`-style approval — the same record with a different
  `kind`, and both are follow-ons the issue parks until the primitive is proven
- the expired-ask dead-letter surface
- widening the `task.finalize` payload to disambiguate the resumed-task scope mix (D6)
- the `Verdict::Escalate` severity-split in `cassandra/deterministic.rs` — it now has somewhere to
  land, but choosing the split is a separate judgement

## Open risk, to revisit after the first live escalation

The digest binding is **provisional** (slice 1a's D2 says so). Its predicted failure mode is
re-escalating on a semantically identical replan, which costs an operator interaction and is the
safe direction. The dangerous direction — an approval carrying to a plan the operator would not
recognise — is what the exclusion-list digest exists to make unlikely. The first real escalation on
the DGX is the measurement; do not tune the exclusion list before it.

---

## Addendum (2026-08-19) — D11: the suspend carries the step history

Added after the final whole-branch review. **This supersedes the original D4/D6 assumption that a
resumed task may simply replan from an empty history.**

### The defect

`run_one` rebuilt `TaskContext` with `plans: vec![]`, so a resumed task started from nothing and
re-formulated every iteration it had already run. Slice 1a's spec did say the resumed task
"replans from scratch" — that is why an approval binds to a digest — but it never stated the
consequence: **the steps of the earlier iterations execute again.**

Concretely: plan 1 sends an email; plan 2 escalates; the operator approves; on resume plan 1 is
re-formulated, passes review again, and the email is sent a second time. The feature whose entire
purpose is human oversight would cause duplicate side effects *because* a human approved something.

Every e2e escalated on the first plan with zero prior steps, so 34 new tests were structurally
blind to it.

### The fix

The suspension carries the run's history, and the resume restores it.

- **Migration 0024** adds `asks.resume_state JSONB NULL`. The state belongs to *this* suspension,
  and there is exactly one ask per suspension, so the ask row is its natural home — not
  `tasks.payload`, which is the producer's declared intent and must not become scheduler scratch.
- `db::asks::raise` takes `resume_state: Option<&serde_json::Value>`; `Ask` carries the column.
- `scheduler::asks::raise_and_suspend` serialises the live context into it.
- `run_one` restores it from the most recent resolved ask before building `TaskContext`.

### What is stored, and why it is the inputs rather than the renders

`PlanRecord` holds `plan: Plan` plus a **private** `rendered: Vec<RenderedStep>`, and `RenderedStep`
is neither public nor serde-able. That is not an obstacle to work around — it is a hint. `rendered`
is a *pure, deterministic function* of `(plan, outcomes)` (`PlanRecord::new` screens each outcome
under its step's own guard profile), so the honest thing to persist is the **inputs**, and to
rebuild the record by calling the same constructor on restore.

So `resume_state` is `{"plans": [{"plan": <Plan>, "outcomes": [<StepOutcome>]}], "advisories": [...],
"blocks": [...]}`. `Plan` and `StepOutcome` both already derive `Serialize`/`Deserialize`.

Two consequences worth stating:

- **`PlanRecord` must retain its `outcomes`.** It currently consumes them and keeps only the
  renders, so the raw inputs are unrecoverable at suspend time. Keeping them costs memory
  proportional to what the dispatcher already caps, and it is what makes the screened-once
  invariant survive a restore: the screen is re-applied by the same constructor rather than a
  serialized render being trusted.
- **`advisories` and `blocks` carry too.** They are accumulated reviewer feedback; dropping them
  would make the resumed planner repeat mistakes the reviewer already corrected.

### What this does NOT claim

Restoring the history makes the resumed planner *aware* of what it already did; it does not make
re-execution impossible. A planner that re-emits an identical step will still dispatch it. The
guarantee this buys is that the planner has the information not to — the same guarantee the loop
already relies on between ordinary iterations. A stronger property (idempotency keys per dispatched
step) is out of scope and is not implied anywhere in this document.
