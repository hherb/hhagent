# The durable ask record — #564 slice 1a

**Status:** approved 2026-08-16 · **Issue:** [#564](https://github.com/hherb/kastellan/issues/564) ·
**Scope:** the DB layer only. No scheduler change, no channel, no `Verdict::Escalate` wiring.

## Why this exists

`core/src/scheduler/inner_loop.rs`'s `Verdict::Escalate` arm degrades to `Block` with a
`warn!`. CASSANDRA's one verdict for *"a human must decide this"* is therefore a refusal at
runtime, and the degradation is visible only in the daemon journal — the audit row records
`verdict_kind=escalate`, not the fact that it was downgraded.

The TODO in that arm blames the missing channel bus. The channel bus landed; the arm is still
wrong, because the primitive it actually needs does not exist. `core/src/channel/bus.rs` is
strictly *inbound message → task → outbound reply on completion*. There is no way for core to
**initiate** an outbound message and suspend a task until a correlated answer returns.

This slice builds the durable record that primitive rests on, and nothing else.

## What slice 1a delivers

1. Migration `0023_asks.sql` — the `asks` table, the `awaiting_operator` task state, a
   `tasks_resumed` NOTIFY channel, and the role grants.
2. `db::asks` — `raise` / `resolve` / `expire_due` / `get` / `list_pending`.
3. `db::tasks` — `suspend_for_ask`, `resume_from_ask`, `fail_awaiting_operator`, and a widened
   `mark_cancelled`.
4. `core::cassandra::plan_digest` — one pure function defining what an approval binds to.

Every one of these is driven directly by a PG e2e. Nothing ships whose branches a test cannot
reach — see [What this slice deliberately excludes](#what-this-slice-deliberately-excludes).

## Design decisions

### D1 — An approval binds to a plan digest, and the resumed task is re-reviewed

`run_one` rebuilds `TaskContext` from `task.payload` with `plan_count: 0`, so a resumed task
**replans from scratch**; the escalated plan is gone. Three bindings were considered:

| Binding | Verdict |
| --- | --- |
| Nothing — the resolution is context for a fresh replan | Rejected. Approving plan P can result in plan P′ running, unreviewed by the human who approved. A denied ask also fails to reliably stop the agent re-proposing the same thing. |
| The plan itself, executed verbatim on resume, review skipped | Rejected. Strongest binding, but it introduces a review-bypass path into the one place the architecture has no carve-outs, and it means storing a plan (with its recalled memories and step parameters) in a second table. |
| **A digest of the plan; re-review on resume** | **Chosen.** |

The ask stores a hash of the plan that escalated. On resume the agent replans and goes through
CASSANDRA again as normal; if the new plan's digest matches the approved one, the `Escalate` arm
consults the resolved ask rather than raising a second one. A *different* plan escalates afresh.

This keeps "every plan is reviewed" intact, needs no bypass, and closes the approve-P-run-P′ gap
by construction. The cost is real and accepted: a nondeterministic planner may never reproduce the
approved plan, in which case the task escalates twice and the operator sees a near-duplicate ask.

### D2 — The digest covers the executable surface only

`plan_digest(&Plan) -> String` is SHA-256 over a canonical serialization of:

- per step: `tool`, `method`, `parameters`, `classification`
- plan-level: `data_ceiling`

It **excludes** `context`, `rationale`, and the per-step `returns` and `done_when`. Those are
narration the model regenerates differently on every call, and none of them is read by the
dispatcher — `dispatch_step` uses `tool`/`method`/`parameters`, and `classification` and
`data_ceiling` are what the deterministic policy enforces.

The trade-off cuts both ways and is the reason the boundary sits exactly here. Digest the whole
plan and it will essentially never match on replan, so approvals never carry and the binding is
decorative. Digest too little and an approval covers a plan that does something else. Confining it
to the executable surface covers 100% of what executes while leaving the digest stable enough to
actually match.

> ⚠️ **This selection is provisional and must prove itself in real use.** The revisit trigger is
> the first real escalation that re-escalates on a semantically identical replan — that is the
> signal the boundary is drawn too wide. The opposite signal (an approval carrying to a plan the
> operator would not recognise) is the more serious one and would mean it is drawn too narrow.
> Whichever fires first, re-derive the field list from what `dispatch_step` and
> `cassandra::deterministic` actually read at that time, not from this table.

### D3 — The nonce is stored hashed, and returned in plaintext exactly once

`asks.nonce_sha256`, never the nonce. `raise()` hands the plaintext to its caller once; the DB only
ever holds the hash, so a DB read cannot recover a live token. Slice 2 matches an inbound nonce by
hashing it. Same shape as `pairing_codes.code_sha256` in `0018_pairings.sql`.

This is the unforgeability constraint from the issue, which openworker's borrowed design does not
have: it embeds a plain item id and is safe only because its transport is a single-user desktop
app. On a Matrix room, any peer who can send could guess an id and resolve someone else's approval.

### D4 — Resume gets its own NOTIFY channel

`tasks_inserted` fires `AFTER INSERT` only, so an `awaiting_operator → pending` UPDATE wakes
nobody and the resumed task waits up to the 30 s `HEARTBEAT`. A new `notify_task_resumed` trigger
fires a **new `tasks_resumed` channel**, and `lane_loop` listens on it.

Overloading `tasks_inserted` to also mean "or resumed" was rejected: a channel name that no longer
describes what it carries is the same trap as the renamed log line in the #516 arc, which silently
broke `upgrade_from_git.sh`'s own post-deploy check. Four lines in `lane_loop` is the cheaper price.

### D5 — Two-table writes are one transaction

`resolve`, `expire_due` and the widened `mark_cancelled` (D8) each write both `asks` and `tasks`.
A resolved ask whose task never resumed is a wedged task; an expired ask whose task stayed
suspended is the same bug; a cancelled task whose ask stayed `pending` would be resolvable after
its task is dead, and on resolution would try to re-enqueue a cancelled task. All three run in one
transaction.

### D6 — Expiry lands in this slice, not slice 2

The issue puts deadline enforcement in slice 2. It moves here because `deadline_at` is already a
slice-1a column and the sweep is pure PG with no channel involvement, so it sits inside this
slice's "testable against live PG alone" boundary. Shipping suspension *without* it would add a way
to wedge a task permanently — the headless-daemon-cannot-wait-forever failure the issue itself
names. On expiry the task fails closed with an `ask_timeout` outcome.

### D7 — `finalize`'s contract is not widened

Expiry needs a terminal write from `awaiting_operator`, and `tasks::finalize` matches
`WHERE state = 'running'`. Rather than widen that guard, expiry gets its own
`fail_awaiting_operator` with its own state guard. `finalize` means "the lane runner finished a
task it was running", and keeping that true is worth one small function.

### D8 — `mark_cancelled` must be widened, and this is not optional

`mark_cancelled` matches `state IN ('pending','running')`. Without a change, `awaiting_operator`
would be a state from which a task **cannot be cancelled** — a wedge introduced by this very slice.
It gains `'awaiting_operator'`, and the ask is marked `cancelled` with it.

The ask-cancelling write goes **inside** `mark_cancelled` rather than into a separate
cancel-both helper, which couples `db::tasks` to `db::asks`. That coupling is deliberate: with a
separate helper, any caller reaching for plain `mark_cancelled` — and the CLI cancel path is one —
leaves a `pending` ask attached to a dead task, which is resolvable afterwards and on resolution
tries to re-enqueue a cancelled task. One cancel path that cannot be bypassed is worth the import,
the same argument `AllowlistDecl` made in #545 for making the half-declared state unrepresentable.

## Schema

```sql
CREATE TABLE asks (
    id            BIGSERIAL   PRIMARY KEY,
    task_id       BIGINT      NOT NULL REFERENCES tasks(id),
    kind          TEXT        NOT NULL,          -- 'plan_approval' today
    body          TEXT        NOT NULL,          -- what the operator is shown
    options       JSONB       NOT NULL,          -- the closed resolution set
    plan_digest   TEXT,                          -- NULL for kinds binding to no plan
    nonce_sha256  TEXT        NOT NULL,
    state         TEXT        NOT NULL DEFAULT 'pending'
                  CHECK (state IN ('pending','resolved','expired','cancelled')),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    deadline_at   TIMESTAMPTZ NOT NULL,
    resolved_at   TIMESTAMPTZ,
    resolved_by   TEXT,                          -- "<channel>/<peer>" or "operator/cli"
    resolution    JSONB                          -- {choice, free_text?}
);
CREATE INDEX asks_pending_deadline ON asks (deadline_at) WHERE state = 'pending';
CREATE INDEX asks_task             ON asks (task_id);

GRANT  SELECT, INSERT, UPDATE ON asks TO kastellan_runtime;
REVOKE DELETE, TRUNCATE       ON asks FROM kastellan_runtime;
```

On `tasks`: `awaiting_operator` appended to `tasks_state_check`, and the `notify_task_resumed`
trigger. `awaiting_operator` is **not** terminal, so `notify_task_completed`'s two `IN` lists are
correctly left alone — and because `OLD.state NOT IN (…terminal…)` holds for it, the
`awaiting_operator → failed` expiry transition still fires `tasks_completed` as it should.

`resolution` is a closed set — `{choice}` indexing into `options`, plus optional `free_text` stored
for the audit row and shown to the operator. Free text is **never** interpolated into a plan;
otherwise the ask channel becomes an injection funnel aimed at the reviewer's own decision.

## The state machine

| fn | guard | returns |
| --- | --- | --- |
| `raise` | tx: `INSERT asks` + `UPDATE tasks SET state='awaiting_operator', lease_expires_at=NULL WHERE id=$1 AND state='running'` | `(ask_id, plaintext_nonce)`; errors if the task was not `running` |
| `resolve` | `UPDATE asks … WHERE id=$1 AND state='pending'` + `UPDATE tasks … 'pending'`, one tx | `bool` from rows-affected — first-responder-wins |
| `expire_due` | `UPDATE asks SET state='expired' WHERE state='pending' AND deadline_at < now()` + fail each owning task | `Vec<ExpiredAsk>` for the caller's audit rows |
| `cancel_for_task` | `UPDATE asks SET state='cancelled' WHERE task_id=$1 AND state='pending'` | count; called inside `tasks::mark_cancelled`'s transaction (D8) |
| `get` / `list_pending` | — | reads |

`resolve` uses the guarded `UPDATE … WHERE state='pending'` returning rows-affected — the same
race-safe idiom `memories::set_embedding` uses. That is what buys resolved-exactly-once,
first-responder-wins across surfaces with no lock.

**Two existing queries need no change, and that is worth stating so nobody "fixes" them:**
`tasks::claim_one` filters `state='pending'` and `tasks::sweep_crashed` filters `state='running'`,
so both already exclude `awaiting_operator`. The lease is nulled at suspend anyway, because a
suspended task holding a lease is a lie to `any_live_worker`.

## Testing

TDD — tests first, in this order.

**Unit — `plan_digest`:**
- stable across `context` / `rationale` / `returns` / `done_when` edits
- sensitive to every executable field: `tool`, `method`, `parameters`, `classification`,
  `data_ceiling`
- canonical: two `Plan`s differing only in JSON key insertion order digest identically
- step *order* is significant (reordering steps changes the digest)

**PG e2e — `db/tests/asks_e2e.rs`:**
- `raise` suspends the task and nulls the lease
- **double `resolve` returns `true` then `false`** — the exactly-once property
- `resolve` re-enqueues the task to `pending` and fires `tasks_resumed`
- `expire_due` fails the task closed and does not touch a resolved ask
- `claim_one` never returns an `awaiting_operator` task
- `sweep_crashed` never reaps one
- `mark_cancelled` works from `awaiting_operator`, and cancels the ask with it
- `raise` against a non-`running` task errors rather than orphaning an ask

**Mutations to run:** drop `AND state='pending'` from `resolve` (double-resolve must fail); drop
`lease_expires_at = NULL` from `raise`; drop the `awaiting_operator` arm from `mark_cancelled`;
make `expire_due` non-transactional.

`sqlx::migrate!` embeds at compile time — `touch db/src/lib.rs` after adding the migration, or it
silently does not apply.

## What this slice deliberately excludes

- `Outcome::AwaitingOperator` and `drain_lane`'s non-finalize branch
- the `Verdict::Escalate` wiring (the arm keeps degrading to `Block`; the TODO stays)
- channel delivery and inbound nonce correlation
- `ask.raised` / `ask.resolved` audit rows
- an `ask_user` planner tool, `propose_plan`-style plan approval, the expired-ask dead-letter
  surface

The first two are excluded **together and for one reason**: `drain_lane`'s suspend branch is
reachable only from the `Escalate` arm, so shipping the plumbing without its producer means
shipping a branch no test can reach. That is the "a check that cannot fail" family this repo has
been bitten by repeatedly (#539's `Err`-arm assertions, #542's shared-prefix probe, the four
fail-open defects in the Shieldstral harness). Slice 1b takes both at once, which is the smallest
unit that is testable end-to-end.

## Slice 1b, for the next session

`Outcome::AwaitingOperator` (making `final_state()` return `Option<&'static str>` so every call
site must handle the non-terminal case, rather than inventing a pseudo-terminal string),
`drain_lane`'s non-finalize branch, the `Escalate` arm raising an ask via a
`scheduler::asks::raise_and_suspend` helper, the expiry sweep called at daemon startup alongside
`crash_recovery::sweep_and_audit`, and the `ask.raised` / `ask.resolved` / `ask.expired` audit
rows. Testable end-to-end with a scripted review stage returning `Escalate`.
