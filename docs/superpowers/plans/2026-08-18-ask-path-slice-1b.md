# Ask path (#564 slice 1b) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `Verdict::Escalate` suspend its task on a durable operator question, let an operator answer it from the CLI, and resume or fail the task accordingly — replacing the silent degrade-to-`Block`.

**Architecture:** Slice 1a shipped `db::asks` with no production caller. This slice adds the caller. `run_one` reads the task's latest resolved ask once before planning; a `deny` terminates the task, an `approve` rides into `TaskContext`. The `Escalate` arm compares the live plan's digest against that approval and either proceeds or raises a new ask and returns the new non-terminal `Outcome::AwaitingOperator`. `drain_lane` learns to not finalize. A periodic sweep expires overdue asks. `kastellan-cli inbox` is the answer surface.

**Tech Stack:** Rust 2021, `sqlx` (Postgres), `tokio`, `time::OffsetDateTime`, `serde_json`. No new dependencies.

**Spec:** [`docs/superpowers/specs/2026-08-18-ask-path-slice-1b-design.md`](../specs/2026-08-18-ask-path-slice-1b-design.md)

## Global Constraints

- **Branch:** `feat/564-slice-1b-ask-path`, off `main` @ `e8ea4339`. Already created; the spec is committed on it.
- **Source the cargo env first** in every shell: `source "$HOME/.cargo/env"`. Cargo is not on the non-interactive `PATH`.
- **Run every cargo command in the FOREGROUND.** Never background a `cargo test`/`clippy` and poll it.
- **Clippy is enforced:** `cargo clippy --workspace --all-targets -- -D warnings` must stay at zero. Note local rust is 1.96.0 and CI is 1.97.0, so a clean local run is not CI parity — expect a possible one-line lint follow-up after pushing.
- **TDD, strictly:** the failing test is written and *run* before the implementation, every task.
- **AGPL-3.0 project, AGPL-compatible deps only.** This slice adds no dependency.
- **Cross-platform.** Everything here is `cfg`-free. Do not add a `cfg(target_os = …)` anywhere in this slice.
- **500-LOC guidance.** `core/src/scheduler/inner_loop.rs` is already 625 lines. Its net growth in this plan must stay under ~20 lines; the logic lives in the new `core/src/scheduler/asks.rs`.
- **Audit inserts are best-effort** — log at `warn!` and swallow, matching `write_lifecycle_row` and `crash_recovery`. Never roll back a committed state transition because a log row failed.
- **`git add <specific files>`, never `git add -A`.**
- **PG e2e tests skip silently** without a local Postgres. Run them with `KASTELLAN_PG_BIN_DIR` set (see the repo's `pg_bin_dir_or_skip`), and confirm with `-- --nocapture` that they *ran* rather than printed `[SKIP]`.

---

### Task 1: `db::asks::latest_resolved_for_task`

The one new DB read. Serves both the deny check (Task 5) and the approval comparison (Task 6).

**Files:**
- Modify: `db/src/asks.rs` (add after `list_pending`, before `get`)
- Test: `db/tests/asks_e2e.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `pub async fn latest_resolved_for_task(pool: &PgPool, task_id: i64) -> Result<Option<Ask>, DbError>`

- [ ] **Step 1: Write the failing tests**

Append to `db/tests/asks_e2e.rs`. Follow the existing file's bring-up helper (copy the exact `bring_up_pg`/seed pattern the neighbouring tests use — do not invent a new one).

```rust
#[tokio::test]
async fn latest_resolved_for_task_returns_none_when_nothing_is_resolved() {
    let Some((pool, _cluster)) = bring_up_pg("asks-latest-none").await else { return };
    let task_id = seed_running_task(&pool).await;

    // A pending ask is not a resolved ask.
    let _ = asks::raise(
        &pool, task_id, "plan_approval", "why", &serde_json::json!(["approve", "deny"]),
        Some("digest-a"), OffsetDateTime::now_utc() + Duration::hours(1),
    ).await.expect("raise");

    let got = asks::latest_resolved_for_task(&pool, task_id).await.expect("read");
    assert!(got.is_none(), "a pending ask must not be returned as resolved");
}

#[tokio::test]
async fn latest_resolved_for_task_returns_the_most_recent_resolution() {
    let Some((pool, _cluster)) = bring_up_pg("asks-latest-recent").await else { return };
    let task_id = seed_running_task(&pool).await;

    // First ask: raised, approved. Resolving it returns the task to `pending`.
    let first = asks::raise(
        &pool, task_id, "plan_approval", "first concern",
        &serde_json::json!(["approve", "deny"]), Some("digest-a"),
        OffsetDateTime::now_utc() + Duration::hours(1),
    ).await.expect("raise 1");
    assert!(asks::resolve(&pool, first.ask_id, "operator",
        &serde_json::json!({"choice": "approve"})).await.expect("resolve 1"));

    // Second ask on the same task, denied. `raise` needs `running`, so
    // re-claim the task the resolve just re-enqueued.
    tasks::claim_one(&pool, Lane::Fast, 60).await.expect("claim").expect("a task");
    let second = asks::raise(
        &pool, task_id, "plan_approval", "second concern",
        &serde_json::json!(["approve", "deny"]), Some("digest-b"),
        OffsetDateTime::now_utc() + Duration::hours(1),
    ).await.expect("raise 2");
    assert!(asks::resolve(&pool, second.ask_id, "operator",
        &serde_json::json!({"choice": "deny"})).await.expect("resolve 2"));

    let got = asks::latest_resolved_for_task(&pool, task_id).await.expect("read")
        .expect("a resolved ask");
    assert_eq!(got.id, second.ask_id, "the LATEST resolution must win, not the first");
    assert_eq!(got.plan_digest.as_deref(), Some("digest-b"));
}

#[tokio::test]
async fn latest_resolved_for_task_ignores_expired_and_cancelled_asks() {
    let Some((pool, _cluster)) = bring_up_pg("asks-latest-states").await else { return };
    let task_id = seed_running_task(&pool).await;

    // Deadline one second out, then swept: the ask ends `expired`, not `resolved`.
    let _ = asks::raise(
        &pool, task_id, "plan_approval", "why", &serde_json::json!(["approve", "deny"]),
        Some("digest-a"), OffsetDateTime::now_utc() + Duration::seconds(1),
    ).await.expect("raise");
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let expired = asks::expire_due(&pool).await.expect("expire");
    assert_eq!(expired.len(), 1, "the ask must have been swept");

    let got = asks::latest_resolved_for_task(&pool, task_id).await.expect("read");
    assert!(got.is_none(), "an expired ask is not a resolution and must not be returned");
}
```

If `db/tests/asks_e2e.rs` has no `seed_running_task` helper, add one beside the existing helpers: insert a pending task via `tasks::insert_pending` and claim it with `tasks::claim_one(&pool, Lane::Fast, 60)` so it is `running` (which `asks::raise` requires).

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-db --test asks_e2e latest_resolved_for_task -- --nocapture
```

Expected: FAIL to compile — `no function or associated item named 'latest_resolved_for_task'`.

- [ ] **Step 3: Implement**

Add to `db/src/asks.rs`, immediately after `list_pending`:

```rust
/// The task's most recently resolved ask, if it has one.
///
/// Slice 1b's single read: `run_one` calls it once per claimed task and
/// both consumers work from that one value — the pre-plan deny check and
/// the `Escalate` arm's digest comparison (spec D4).
///
/// **`state = 'resolved'` only.** An `expired` or `cancelled` ask is not a
/// decision anybody made, and returning one would let a timeout read as an
/// answer. A `pending` ask cannot be seen here either, and that is not
/// merely filtered: a task with a pending ask is `awaiting_operator`, which
/// `claim_one` never returns, so no caller of this function can be running
/// one.
///
/// Ordered `resolved_at DESC, id DESC`. `resolved_at` is `now()` at resolve
/// time, so two asks resolved inside one transaction tick can tie — the
/// same tiebreaker [`list_pending`] carries, for the same reason. A task
/// that escalates twice (P approved, then P′ denied) must see the second
/// decision, not the first.
pub async fn latest_resolved_for_task(
    pool: &PgPool,
    task_id: i64,
) -> Result<Option<Ask>, DbError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {ASK_COLUMNS} FROM asks \
         WHERE task_id = $1 AND state = 'resolved' \
         ORDER BY resolved_at DESC, id DESC \
         LIMIT 1"
    )))
    .bind(task_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| DbError::Query(format!("asks latest_resolved_for_task: {e}")))?;

    row.as_ref().map(decode_ask_row).transpose()
}
```

Check how `list_pending` wraps its query string — if it uses `sqlx::query(sqlx::AssertSqlSafe(format!(...)))`, match it exactly; if it uses a plain `&str` with the columns interpolated differently, match that instead. The point is one spelling of the column list, not a second copy.

- [ ] **Step 4: Run the tests to verify they pass**

```sh
cargo test -p kastellan-db --test asks_e2e latest_resolved_for_task -- --nocapture
```

Expected: 3 passed. If you see `[SKIP]`, Postgres is not configured — set `KASTELLAN_PG_BIN_DIR` and re-run. A skipped test is not a passing test.

- [ ] **Step 5: Commit**

```sh
git add db/src/asks.rs db/tests/asks_e2e.rs
git commit -m "feat(db): the task's latest resolved ask

Slice 1b reads this once per claimed task and both consumers work from
that one value. 'resolved' only: an expired ask is a timeout, not a
decision, and returning one would let a timeout read as an answer.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: `Outcome::AwaitingOperator` + `Outcome::Denied`, and a fallible `final_state()`

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs:120-161` (the `Outcome` enum and its `impl`)
- Modify: `core/src/scheduler/runner.rs:323` (the `final_state` binding in `drain_lane`)
- Modify: `core/src/scheduler/inner_loop/tests.rs` (the `outcome_final_state_mapping` test)
- Modify: `core/tests/scheduler_inner_loop_e2e.rs:603,727,796` (three `final_state()` asserts)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `Outcome::AwaitingOperator { ask_id: i64 }`
  - `Outcome::Denied { ask_id: i64, reason: String }`
  - `Outcome::final_state(&self) -> Option<&'static str>`

- [ ] **Step 1: Write the failing tests**

In `core/src/scheduler/inner_loop/tests.rs`, replace the body of `outcome_final_state_mapping` and add a companion. Keep every existing assertion, wrapped in `Some(...)`:

```rust
#[test]
fn outcome_final_state_mapping() {
    assert_eq!(Outcome::Completed(serde_json::json!("x")).final_state(), Some("completed"));
    assert_eq!(Outcome::Failed("e".into()).final_state(), Some("failed"));
    assert_eq!(Outcome::Cancelled.final_state(), Some("cancelled"));
    assert_eq!(Outcome::TimedOut.final_state(), Some("timed_out"));
    assert_eq!(
        Outcome::Blocked { principle: 1, reason: "r".into() }.final_state(),
        Some("blocked")
    );
    // Keep the existing Refused assertion here, wrapped in Some(...).
}

#[test]
fn awaiting_operator_is_not_a_terminal_state() {
    // The whole reason `final_state` is an Option: a suspended task has
    // not finished, and `tasks::finalize` matches `WHERE state='running'`
    // so any string here would be a silent no-op UPDATE plus two audit
    // rows asserting an end that did not happen.
    let o = Outcome::AwaitingOperator { ask_id: 7 };
    assert_eq!(o.final_state(), None);
    assert_eq!(o.result_payload(), None);
}

#[test]
fn denied_is_terminal_as_blocked_and_says_so_in_the_payload() {
    let o = Outcome::Denied { ask_id: 7, reason: "sends mail to a stranger".into() };
    // `blocked` because the operator is the review authority of last
    // resort — and no fabricated `principle`, which is what reusing
    // Outcome::Blocked would have required.
    assert_eq!(o.final_state(), Some("blocked"));
    let p = o.result_payload().expect("a denied task has a payload");
    assert_eq!(p.get("kind").and_then(|v| v.as_str()), Some("denied"));
    assert_eq!(p.get("ask_id").and_then(|v| v.as_i64()), Some(7));
    assert_eq!(
        p.get("reason").and_then(|v| v.as_str()),
        Some("sends mail to a stranger")
    );
    assert!(p.get("principle").is_none(), "a denial violates no CASSANDRA principle");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib inner_loop::tests::outcome -- --nocapture
cargo test -p kastellan-core --lib inner_loop::tests::awaiting -- --nocapture
cargo test -p kastellan-core --lib inner_loop::tests::denied -- --nocapture
```

Expected: FAIL to compile — no variant `AwaitingOperator`, and `final_state()` returns `&'static str` not `Option`.

- [ ] **Step 3: Implement**

In `core/src/scheduler/inner_loop.rs`, add the two variants to `Outcome`:

```rust
    /// **Non-terminal.** The reviewer escalated and an operator ask was
    /// raised; `db::asks::raise` has already moved the task to
    /// `awaiting_operator` inside its own transaction, so the lane runner
    /// must NOT finalize. Resolution re-enqueues the task through the
    /// `tasks_resumed` NOTIFY and it runs again from the top.
    AwaitingOperator { ask_id: i64 },
    /// An operator answered a raised ask with `deny`. Terminal.
    ///
    /// `reason` is the ask's own `body` — the reviewer's escalation
    /// concern, i.e. the question the operator was answering. The
    /// operator's optional free-text note deliberately does NOT travel
    /// here; it lives in `asks.resolution` and the `ask.resolved` audit
    /// row (spec D10).
    Denied { ask_id: i64, reason: String },
```

Then the `impl`:

```rust
    /// The `tasks.state` this outcome finalizes to, or `None` when the
    /// task has not finished.
    ///
    /// **An `Option` rather than a pseudo-terminal string, and that is
    /// load-bearing.** `AwaitingOperator` is the first non-terminal
    /// outcome the loop can return. Answering `"awaiting_operator"` here
    /// would send it to `tasks::finalize`, which matches
    /// `WHERE state = 'running'` — the row is already `awaiting_operator`,
    /// so the UPDATE silently no-ops — and would then write a terminal
    /// lifecycle row and a `task.finalize` row asserting an end that did
    /// not happen. `Option` makes every call site say what it does with an
    /// unfinished task, and the compiler finds them all.
    pub fn final_state(&self) -> Option<&'static str> {
        match self {
            Outcome::Completed(_) => Some("completed"),
            Outcome::Failed(_)    => Some("failed"),
            Outcome::Cancelled    => Some("cancelled"),
            Outcome::TimedOut     => Some("timed_out"),
            Outcome::Blocked { .. } => Some("blocked"),
            Outcome::Refused { .. } => Some("refused"),
            // `blocked` is shared with the reviewer-detected path on
            // purpose: a third terminal state would need a migration to
            // widen `tasks_state_check` and would partition every existing
            // observation query grouping on terminal states. The
            // `ask.resolved` audit row's `choice` is what separates the two
            // populations.
            Outcome::Denied { .. } => Some("blocked"),
            Outcome::AwaitingOperator { .. } => None,
        }
    }
```

And in `result_payload`, add the `Denied` arm above the trailing `_ => None` (which keeps covering `AwaitingOperator`):

```rust
            Outcome::Denied { ask_id, reason } => Some(serde_json::json!({
                "kind": "denied",
                "ask_id": ask_id,
                "reason": reason,
            })),
```

In `core/src/scheduler/runner.rs`, replace line 323's binding. **This branch is unreachable until Task 6 supplies its producer — that is expected, and Task 6's e2e is what covers it. Do not add a test for it here; a test that cannot fail is worse than none.**

```rust
        // A non-terminal outcome: the task suspended on an operator ask.
        // `db::asks::raise` already moved the row to `awaiting_operator`
        // inside its own transaction and the `ask.raised` audit row is
        // written by `scheduler::asks::raise_and_suspend`, so there is
        // nothing to finalize and no terminal lifecycle row to write. The
        // L1/L3 hooks below are `Outcome::Completed`-only anyway.
        let Some(final_state) = result.outcome.final_state() else {
            // The per-task out dir is left in place: the task resumes and
            // `create_dir_all` is idempotent, so re-creating it costs
            // nothing and removing it could delete a deliverable a step
            // already wrote before the escalation.
            continue;
        };
```

Then fix the three `final_state()` asserts in `core/tests/scheduler_inner_loop_e2e.rs` by wrapping the expected values in `Some(...)`.

- [ ] **Step 4: Run the tests to verify they pass**

```sh
cargo test -p kastellan-core --lib inner_loop::tests -- --nocapture
cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: all `inner_loop::tests` pass; clippy exit 0. `cargo build --workspace` must also succeed — the `Outcome` change is a wire-type change and its blast radius is not bounded by its module.

- [ ] **Step 5: Commit**

```sh
git add core/src/scheduler/inner_loop.rs core/src/scheduler/inner_loop/tests.rs \
        core/src/scheduler/runner.rs core/tests/scheduler_inner_loop_e2e.rs
git commit -m "feat(scheduler): a non-terminal outcome, and final_state() says so

AwaitingOperator is the first outcome that is not an ending. A
pseudo-terminal string would have gone to tasks::finalize, whose guard is
WHERE state='running' — the row is already awaiting_operator, so the
UPDATE no-ops silently while two audit rows assert an end that did not
happen. Option makes every call site handle it and the compiler finds
them.

Denied finalizes as 'blocked' rather than earning a third terminal state:
a new state needs a migration and partitions every observation query
grouping on terminal states. It does NOT reuse Outcome::Blocked, whose
payload carries a CASSANDRA principle number — a denial violates no
principle and inventing one fabricates a record.

drain_lane's non-finalize branch is unreachable until the Escalate arm
lands; its test comes with its producer.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: `core/src/scheduler/asks.rs` — the pure half

**Files:**
- Create: `core/src/scheduler/asks.rs`
- Modify: `core/src/scheduler/mod.rs` (register the module + its one-line map entry)

**Interfaces:**
- Consumes: `kastellan_db::asks::Ask`.
- Produces:
  - `pub enum Choice { Approve, Deny }` (derives `Clone, Copy, Debug, Eq, PartialEq`)
  - `pub enum AskDecision { Approved, Denied, NotForThisPlan }` (same derives)
  - `pub const ASK_KIND_PLAN_APPROVAL: &str = "plan_approval"`
  - `pub const ASK_DEADLINE_ENV: &str = "KASTELLAN_ASK_DEADLINE_S"`
  - `pub const DEFAULT_ASK_DEADLINE_S: i64 = 86_400`
  - `pub fn resolution_choice(ask: &Ask) -> Option<Choice>`
  - `pub fn decide(ask: &Ask, plan_digest: &str) -> AskDecision`
  - `pub fn ask_deadline_seconds(raw: Option<&str>) -> i64`
  - `pub fn deadline_from_env() -> i64`

- [ ] **Step 1: Write the failing tests**

Create `core/src/scheduler/asks.rs` containing **only** a `#[cfg(test)] mod tests` block plus a test-local `Ask` builder, so the tests fail to compile against absent functions rather than passing vacuously:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_db::asks::Ask;
    use time::OffsetDateTime;

    /// A resolved `Ask` fixture. Every field is set explicitly so a new
    /// column added to `Ask` is a compile error here rather than a silent
    /// default the tests stop covering.
    fn ask(resolution: Option<serde_json::Value>, plan_digest: Option<&str>) -> Ask {
        Ask {
            id: 42,
            task_id: 7,
            kind: ASK_KIND_PLAN_APPROVAL.to_string(),
            body: "sends mail to a stranger".to_string(),
            options: serde_json::json!(["approve", "deny"]),
            plan_digest: plan_digest.map(String::from),
            state: "resolved".to_string(),
            created_at: OffsetDateTime::now_utc(),
            deadline_at: OffsetDateTime::now_utc(),
            resolved_at: Some(OffsetDateTime::now_utc()),
            resolved_by: Some("operator".to_string()),
            resolution,
        }
    }

    #[test]
    fn resolution_choice_reads_the_two_offered_answers() {
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!({"choice": "approve"})), Some("d"))),
            Some(Choice::Approve)
        );
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!({"choice": "deny"})), Some("d"))),
            Some(Choice::Deny)
        );
    }

    #[test]
    fn resolution_choice_is_none_for_every_malformed_shape() {
        // `asks::resolve` enforces the closed set on the way in, so these
        // arms are defence in depth against a direct-SQL writer or a future
        // resolver. They must answer None — never Approve — because None is
        // the arm that leads to raising a fresh ask.
        assert_eq!(resolution_choice(&ask(None, Some("d"))), None, "no resolution");
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!("approve")), Some("d"))),
            None,
            "resolution is not an object"
        );
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!({"note": "ok"})), Some("d"))),
            None,
            "no choice key"
        );
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!({"choice": 1})), Some("d"))),
            None,
            "choice is not a string"
        );
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!({"choice": "maybe"})), Some("d"))),
            None,
            "choice is not one this ask offered"
        );
    }

    #[test]
    fn a_choice_outside_this_asks_own_options_is_refused() {
        // The closed set is per-ask, not a global {approve,deny}: a future
        // kind offers different options, and validating against a constant
        // would silently accept the wrong vocabulary.
        let mut a = ask(Some(serde_json::json!({"choice": "approve"})), Some("d"));
        a.options = serde_json::json!(["yes", "no"]);
        assert_eq!(resolution_choice(&a), None);
    }

    #[test]
    fn decide_approves_only_the_digest_that_was_approved() {
        let a = ask(Some(serde_json::json!({"choice": "approve"})), Some("digest-a"));
        assert_eq!(decide(&a, "digest-a"), AskDecision::Approved);
        assert_eq!(decide(&a, "digest-b"), AskDecision::NotForThisPlan);
    }

    #[test]
    fn decide_denies_regardless_of_digest() {
        let a = ask(Some(serde_json::json!({"choice": "deny"})), Some("digest-a"));
        assert_eq!(decide(&a, "digest-a"), AskDecision::Denied);
        assert_eq!(decide(&a, "digest-b"), AskDecision::Denied);
    }

    #[test]
    fn an_approval_bound_to_no_digest_approves_nothing() {
        // plan_digest is nullable because a future kind need not bind to a
        // plan. Such an approval must never cover a plan by default.
        let a = ask(Some(serde_json::json!({"choice": "approve"})), None);
        assert_eq!(decide(&a, "digest-a"), AskDecision::NotForThisPlan);
    }

    #[test]
    fn a_malformed_resolution_never_reaches_the_approved_arm() {
        let a = ask(Some(serde_json::json!({"choice": "maybe"})), Some("digest-a"));
        assert_eq!(decide(&a, "digest-a"), AskDecision::NotForThisPlan);
    }

    #[test]
    fn ask_deadline_seconds_defaults_and_rejects_unusable_values() {
        assert_eq!(ask_deadline_seconds(None), DEFAULT_ASK_DEADLINE_S);
        assert_eq!(ask_deadline_seconds(Some("3600")), 3600);
        assert_eq!(ask_deadline_seconds(Some("  3600  ")), 3600);
        // A non-positive deadline would be in the past by the time `raise`
        // runs and the asks_deadline_after_created CHECK would reject the
        // INSERT, failing the escalation instead of asking the human.
        assert_eq!(ask_deadline_seconds(Some("0")), DEFAULT_ASK_DEADLINE_S);
        assert_eq!(ask_deadline_seconds(Some("-1")), DEFAULT_ASK_DEADLINE_S);
        assert_eq!(ask_deadline_seconds(Some("later")), DEFAULT_ASK_DEADLINE_S);
        assert_eq!(ask_deadline_seconds(Some("")), DEFAULT_ASK_DEADLINE_S);
    }
}
```

Register the module in `core/src/scheduler/mod.rs`: add `pub mod asks;` in the alphabetical list (before `pub mod audit;`) and one line to the module map comment at the top:

```
//!   - `asks`              — the operator-ask path: pure resolution reading + raise/expire wiring (#564 slice 1b)
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib scheduler::asks -- --nocapture
```

Expected: FAIL to compile — `cannot find function 'resolution_choice'`, `cannot find type 'Choice'`, etc.

- [ ] **Step 3: Implement**

Write the production half at the top of `core/src/scheduler/asks.rs`, above the test module:

```rust
//! The operator-ask path — #564 slice 1b.
//!
//! Slice 1a built the durable record (`db::asks`) and nothing called it.
//! This module is the caller: it turns a `Verdict::Escalate` into a raised
//! ask, reads a resolved ask back into a decision, and sweeps overdue asks.
//!
//! The reading half is **pure** and lives here rather than inside the inner
//! loop, because the `Escalate` arm is otherwise reachable only through a
//! Postgres e2e — and a rule with no unit test is a rule nobody can
//! exercise cheaply.
//!
//! Spec: `docs/superpowers/specs/2026-08-18-ask-path-slice-1b-design.md`.

use kastellan_db::asks::Ask;

/// The answers a `plan_approval` ask offers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Choice {
    Approve,
    Deny,
}

/// What a resolved ask means for one specific plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AskDecision {
    /// The operator approved, and this is the plan they approved.
    Approved,
    /// The operator denied. Terminal for the task, whatever the plan.
    Denied,
    /// There is no usable decision for *this* plan — a different plan was
    /// approved, the ask binds to no plan, or the resolution is malformed.
    /// The safe arm: it leads to raising a fresh ask, never to proceeding.
    NotForThisPlan,
}

/// `asks.kind` for an escalated-plan approval. The column's CHECK accepts
/// only this value today; a new kind is a deliberate migration.
pub const ASK_KIND_PLAN_APPROVAL: &str = "plan_approval";

/// Operator override for how long an ask stays answerable.
pub const ASK_DEADLINE_ENV: &str = "KASTELLAN_ASK_DEADLINE_S";

/// 24 hours. Long enough that an escalation raised overnight is still
/// answerable in the morning, short enough that an abandoned task does not
/// sit suspended for a week.
pub const DEFAULT_ASK_DEADLINE_S: i64 = 86_400;

/// The ask's resolution as a validated [`Choice`], or `None` if there is
/// no usable one.
///
/// **Validated against the ask's own `options`, not against a constant.**
/// The closed set is per-ask: a future kind offers different options, and
/// checking against a hardcoded `{approve, deny}` would silently accept the
/// wrong vocabulary for it.
///
/// `db::asks::resolve` already enforces the closed set on the way in, so
/// every arm returning `None` here is defence in depth — against a
/// direct-SQL writer, or a resolver added later that forgets. They must
/// return `None` and never `Approve`: `None` leads to raising a fresh ask,
/// which costs one operator interaction, while a wrong `Approve` lets a
/// plan run that nobody approved.
pub fn resolution_choice(ask: &Ask) -> Option<Choice> {
    let choice = ask.resolution.as_ref()?.get("choice")?.as_str()?;
    let offered = ask.options.as_array()?;
    if !offered.iter().any(|o| o.as_str() == Some(choice)) {
        return None;
    }
    match choice {
        "approve" => Some(Choice::Approve),
        "deny" => Some(Choice::Deny),
        _ => None,
    }
}

/// What `ask` decides about the plan whose digest is `plan_digest`.
///
/// An approval binds to a plan digest (slice 1a, spec D1): the resumed task
/// replans from scratch and goes through review again, and the approval
/// carries only if the new plan is the one that was approved. A denial
/// binds to the task, not the plan — see spec D2 for why the asymmetry is
/// deliberate.
///
/// An approval whose ask carries no `plan_digest` approves nothing. The
/// column is nullable for kinds that do not bind to a plan, and defaulting
/// such an approval to "covers this plan" is the fail-open direction.
pub fn decide(ask: &Ask, plan_digest: &str) -> AskDecision {
    match resolution_choice(ask) {
        Some(Choice::Deny) => AskDecision::Denied,
        Some(Choice::Approve) if ask.plan_digest.as_deref() == Some(plan_digest) => {
            AskDecision::Approved
        }
        _ => AskDecision::NotForThisPlan,
    }
}

/// Parse an ask deadline in seconds, falling back to
/// [`DEFAULT_ASK_DEADLINE_S`] for anything unusable.
///
/// Pure, taking the raw string, so the fallback rules have a test that does
/// not mutate process environment (which is global and races Rust's
/// parallel test threads — the `KASTELLAN_WORKER_OUT` flake).
///
/// Non-positive values fall back rather than being honoured: a zero or
/// negative deadline is already in the past when `raise` runs, so the
/// `asks_deadline_after_created` CHECK rejects the INSERT and the
/// escalation fails instead of asking the human — an operator typo turning
/// into a task failure.
pub fn ask_deadline_seconds(raw: Option<&str>) -> i64 {
    raw.and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_ASK_DEADLINE_S)
}

/// [`ask_deadline_seconds`] against the live environment.
pub fn deadline_from_env() -> i64 {
    ask_deadline_seconds(std::env::var(ASK_DEADLINE_ENV).ok().as_deref())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```sh
cargo test -p kastellan-core --lib scheduler::asks -- --nocapture
cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: 8 passed, clippy exit 0.

- [ ] **Step 5: Commit**

```sh
git add core/src/scheduler/asks.rs core/src/scheduler/mod.rs
git commit -m "feat(scheduler): the pure half of the ask path

resolution_choice validates against the ask's OWN options rather than a
constant {approve,deny}: the closed set is per-ask, and a future kind
offering different options would otherwise be checked against the wrong
vocabulary. Every malformed shape answers None, which leads to raising a
fresh ask — one operator interaction — where a wrong Approve would run a
plan nobody approved.

An approval whose ask carries no plan_digest approves nothing. The column
is nullable for kinds that do not bind to a plan, and treating that as
'covers this plan' is the fail-open direction.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: `raise_and_suspend`, `sweep_expired_and_audit`, and the three audit actions

**Files:**
- Modify: `core/src/scheduler/audit.rs` (three new `ACTION_*` constants)
- Modify: `core/src/scheduler/asks.rs` (the async half, above the test module)
- Test: `core/tests/scheduler_asks_e2e.rs` (create)

**Interfaces:**
- Consumes: Task 3's `ASK_KIND_PLAN_APPROVAL`, `deadline_from_env`; `kastellan_db::asks::{raise, expire_due}`; `crate::cassandra::plan_digest::plan_digest`; `crate::cassandra::types::{Plan, Severity}`.
- Produces:
  - `pub const ACTION_ASK_RAISED: &str = "ask.raised"` (in `audit.rs`)
  - `pub const ACTION_ASK_RESOLVED: &str = "ask.resolved"` (in `audit.rs`)
  - `pub const ACTION_ASK_EXPIRED: &str = "ask.expired"` (in `audit.rs`)
  - `pub async fn raise_and_suspend(pool: &PgPool, task_id: i64, plan: &Plan, concern: &str, severity: Severity) -> Result<i64, DbError>`
  - `pub async fn sweep_expired_and_audit(pool: &PgPool) -> Result<usize, DbError>`

- [ ] **Step 1: Write the failing tests**

Create `core/tests/scheduler_asks_e2e.rs`. Copy the `bring_up_pg` helper verbatim from `core/tests/scheduler_inner_loop_e2e.rs` (lines ~37-68), changing only the service-name prefix to `kastellan-sched-test-pg-asks-` and the label strings.

```rust
//! PG e2e for `scheduler::asks` — the raise/expire wiring and its audit rows.
//!
//! Skips silently with `[SKIP]` on hosts without Postgres; run with
//! `-- --nocapture` to see whether it ran.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use kastellan_core::cassandra::types::{DataClass, Plan, Severity};
use kastellan_core::scheduler::asks;
use kastellan_core::scheduler::audit::{ACTION_ASK_EXPIRED, ACTION_ASK_RAISED};
use kastellan_db::tasks::{self, insert_pending, Lane};

// <copy bring_up_pg here>

/// A minimal terminal plan varying only in `context` — one of the four
/// fields `plan_digest` deliberately EXCLUDES. Mirrors
/// `task_complete_plan` in `core/tests/scheduler_lanes_e2e.rs:147`.
fn plan_with_context(context: &str) -> Plan {
    Plan {
        context: context.into(),
        decision: "task_complete".into(),
        rationale: "done".into(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": "ok"})),
        data_ceiling: Some(DataClass::Public),
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
        python_skill: None,
    }
}

async fn seed_running_task(pool: &sqlx::PgPool) -> i64 {
    let id = insert_pending(pool, Lane::Fast, serde_json::json!({"instruction": "x", "max_plans": 3}))
        .await
        .expect("insert");
    tasks::claim_one(pool, Lane::Fast, 60).await.expect("claim").expect("a task");
    id
}

async fn audit_actions_for(pool: &sqlx::PgPool, task_id: i64) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT action FROM audit_log \
         WHERE payload->>'task_id' = $1::text ORDER BY id ASC",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
    .expect("audit read")
}

#[tokio::test]
async fn raise_and_suspend_suspends_the_task_and_audits_it() {
    let Some((pool, _cluster)) = bring_up_pg("raise").await else { return };
    let task_id = seed_running_task(&pool).await;

    let ask_id = asks::raise_and_suspend(
        &pool, task_id, &plan_with_context("send the mail"),
        "this sends mail to a stranger", Severity::High,
    ).await.expect("raise_and_suspend");

    assert_eq!(
        tasks::observe_state(&pool, task_id).await.expect("state"),
        "awaiting_operator",
    );
    let ask = kastellan_db::asks::get(&pool, ask_id).await.expect("get").expect("an ask");
    assert_eq!(ask.state, "pending");
    assert_eq!(ask.kind, asks::ASK_KIND_PLAN_APPROVAL);
    assert_eq!(ask.body, "this sends mail to a stranger");
    assert_eq!(ask.options, serde_json::json!(["approve", "deny"]));
    assert!(ask.plan_digest.is_some(), "a plan_approval ask must bind to a digest");

    assert!(
        audit_actions_for(&pool, task_id).await.iter().any(|a| a == ACTION_ASK_RAISED),
        "an ask.raised row must be written",
    );
}

#[tokio::test]
async fn the_digest_recorded_is_the_digest_of_the_plan_passed_in() {
    let Some((pool, _cluster)) = bring_up_pg("digest").await else { return };
    let a = seed_running_task(&pool).await;
    let ask_a = asks::raise_and_suspend(
        &pool, a, &plan_with_context("plan one"), "c", Severity::Medium,
    ).await.expect("raise a");

    let b = seed_running_task(&pool).await;
    let ask_b = asks::raise_and_suspend(
        &pool, b, &plan_with_context("plan two"), "c", Severity::Medium,
    ).await.expect("raise b");

    let da = kastellan_db::asks::get(&pool, ask_a).await.unwrap().unwrap().plan_digest;
    let db_ = kastellan_db::asks::get(&pool, ask_b).await.unwrap().unwrap().plan_digest;
    // `context` is one of the four fields the digest EXCLUDES, so two plans
    // differing only in it must digest identically. This pins that
    // raise_and_suspend really calls plan_digest rather than hashing
    // something convenient.
    assert_eq!(da, db_, "context is excluded from the digest (slice 1a D2)");
}

#[tokio::test]
async fn raising_against_a_task_that_is_not_running_is_an_error() {
    let Some((pool, _cluster)) = bring_up_pg("notrunning").await else { return };
    let task_id = insert_pending(&pool, Lane::Fast, serde_json::json!({"instruction": "x", "max_plans": 3}))
        .await.expect("insert");
    // Never claimed, so still `pending`.
    let err = asks::raise_and_suspend(
        &pool, task_id, &plan_with_context("x"), "c", Severity::Low,
    ).await;
    assert!(err.is_err(), "raising against a non-running task must fail, not orphan an ask");
    assert_eq!(tasks::observe_state(&pool, task_id).await.expect("state"), "pending");
}

#[tokio::test]
async fn sweep_expired_and_audit_fails_the_task_and_writes_one_row_each() {
    let Some((pool, _cluster)) = bring_up_pg("sweep").await else { return };
    let task_id = seed_running_task(&pool).await;

    // A one-second deadline, honoured through the documented env knob so
    // the test exercises the same path production does.
    std::env::set_var(asks::ASK_DEADLINE_ENV, "1");
    let ask_id = asks::raise_and_suspend(
        &pool, task_id, &plan_with_context("x"), "c", Severity::Low,
    ).await.expect("raise");
    std::env::remove_var(asks::ASK_DEADLINE_ENV);

    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let swept = asks::sweep_expired_and_audit(&pool).await.expect("sweep");
    assert_eq!(swept, 1);

    assert_eq!(tasks::observe_state(&pool, task_id).await.expect("state"), "failed");
    let t = tasks::get(&pool, task_id).await.expect("get").expect("a task");
    assert_eq!(
        t.result.as_ref().and_then(|r| r.get("detail")).and_then(|d| d.as_str()),
        Some(kastellan_db::asks::ASK_TIMEOUT_DETAIL),
    );
    assert_eq!(
        kastellan_db::asks::get(&pool, ask_id).await.unwrap().unwrap().state,
        "expired",
    );
    assert!(
        audit_actions_for(&pool, task_id).await.iter().any(|a| a == ACTION_ASK_EXPIRED),
        "an ask.expired row must be written",
    );

    // Idempotent: a second sweep finds nothing and writes nothing.
    assert_eq!(asks::sweep_expired_and_audit(&pool).await.expect("sweep 2"), 0);
    let expired_rows = audit_actions_for(&pool, task_id).await
        .iter().filter(|a| *a == ACTION_ASK_EXPIRED).count();
    assert_eq!(expired_rows, 1, "a second sweep must not duplicate the audit row");
}
```

`std::env::set_var` is process-global and Rust runs tests in parallel. If any other test in this file also sets `ASK_DEADLINE_ENV`, serialise them behind a `std::sync::Mutex` with a `Drop` guard, the way `workers/mail`'s `with_out_dir` does — a green single-test run is not evidence about a process-global.

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test scheduler_asks_e2e -- --nocapture
```

Expected: FAIL to compile — `ACTION_ASK_RAISED` and `asks::raise_and_suspend` do not exist.

- [ ] **Step 3: Implement**

In `core/src/scheduler/audit.rs`, add after `ACTION_TASK_SUBMITTED`:

```rust
/// `action` written when the reviewer escalated a plan and an operator ask
/// was raised for it (#564 slice 1b). `actor='scheduler'`. Payload:
/// `{ask_id, task_id, kind, plan_digest, severity, deadline_at}`.
///
/// The plaintext correlation nonce is deliberately NOT in the payload:
/// `audit_log` is readable by every role that can read the audit trail, and
/// the nonce is a live approval token.
pub const ACTION_ASK_RAISED: &str = "ask.raised";

/// `action` written when an operator answered a raised ask (#564 slice 1b).
/// `actor='cli'` from `kastellan-cli inbox resolve`; a future channel
/// resolver writes the same action under its own actor. Payload:
/// `{ask_id, task_id, choice, resolved_by, free_text}`.
///
/// `choice` is what separates an operator denial from a CASSANDRA block:
/// both land in `tasks.state='blocked'` (see `Outcome::final_state`).
pub const ACTION_ASK_RESOLVED: &str = "ask.resolved";

/// `action` written for each ask the deadline sweep retired (#564 slice
/// 1b). `actor='scheduler'`. Payload: `{ask_id, task_id}`.
pub const ACTION_ASK_EXPIRED: &str = "ask.expired";
```

In `core/src/scheduler/asks.rs`, add the async half below the pure functions:

```rust
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};

use kastellan_db::asks as db_asks;
use kastellan_db::DbError;

use crate::cassandra::plan_digest::plan_digest;
use crate::cassandra::types::{Plan, Severity};

use super::audit::{ACTION_ASK_EXPIRED, ACTION_ASK_RAISED, SCHEDULER_AUDIT_ACTOR};

/// Raise a `plan_approval` ask for an escalated plan and suspend its task.
///
/// Returns the new ask's id. The task is `awaiting_operator` on success —
/// `db::asks::raise` writes the ask INSERT and the task UPDATE in one
/// transaction, so there is no window where either exists without the
/// other.
///
/// **The plaintext nonce is dropped unread, and that is correct for this
/// slice** (spec D5). Slice 1b's only answer surface is `kastellan-cli
/// inbox`, which resolves by row id — the path `db::asks::resolve` reserves
/// for a trusted local caller. Logging the nonce would put a live approval
/// token into `~/.local/state/kastellan/*.out`, a plaintext file with none
/// of `audit_log`'s role gating. Slice 2 delivers it over Matrix at raise
/// time; it never needs to recover one raised earlier.
///
/// `plan` is digested as passed — i.e. *after* `apply_floor_raise`,
/// `data_ceiling` resolution, invoke expansion and namespace completion.
/// That is deliberate: the digest must cover what would execute, and the
/// same normalisations run again on the replan, so the two digests are
/// comparable.
pub async fn raise_and_suspend(
    pool: &PgPool,
    task_id: i64,
    plan: &Plan,
    concern: &str,
    severity: Severity,
) -> Result<i64, DbError> {
    let digest = plan_digest(plan);
    let deadline_at = OffsetDateTime::now_utc() + Duration::seconds(deadline_from_env());

    let raised = db_asks::raise(
        pool,
        task_id,
        ASK_KIND_PLAN_APPROVAL,
        concern,
        &serde_json::json!(["approve", "deny"]),
        Some(&digest),
        deadline_at,
    )
    .await?;

    // Destructured rather than field-accessed so the nonce's drop (and its
    // zeroize) is visible at this call site instead of implied.
    let db_asks::RaisedAsk { ask_id, nonce } = raised;
    drop(nonce);

    emit_ask_raised(pool, ask_id, task_id, &digest, severity, deadline_at).await;
    Ok(ask_id)
}

/// Best-effort `ask.raised` row. Same posture as
/// `runner::audit_rows::write_lifecycle_row`: the ask and the suspension
/// have already committed, and a transient `audit_log` failure must not
/// undo them.
async fn emit_ask_raised(
    pool: &PgPool,
    ask_id: i64,
    task_id: i64,
    plan_digest: &str,
    severity: Severity,
    deadline_at: OffsetDateTime,
) {
    let payload = serde_json::json!({
        "ask_id": ask_id,
        "task_id": task_id,
        "kind": ASK_KIND_PLAN_APPROVAL,
        "plan_digest": plan_digest,
        "severity": format!("{severity:?}"),
        "deadline_at": deadline_at.to_string(),
    });
    if let Err(e) =
        kastellan_db::audit::insert(pool, SCHEDULER_AUDIT_ACTOR, ACTION_ASK_RAISED, payload).await
    {
        tracing::warn!(
            ask_id, task_id, error = %e,
            "audit insert for scheduler/ask.raised failed (best-effort)"
        );
    }
}

/// Expire every overdue ask and emit one `ask.expired` row each. Returns
/// how many were retired.
///
/// Mirrors [`super::crash_recovery::sweep_and_audit`] exactly: the DB sweep
/// is fail-closed (its error propagates) and the audit inserts are
/// best-effort.
///
/// `db::asks::expire_due` returns only the rows *it* moved, so a concurrent
/// second sweep cannot make two callers each emit a row for the same ask,
/// and re-running finds nothing.
pub async fn sweep_expired_and_audit(pool: &PgPool) -> Result<usize, DbError> {
    let expired = db_asks::expire_due(pool).await?;
    for e in &expired {
        let payload = serde_json::json!({"ask_id": e.ask_id, "task_id": e.task_id});
        if let Err(err) =
            kastellan_db::audit::insert(pool, SCHEDULER_AUDIT_ACTOR, ACTION_ASK_EXPIRED, payload)
                .await
        {
            tracing::warn!(
                ask_id = e.ask_id, task_id = e.task_id, error = %err,
                "audit insert for scheduler/ask.expired failed (best-effort)"
            );
        }
    }
    if !expired.is_empty() {
        tracing::warn!(count = expired.len(), "expired overdue operator asks; their tasks failed closed");
    }
    Ok(expired.len())
}
```

If `Severity` does not derive `Debug`, use whatever `Display`/`as_str` it does provide rather than adding a derive — check `core/src/cassandra/types.rs` before writing `format!("{severity:?}")`.

- [ ] **Step 4: Run the tests to verify they pass**

```sh
cargo test -p kastellan-core --test scheduler_asks_e2e -- --nocapture
cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: 4 passed (or `[SKIP]` lines if PG is unavailable — in which case set `KASTELLAN_PG_BIN_DIR` and re-run; do not proceed on a skip).

- [ ] **Step 5: Commit**

```sh
git add core/src/scheduler/audit.rs core/src/scheduler/asks.rs core/tests/scheduler_asks_e2e.rs
git commit -m "feat(scheduler): raise an ask for an escalated plan, and sweep overdue ones

The plaintext nonce is destructured and dropped rather than field-accessed,
so its zeroize is visible at the call site. It is not logged and not put in
the audit payload: audit_log is readable by every role that can read the
trail, and the daemon log has no role gating at all, while the nonce is a
live approval token. Slice 2's Matrix delivery is its only consumer.

The digest test pins that raise_and_suspend calls plan_digest rather than
hashing something convenient — two plans differing only in `context`, one of
the four EXCLUDED fields, must digest identically.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: `run_one` reads the decision, terminates a denial, and carries the plan count

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs` (add `resolved_ask` to `TaskContext`)
- Modify: `core/src/scheduler/runner/task_exec.rs` (two pure helpers + the wiring)
- Test: `core/src/scheduler/runner/task_exec.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: Task 1's `latest_resolved_for_task`; Task 2's `Outcome::Denied`; Task 3's `resolution_choice` / `Choice`.
- Produces:
  - `TaskContext.resolved_ask: Option<kastellan_db::asks::Ask>`
  - `pub(crate) fn pre_plan_outcome(resolved: Option<&Ask>) -> Option<Outcome>`
  - `pub(crate) fn resume_budget(db_plan_count: i32, max_plans: u32) -> (u32, u32)`

- [ ] **Step 1: Write the failing tests**

Add to `core/src/scheduler/runner/task_exec.rs` (create the `mod tests` block if the file has none):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_db::asks::Ask;
    use time::OffsetDateTime;

    fn resolved_ask(choice: &str) -> Ask {
        Ask {
            id: 9,
            task_id: 3,
            kind: "plan_approval".to_string(),
            body: "this sends mail to a stranger".to_string(),
            options: serde_json::json!(["approve", "deny"]),
            plan_digest: Some("digest-a".to_string()),
            state: "resolved".to_string(),
            created_at: OffsetDateTime::now_utc(),
            deadline_at: OffsetDateTime::now_utc(),
            resolved_at: Some(OffsetDateTime::now_utc()),
            resolved_by: Some("operator".to_string()),
            resolution: Some(serde_json::json!({"choice": choice})),
        }
    }

    #[test]
    fn a_denial_terminates_the_task_before_any_planning() {
        let out = pre_plan_outcome(Some(&resolved_ask("deny"))).expect("a denial is terminal");
        match out {
            Outcome::Denied { ask_id, reason } => {
                assert_eq!(ask_id, 9);
                // The ask's body — the question the operator answered —
                // not the operator's own note (spec D10).
                assert_eq!(reason, "this sends mail to a stranger");
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn an_approval_does_not_terminate_the_task() {
        assert!(pre_plan_outcome(Some(&resolved_ask("approve"))).is_none());
    }

    #[test]
    fn no_resolved_ask_does_not_terminate_the_task() {
        assert!(pre_plan_outcome(None).is_none());
    }

    #[test]
    fn a_malformed_resolution_does_not_terminate_the_task() {
        // Fail toward running rather than toward a silent refusal: a task
        // killed by an unparseable row is far harder to diagnose than one
        // that escalates again.
        let mut a = resolved_ask("deny");
        a.resolution = Some(serde_json::json!({"choice": "maybe"}));
        assert!(pre_plan_outcome(Some(&a)).is_none());
    }

    #[test]
    fn resume_budget_carries_the_count_and_extends_the_allowance() {
        // A fresh task: nothing carried, budget is the lane default.
        assert_eq!(resume_budget(0, 5), (0, 5));
        // A task that escalated after 4 plans and was approved: the column
        // keeps its 4 (so `increment_plan_count`'s absolute write no longer
        // rewinds it to 1) and the approved plan gets a full 5 more.
        assert_eq!(resume_budget(4, 5), (4, 9));
    }

    #[test]
    fn resume_budget_survives_impossible_column_values() {
        // plan_count is a signed column; a negative would make `as u32`
        // wrap to ~4 billion and the cap check pass forever.
        assert_eq!(resume_budget(-1, 5), (0, 5));
        // And the extension must not overflow into a tiny budget.
        assert_eq!(resume_budget(i32::MAX, u32::MAX).1, u32::MAX);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib task_exec::tests -- --nocapture
```

Expected: FAIL to compile — `pre_plan_outcome` and `resume_budget` do not exist.

- [ ] **Step 3: Implement**

First, add the field to `TaskContext` in `core/src/scheduler/inner_loop.rs`, after `blocks`:

```rust
    /// The task's most recently resolved operator ask, when it has one.
    ///
    /// Read **once** by `runner::task_exec::run_one` before the first
    /// formulation and threaded in here, so the `Escalate` arm compares
    /// digests against an in-memory value rather than issuing a second
    /// query from inside the loop — and so a test can construct the
    /// decision without a live Postgres (spec D4).
    ///
    /// Never holds a denial in practice: `run_one` terminates a denied task
    /// before building this context. `asks::decide` still handles that case
    /// correctly rather than assuming it away.
    pub resolved_ask: Option<kastellan_db::asks::Ask>,
```

Every existing `TaskContext { .. }` literal now needs `resolved_ask: None`. Let the compiler list them — expect construction sites in `runner/task_exec.rs`, `core/tests/scheduler_inner_loop_e2e.rs`, `core/tests/scheduler_lanes_e2e.rs`, and possibly `inner_loop/tests.rs` and the observation-replay path. **Audit each one rather than trusting that it compiles** — the #506 lesson is that a mechanical field addition can leave a test agreeing with the code and disagreeing with the intent.

Then in `core/src/scheduler/runner/task_exec.rs`, add the two pure helpers above `run_one`:

```rust
use kastellan_db::asks::Ask;
use crate::scheduler::asks::{resolution_choice, Choice};

/// The terminal outcome a task's already-resolved ask forces before any
/// planning happens, or `None` to proceed.
///
/// Only a **denial** is terminal here. `asks::resolve` re-enqueues the task
/// on any resolution, so a denied task returns to `pending`, is claimed,
/// and would otherwise replan from scratch — and if denial only bound to
/// the plan digest, the agent could replan around it: the operator denies
/// plan P, the agent produces P′, P′ passes review, and the thing that was
/// just refused executes. An operator saying "deny" means *do not do this*
/// (spec D2).
///
/// `reason` is the ask's `body` — the concern the operator was answering.
/// Their optional free-text note is deliberately not copied here; it lives
/// in `asks.resolution` and the `ask.resolved` audit row (spec D10).
///
/// A malformed resolution proceeds rather than terminating: failing toward
/// running costs an escalation, where failing toward refusal kills a task
/// for a reason nobody can see in the plan trail.
pub(crate) fn pre_plan_outcome(resolved: Option<&Ask>) -> Option<Outcome> {
    let ask = resolved?;
    match resolution_choice(ask) {
        Some(Choice::Deny) => Some(Outcome::Denied {
            ask_id: ask.id,
            reason: ask.body.clone(),
        }),
        _ => None,
    }
}

/// `(starting plan_count, max_plans)` for a claimed task, given the value
/// its DB column holds.
///
/// Two separable things the old code conflated by starting every run at 0
/// (spec D6):
///
/// * **the column is a historical fact.** `inner_loop` mirrors
///   `ctx.plan_count` back with `increment_plan_count`, which writes the
///   **absolute** value — so a task that escalated after 4 plans and
///   resumed at 0 rewrote its own column 4 → 1. Seeding from the column
///   keeps it monotonic and true.
/// * **the budget is a policy.** An approved plan gets a full further
///   allowance, because the alternative — spending from the original
///   budget — leaves a task that escalated on its last allowed plan
///   resuming with none, so the operator's approval buys nothing. Runaway
///   is not the risk it looks like: each additional allowance costs one
///   human interaction.
///
/// A negative column value (impossible today; `plan_count` is a signed
/// column with a non-negative writer) clamps to 0 rather than wrapping
/// through `as u32` to ~4 billion, which would make the cap check pass
/// forever.
pub(crate) fn resume_budget(db_plan_count: i32, max_plans: u32) -> (u32, u32) {
    let carried = u32::try_from(db_plan_count).unwrap_or(0);
    (carried, carried.saturating_add(max_plans))
}
```

Now wire them into `run_one`. Immediately before the `let ctx = TaskContext { … }` literal:

```rust
    // One read, two consumers (spec D4): the denial check just below, and
    // the `Escalate` arm's digest comparison via `ctx.resolved_ask`.
    //
    // A read failure is NOT fatal. It means at worst that an approval is
    // not seen and the plan escalates again — one more operator
    // interaction — where failing the task would turn a transient DB blip
    // into a lost task.
    let resolved_ask = match kastellan_db::asks::latest_resolved_for_task(pool, task.id).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(
                task_id = task.id, error = %e,
                "could not read the task's resolved ask; proceeding as if there is none \
                 (an approval will not be seen and the plan will escalate again)"
            );
            None
        }
    };

    if let Some(outcome) = pre_plan_outcome(resolved_ask.as_ref()) {
        tracing::info!(task_id = task.id, "operator denied this task's ask; terminating without planning");
        return InnerLoopResult {
            outcome,
            plan_count: u32::try_from(task.plan_count).unwrap_or(0),
            dispatch_count: 0,
            terminal_l1_insight: None,
            terminal_l3_skill: None,
            terminal_python_skill: None,
        };
    }

    let (start_plan_count, max_plans_for_run) = resume_budget(task.plan_count, max_plans_override);
```

Then in the `TaskContext` literal, replace `plan_count: 0` with `plan_count: start_plan_count`, replace `max_plans: max_plans_override` with `max_plans: max_plans_for_run`, and add `resolved_ask,`.

**Check the order:** `max_plans_override` is computed earlier in `run_one` from the payload; `resume_budget` must take *that* value, not the bare `max_plans` argument, or a task-specific override is lost on resume.

- [ ] **Step 4: Run the tests to verify they pass**

```sh
cargo test -p kastellan-core --lib task_exec::tests -- --nocapture
cargo build --workspace
cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: 6 passed; workspace builds; clippy exit 0.

- [ ] **Step 5: Commit**

```sh
git add core/src/scheduler/inner_loop.rs core/src/scheduler/runner/task_exec.rs
git commit -m "feat(scheduler): a denial terminates the task, and the plan count stops rewinding

asks::resolve re-enqueues a task on ANY resolution, so a denied task
replans from scratch. Had denial only bound to the plan digest, the agent
could replan around it — operator denies P, agent produces P', P' passes
review, and the refused thing executes.

The plan count fix is separable and was a defect either way:
increment_plan_count writes the ABSOLUTE value, so a task that escalated
after 4 plans and resumed at 0 rewrote its own column 4 -> 1. The column is
now seeded from the DB and the budget is extended rather than reset, which
also stops a task that escalated on its last allowed plan from resuming
with no budget at all.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 6: The `Escalate` arm raises the ask — and the end-to-end test that reaches `drain_lane`'s branch

This is the task that makes Task 2's non-finalize branch reachable. It must not be split from its test.

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs:467-503` (the `Verdict::Escalate` arm)
- Test: `core/tests/scheduler_ask_path_e2e.rs` (create)

**Interfaces:**
- Consumes: Task 3's `decide` / `AskDecision`; Task 4's `raise_and_suspend`; Task 5's `TaskContext.resolved_ask`.
- Produces: no new public API.

- [ ] **Step 1: Write the failing test**

Create `core/tests/scheduler_ask_path_e2e.rs`. Drive the real lane runner via `spawn_scheduler` — copy the harness shape from `core/tests/scheduler_lanes_e2e.rs`, which already does this.

```rust
//! End-to-end: escalate -> suspend -> resolve -> resume, through the real
//! lane runner (#564 slice 1b).
//!
//! Driven through `spawn_scheduler` rather than `run_to_terminal`, because
//! `drain_lane`'s non-finalize branch is the thing under test and only the
//! lane runner reaches it.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kastellan_core::cassandra::review::{ChainReviewStage, ReviewStage, ReviewStageContext};
use kastellan_core::cassandra::types::{Plan, Severity, Verdict};
use kastellan_core::scheduler::audit::ACTION_TASK_FINALIZE;
use kastellan_db::tasks::{self, insert_pending, Lane};

// <copy the bring_up_pg helper and the ScriptedFormulator/CountingDispatcher
//  stubs from core/tests/scheduler_lanes_e2e.rs; change only the PG service
//  name prefix to kastellan-sched-test-pg-askpath->

/// Escalates the first `escalate_first_n` plans it sees, then approves.
struct EscalatingReview {
    remaining: Mutex<u32>,
}

#[async_trait]
impl ReviewStage for EscalatingReview {
    fn name(&self) -> &str { "escalating" }
    async fn review(&self, _plan: &Plan, _ctx: &ReviewStageContext<'_>) -> Verdict {
        let mut n = self.remaining.lock().unwrap();
        if *n > 0 {
            *n -= 1;
            Verdict::Escalate("needs a human".to_string(), Severity::High)
        } else {
            Verdict::Approve
        }
    }
}

/// Poll `tasks.state` until it equals `want`, or fail after `secs`.
/// A fixed sleep would pass or fail on machine speed rather than on
/// behaviour.
async fn await_state(pool: &sqlx::PgPool, task_id: i64, want: &str, secs: u64) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let s = tasks::observe_state(pool, task_id).await.expect("state");
        if s == want || std::time::Instant::now() > deadline {
            return s;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_escalated_plan_suspends_the_task_and_writes_no_finalize_row() {
    let Some((pool, _cluster)) = bring_up_pg("suspend").await else { return };
    let task_id = insert_pending(&pool, Lane::Fast, serde_json::json!({"instruction": "x", "max_plans": 3}))
        .await.expect("insert");

    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(EscalatingReview {
        remaining: Mutex::new(1),
    })]));
    let handle = spawn_test_scheduler(&pool, review, /*plans*/ 3);

    assert_eq!(await_state(&pool, task_id, "awaiting_operator", 20).await, "awaiting_operator");

    // The load-bearing negative: `drain_lane` must NOT have finalized.
    let finalize_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = $1 AND payload->>'task_id' = $2::text",
    )
    .bind(ACTION_TASK_FINALIZE).bind(task_id)
    .fetch_one(&pool).await.expect("count");
    assert_eq!(finalize_rows, 0, "a suspended task has not finished and must not be finalized");

    let pending = kastellan_db::asks::list_pending(&pool, 10).await.expect("list");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].task_id, task_id);

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approval_lets_the_same_plan_through_on_resume() {
    let Some((pool, _cluster)) = bring_up_pg("approve").await else { return };
    let task_id = insert_pending(&pool, Lane::Fast, serde_json::json!({"instruction": "x", "max_plans": 3}))
        .await.expect("insert");

    // The reviewer escalates EVERY plan and the formulator returns the
    // identical plan every time. That combination is load-bearing: with a
    // reviewer that stops escalating after the first plan, the resumed run
    // would simply be approved by the reviewer and the test would pass
    // whether or not the arm ever consults the operator's approval. Here
    // the replan escalates again, so the only way the task can complete is
    // the digest matching the resolved ask.
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(EscalatingReview {
        remaining: Mutex::new(u32::MAX),
    })]));
    let handle = spawn_test_scheduler(&pool, review);

    assert_eq!(await_state(&pool, task_id, "awaiting_operator", 20).await, "awaiting_operator");
    let ask = kastellan_db::asks::list_pending(&pool, 10).await.expect("list")[0].clone();
    assert!(kastellan_db::asks::resolve(
        &pool, ask.id, "operator", &serde_json::json!({"choice": "approve"})
    ).await.expect("resolve"));

    assert_eq!(await_state(&pool, task_id, "completed", 20).await, "completed");
    // Exactly one ask: the approval covered the replan rather than
    // producing a second question.
    let asks_count: i64 = sqlx::query_scalar("SELECT count(*) FROM asks WHERE task_id = $1")
        .bind(task_id).fetch_one(&pool).await.expect("count");
    assert_eq!(asks_count, 1, "an approved, identical replan must not re-escalate");

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denial_terminates_the_task_without_replanning() {
    let Some((pool, _cluster)) = bring_up_pg("deny").await else { return };
    let task_id = insert_pending(&pool, Lane::Fast, serde_json::json!({"instruction": "x", "max_plans": 3}))
        .await.expect("insert");

    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(EscalatingReview {
        remaining: Mutex::new(1),
    })]));
    // The formulator counts its calls so we can assert the resumed run
    // never asked for a plan.
    let (handle, formulate_calls) = spawn_test_scheduler_counting(&pool, review);

    assert_eq!(await_state(&pool, task_id, "awaiting_operator", 20).await, "awaiting_operator");
    let calls_at_suspend = *formulate_calls.lock().unwrap();
    let ask = kastellan_db::asks::list_pending(&pool, 10).await.expect("list")[0].clone();
    assert!(kastellan_db::asks::resolve(
        &pool, ask.id, "operator", &serde_json::json!({"choice": "deny"})
    ).await.expect("resolve"));

    assert_eq!(await_state(&pool, task_id, "blocked", 20).await, "blocked");
    assert_eq!(
        *formulate_calls.lock().unwrap(), calls_at_suspend,
        "a denied task must terminate BEFORE planning — this is the assertion that \
         fails if the denial only bound to the plan digest",
    );

    let t = tasks::get(&pool, task_id).await.expect("get").expect("a task");
    let r = t.result.expect("a denied task has a result");
    assert_eq!(r.get("kind").and_then(|v| v.as_str()), Some("denied"));
    assert_eq!(r.get("ask_id").and_then(|v| v.as_i64()), Some(ask.id));

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_different_replan_raises_a_second_ask_rather_than_riding_the_first_approval() {
    let Some((pool, _cluster)) = bring_up_pg("differs").await else { return };
    let task_id = insert_pending(&pool, Lane::Fast, serde_json::json!({"instruction": "x", "max_plans": 3}))
        .await.expect("insert");

    // Every plan escalates, and the formulator returns a DIFFERENT plan each
    // time (vary a digested field — e.g. a step's `parameters` — not
    // `context`, which the digest excludes).
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(EscalatingReview {
        remaining: Mutex::new(5),
    })]));
    let handle = spawn_test_scheduler_varying(&pool, review);

    assert_eq!(await_state(&pool, task_id, "awaiting_operator", 20).await, "awaiting_operator");
    let first = kastellan_db::asks::list_pending(&pool, 10).await.expect("list")[0].clone();
    assert!(kastellan_db::asks::resolve(
        &pool, first.id, "operator", &serde_json::json!({"choice": "approve"})
    ).await.expect("resolve"));

    // The replan differs, so the approval must not cover it.
    assert_eq!(await_state(&pool, task_id, "awaiting_operator", 20).await, "awaiting_operator");
    let asks_count: i64 = sqlx::query_scalar("SELECT count(*) FROM asks WHERE task_id = $1")
        .bind(task_id).fetch_one(&pool).await.expect("count");
    assert_eq!(asks_count, 2, "an approval binds to a digest, not to a task");

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_plan_count_is_monotonic_across_a_suspend_and_resume() {
    let Some((pool, _cluster)) = bring_up_pg("plancount").await else { return };
    let task_id = insert_pending(&pool, Lane::Fast, serde_json::json!({"instruction": "x", "max_plans": 3}))
        .await.expect("insert");

    // Always-escalating, identical plan — same reasoning as the approval
    // test: the resumed run must reach completion via the approval, not by
    // the reviewer changing its mind.
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(EscalatingReview {
        remaining: Mutex::new(u32::MAX),
    })]));
    let handle = spawn_test_scheduler(&pool, review);

    assert_eq!(await_state(&pool, task_id, "awaiting_operator", 20).await, "awaiting_operator");
    let before = tasks::get(&pool, task_id).await.unwrap().unwrap().plan_count;
    assert!(before >= 1, "at least one plan ran before the escalation");

    let ask = kastellan_db::asks::list_pending(&pool, 10).await.expect("list")[0].clone();
    assert!(kastellan_db::asks::resolve(
        &pool, ask.id, "operator", &serde_json::json!({"choice": "approve"})
    ).await.expect("resolve"));
    assert_eq!(await_state(&pool, task_id, "completed", 20).await, "completed");

    let after = tasks::get(&pool, task_id).await.unwrap().unwrap().plan_count;
    assert!(
        after > before,
        "plan_count must not rewind across a resume (was {before}, now {after})",
    );

    handle.shutdown().await;
}
```

Write the three `spawn_test_scheduler*` helpers in this file. Factor the shared body so they differ only in the formulator:

```rust
/// The shared body. `formulator` is the only thing the three variants
/// differ in; everything else is the stub set `scheduler_lanes_e2e.rs`
/// already builds (a no-op dispatcher, a no-op entity extractor, a no-op
/// embedder — copy those three stubs verbatim from that file).
fn spawn_with(
    pool: &sqlx::PgPool,
    formulator: Arc<dyn kastellan_core::scheduler::agent::PlanFormulator>,
    review: Arc<ChainReviewStage>,
) -> kastellan_core::scheduler::SchedulerHandle {
    kastellan_core::scheduler::spawn_scheduler(
        pool.clone(),
        formulator,
        review,
        Arc::new(NoopDispatcher),
        Arc::new(NoopEntityExtractor),
        Arc::new(NoopEmbedder),
    )
}

/// One formulator covering all three variants. `vary` makes each call
/// return a plan differing in a field the digest INCLUDES; `calls` is the
/// counter the denial test reads.
struct TestFormulator {
    vary: bool,
    calls: Arc<Mutex<u32>>,
}

#[async_trait]
impl kastellan_core::scheduler::agent::PlanFormulator for TestFormulator {
    async fn formulate_plan(
        &self,
        _ctx: &kastellan_core::scheduler::inner_loop::TaskContext,
    ) -> Result<(Plan, FormulationMeta), AgentError> {
        let n = {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            *c
        };
        // `parameters` is digested; `context` is not. Varying the former is
        // what makes the replan a genuinely different plan.
        let plan = if self.vary {
            one_step_plan_with_param(n)
        } else {
            task_complete_plan("ok")
        };
        Ok((plan, test_meta()))
    }
}

/// Returns the same terminal plan on every call.
fn spawn_test_scheduler(
    pool: &sqlx::PgPool, review: Arc<ChainReviewStage>,
) -> kastellan_core::scheduler::SchedulerHandle {
    spawn_with(pool, Arc::new(TestFormulator { vary: false, calls: Arc::new(Mutex::new(0)) }), review)
}

/// As above, plus the shared counter of `formulate_plan` calls.
fn spawn_test_scheduler_counting(
    pool: &sqlx::PgPool, review: Arc<ChainReviewStage>,
) -> (kastellan_core::scheduler::SchedulerHandle, Arc<Mutex<u32>>) {
    let calls = Arc::new(Mutex::new(0));
    let h = spawn_with(pool, Arc::new(TestFormulator { vary: false, calls: Arc::clone(&calls) }), review);
    (h, calls)
}

/// Returns a DIFFERENT plan each call.
fn spawn_test_scheduler_varying(
    pool: &sqlx::PgPool, review: Arc<ChainReviewStage>,
) -> kastellan_core::scheduler::SchedulerHandle {
    spawn_with(pool, Arc::new(TestFormulator { vary: true, calls: Arc::new(Mutex::new(0)) }), review)
}
```

`test_meta()` is a `FormulationMeta` fixture — copy the literal from `ScriptedFormulator::formulate_plan` in `core/tests/scheduler_lanes_e2e.rs` verbatim, including the `recall_query_sha256` value. `task_complete_plan` and `one_step_plan` are that file's factories (lines 147 and 164); `one_step_plan_with_param(n)` is `one_step_plan` with `parameters: serde_json::json!({"n": n})`.

Drop the `max_plans` argument from the three call sites in the tests above — the cap now travels in the task payload.

The lane's `max_plans` comes from `DEFAULT_MAX_PLANS_FAST`, not from `spawn_scheduler`'s arguments, so a per-task override goes in the task payload (`{"instruction": "x", "max_plans": 3}`) rather than through these helpers. Set it there in every test in this file, so a bug that loops instead of suspending hits the cap quickly instead of running to the lane default.

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test scheduler_ask_path_e2e -- --nocapture
```

Expected: FAIL — the task reaches `blocked` (today's degrade-to-`Block` path) instead of `awaiting_operator`, and no `asks` row exists.

- [ ] **Step 3: Implement**

Replace the `Verdict::Escalate` arm in `core/src/scheduler/inner_loop.rs`. Delete the `TODO(channel-bus)` block and the degrade-to-`Block` warn entirely; keep the refusal-plan `info!` exactly as it is.

```rust
            Verdict::Escalate(reason, sev) => {
                // A refusal plan is already terminal; escalating it would
                // ask a human about something that is not going to happen.
                // Falls through to the refusal check below, unchanged.
                if plan.refused.is_none() {
                    // Digest the plan as it stands — after the floor raise,
                    // the `data_ceiling` resolution, invoke expansion and
                    // namespace completion — because that is what would
                    // execute, and the replan runs the same normalisations
                    // so the two digests are comparable.
                    let digest = crate::cassandra::plan_digest::plan_digest(&plan);
                    let approved = ctx.resolved_ask.as_ref().is_some_and(|a| {
                        matches!(asks::decide(a, &digest), asks::AskDecision::Approved)
                    });
                    if approved {
                        tracing::info!(
                            task_id = ctx.task_id,
                            plan_count = ctx.plan_count,
                            severity = ?sev,
                            "Verdict::Escalate covered by a resolved operator approval for this \
                             exact plan; proceeding"
                        );
                        // fall through and execute
                    } else {
                        match asks::raise_and_suspend(
                            pool, ctx.task_id, &plan, reason, *sev,
                        ).await {
                            Ok(ask_id) => {
                                tracing::info!(
                                    task_id = ctx.task_id, ask_id,
                                    plan_count = ctx.plan_count, severity = ?sev,
                                    reason = %reason,
                                    "Verdict::Escalate raised an operator ask; task suspended"
                                );
                                return finish!(Outcome::AwaitingOperator { ask_id });
                            }
                            // Fail, do not fall back to Block. Degrading
                            // silently is the behaviour this slice deletes,
                            // and doing it on the one path where the
                            // reviewer said a human must decide is the worst
                            // place to keep it. If the row really was
                            // cancelled underneath us, `finalize` is a no-op
                            // for it anyway.
                            Err(e) => {
                                tracing::error!(
                                    task_id = ctx.task_id, error = %e,
                                    "Verdict::Escalate could not raise an operator ask"
                                );
                                return finish!(Outcome::Failed(format!(
                                    "escalation could not be raised: {e}"
                                )));
                            }
                        }
                    }
                } else {
                    // Keep the existing info! for the refusal-plan case
                    // verbatim.
                }
            }
```

Add `use super::asks;` (or the equivalent path) to the file's imports.

**Check `reason`'s type at the match site** — the arm binds `Verdict::Escalate(reason, sev)` by reference through `match &verdict`, so `reason` is `&String`; pass it as `reason` (deref-coerces to `&str`) and **not** as `&reason`, which is `clippy::useless_borrows_in_formatting` territory on CI's rust 1.97.

Also confirm that falling through the `approved` branch reaches step execution rather than the `continue` the old code used — the old arm always either `continue`d or fell through to the refusal check; the approved path must land on the latter.

- [ ] **Step 4: Run the tests to verify they pass**

```sh
cargo test -p kastellan-core --test scheduler_ask_path_e2e -- --nocapture
cargo test -p kastellan-core --test scheduler_inner_loop_e2e -- --nocapture
cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: 5 new tests pass and the existing inner-loop e2e still passes. If an existing test asserted the escalate→`Blocked` degradation, it is now asserting deleted behaviour — update it to the new outcome and say so in the commit, do not delete it.

- [ ] **Step 5: Commit**

```sh
git add core/src/scheduler/inner_loop.rs core/tests/scheduler_ask_path_e2e.rs
git commit -m "feat(scheduler): Escalate raises an operator ask instead of degrading to Block

Closes the TODO(channel-bus) the arm has carried since the channel bus
landed. The primitive it was waiting for is #564 slice 1a's ask record.

Driven through spawn_scheduler rather than run_to_terminal, because
drain_lane's non-finalize branch is half of what is under test and only
the lane runner reaches it. The load-bearing assertion is a negative: a
suspended task must have NO task.finalize row.

The denial test asserts the formulator call count is unchanged across the
resume — that is the assertion that fails if a denial binds to the plan
digest instead of the task, which is the hole where the agent replans
around a refusal.

A raise failure fails the task rather than falling back to Block: silent
degradation is what this commit deletes, and the escalate path is the worst
place to keep it.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 7: The periodic expiry sweep

**Files:**
- Modify: `core/src/scheduler/runner.rs` (`SchedulerHandle`, `spawn_scheduler`, a new `sweep_loop`)
- Modify: `core/src/main.rs:120` (the startup call, beside `crash_recovery::sweep_and_audit`)
- Test: `core/tests/scheduler_ask_path_e2e.rs` (one more test)

**Interfaces:**
- Consumes: Task 4's `sweep_expired_and_audit`.
- Produces: `SchedulerHandle.sweep: JoinHandle<()>` (joined by the existing `shutdown()`).

- [ ] **Step 1: Write the failing test**

Append to `core/tests/scheduler_ask_path_e2e.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_ask_expires_and_fails_its_task_without_a_restart() {
    let Some((pool, _cluster)) = bring_up_pg("expire").await else { return };
    let task_id = insert_pending(&pool, Lane::Fast, serde_json::json!({"instruction": "x", "max_plans": 3}))
        .await.expect("insert");

    // One-second deadline through the documented knob, set before the
    // scheduler starts so the raise inside it picks it up.
    std::env::set_var(kastellan_core::scheduler::asks::ASK_DEADLINE_ENV, "1");
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(EscalatingReview {
        remaining: Mutex::new(5),
    })]));
    let handle = spawn_test_scheduler(&pool, review);

    assert_eq!(await_state(&pool, task_id, "awaiting_operator", 20).await, "awaiting_operator");

    // Nobody answers. The sweep must reach it while this process keeps
    // running — a startup-only sweep would leave it suspended forever.
    assert_eq!(await_state(&pool, task_id, "failed", 90).await, "failed");
    let t = tasks::get(&pool, task_id).await.unwrap().unwrap();
    assert_eq!(
        t.result.as_ref().and_then(|r| r.get("detail")).and_then(|d| d.as_str()),
        Some(kastellan_db::asks::ASK_TIMEOUT_DETAIL),
    );

    std::env::remove_var(kastellan_core::scheduler::asks::ASK_DEADLINE_ENV);
    handle.shutdown().await;
}
```

This test needs the sweep interval to be shorter than its 90 s budget. Make the interval a module constant and keep it at 60 s — 60 < 90 with margin. Do **not** add a test-only env knob for the interval; a constant that the test's own budget accommodates is one fewer configuration surface.

Serialise this test against the other `ASK_DEADLINE_ENV` user (Task 4's sweep test is in a different binary, so only same-file collisions matter) — if any other test in this file sets it, put both behind a shared `Mutex` guard.

- [ ] **Step 2: Run the test to verify it fails**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test scheduler_ask_path_e2e an_unanswered_ask -- --nocapture
```

Expected: FAIL — the task stays `awaiting_operator` for the full 90 s because nothing sweeps.

- [ ] **Step 3: Implement**

In `core/src/scheduler/runner.rs`, add the interval constant beside `HEARTBEAT`:

```rust
/// How often the expiry sweep looks for overdue operator asks.
///
/// Slice 1a's spec put this at daemon startup only. On a daemon that runs
/// for weeks that is not a deadline: an unanswered ask holds its task in
/// `awaiting_operator` until the next restart, which is the permanent wedge
/// the deadline exists to prevent. The security half needs no sweep —
/// `asks::resolve` and `resolve_with_nonce` both carry
/// `AND deadline_at > now()`, so an expired nonce is dead on time
/// regardless — but the task side does.
const ASK_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
```

Add the field to `SchedulerHandle` and join it:

```rust
pub struct SchedulerHandle {
    shutdown: watch::Sender<bool>,
    pub fast: JoinHandle<()>,
    pub long: JoinHandle<()>,
    /// The operator-ask expiry sweep (#564 slice 1b). Lane-independent —
    /// `asks::expire_due` is a pool-wide UPDATE — so it is its own task
    /// rather than a call inside `drain_lane`, which is the per-lane claim
    /// hot path.
    pub sweep: JoinHandle<()>,
}

impl SchedulerHandle {
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.fast.await;
        let _ = self.long.await;
        let _ = self.sweep.await;
    }
}
```

Add the loop:

```rust
/// Expire overdue operator asks on a timer until shutdown.
///
/// A sweep error is logged and the loop continues: the next tick retries in
/// `ASK_SWEEP_INTERVAL`, and killing this task over a transient DB error
/// would silently disable every ask deadline for the life of the process
/// (nothing supervises it and every unit would still report `active` — the
/// same asymmetry the `tasks_resumed` LISTEN comment argues).
async fn sweep_loop(pool: PgPool, mut shutdown: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
            _ = sleep(ASK_SWEEP_INTERVAL) => {}
        }
        if *shutdown.borrow() { return; }
        if let Err(e) = super::asks::sweep_expired_and_audit(&pool).await {
            tracing::warn!(error = %e, "operator-ask expiry sweep failed; retrying next tick");
        }
    }
}
```

In `spawn_scheduler`, clone the pool for it and spawn it. The `long` lane currently consumes the un-cloned `pool` — give the sweep a clone before that line:

```rust
    let sweep = tokio::spawn(sweep_loop(pool.clone(), rx.clone()));
```

and return `SchedulerHandle { shutdown: tx, fast, long, sweep }`.

In `core/src/main.rs`, immediately after the existing `crash_recovery::sweep_and_audit` block (~line 120-123), add:

```rust
    // Overdue operator asks from a previous daemon life (#564 slice 1b).
    // The periodic sweep inside `spawn_scheduler` covers the running
    // daemon; this one covers the gap across a restart, so a task does not
    // wait a full interval to learn its ask timed out days ago.
    // Non-fatal for the same reason the crash sweep above is: a degraded
    // audit story is better than refusing to start.
    match kastellan_core::scheduler::asks::sweep_expired_and_audit(&pool).await {
        Ok(0) => {}
        Ok(n) => tracing::warn!(count = n, "expired overdue operator asks at startup"),
        Err(e) => tracing::warn!(error = %e, "asks::sweep_expired_and_audit failed (non-fatal)"),
    }
```

Match the exact `Ok`/`Err` arm style of the `crash_recovery` call above it.

- [ ] **Step 4: Run the test to verify it passes**

```sh
cargo test -p kastellan-core --test scheduler_ask_path_e2e -- --nocapture
cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: all 6 tests in the file pass; clippy exit 0. Confirm the expiry test actually took ~60-70 s rather than passing instantly — an instant pass means it hit some other failure path, not the sweep.

- [ ] **Step 5: Commit**

```sh
git add core/src/scheduler/runner.rs core/src/main.rs core/tests/scheduler_ask_path_e2e.rs
git commit -m "feat(scheduler): sweep overdue operator asks on a timer, not only at startup

Closes #571. Slice 1a's spec put the sweep at daemon startup; on a daemon
that runs for weeks that is not a deadline, it is a restart trigger. The
security half already held without it — both resolvers carry
AND deadline_at > now() — but the task side did not, and a suspended task
nobody answers was the permanent wedge the deadline exists to prevent.

Its own task rather than a call inside drain_lane: expire_due is a
pool-wide UPDATE and has nothing to do with the per-lane claim loop. A
sweep error retries next tick rather than ending the task, because nothing
supervises it and every unit would still report active.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 8: `kastellan-cli inbox`

**Files:**
- Create: `core/src/bin/kastellan-cli/inbox.rs`
- Modify: `core/src/bin/kastellan-cli/main.rs` (module decl, dispatch arm, `help_text`, the module-map doc comment)
- Test: `core/tests/cli_inbox_e2e.rs` (create)

**Interfaces:**
- Consumes: `kastellan_db::asks::{list_pending, get, resolve}`; Task 4's `ACTION_ASK_RESOLVED`; `kastellan_core::cli_audit::CLI_AUDIT_ACTOR`.
- Produces: `pub(crate) fn run_inbox(args: &[String]) -> ExitCode`

- [ ] **Step 1: Write the failing test**

Create `core/tests/cli_inbox_e2e.rs`, modelled on `core/tests/cli_cancel_audit_e2e.rs` (which already drives the CLI binary against a live PG and asserts on audit rows — copy its binary-location and env-plumbing helpers verbatim).

```rust
//! `kastellan-cli inbox` against a live PG (#564 slice 1b).

#![cfg(any(target_os = "linux", target_os = "macos"))]

// <copy bring_up_pg + the cli binary locator + env plumbing from
//  core/tests/cli_cancel_audit_e2e.rs>

#[tokio::test]
async fn inbox_list_shows_a_pending_ask_and_resolve_returns_the_task_to_pending() {
    let Some((pool, cluster)) = bring_up_pg("inbox").await else { return };
    let task_id = seed_running_task(&pool).await;
    let raised = kastellan_db::asks::raise(
        &pool, task_id, "plan_approval", "this sends mail to a stranger",
        &serde_json::json!(["approve", "deny"]), Some("digest-a"),
        time::OffsetDateTime::now_utc() + time::Duration::hours(1),
    ).await.expect("raise");

    let out = run_cli(&cluster, &["inbox", "list"]);
    assert!(out.status.success(), "inbox list must exit 0: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&raised.ask_id.to_string()), "the ask id must be listed");
    assert!(stdout.contains(&task_id.to_string()), "the task id must be listed");
    assert!(
        stdout.contains("this sends mail to a stranger"),
        "the question must be listed — an inbox that does not show the question is unusable"
    );

    let out = run_cli(&cluster, &["inbox", "resolve", &raised.ask_id.to_string(), "approve"]);
    assert!(out.status.success(), "inbox resolve must exit 0: {out:?}");

    assert_eq!(kastellan_db::tasks::observe_state(&pool, task_id).await.unwrap(), "pending");
    let ask = kastellan_db::asks::get(&pool, raised.ask_id).await.unwrap().unwrap();
    assert_eq!(ask.state, "resolved");
    assert_eq!(ask.resolution, Some(serde_json::json!({"choice": "approve"})));

    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE actor = 'cli' AND action = 'ask.resolved' \
         AND payload->>'ask_id' = $1::text",
    ).bind(raised.ask_id).fetch_one(&pool).await.unwrap();
    assert_eq!(rows, 1, "one ask.resolved row, actor=cli");
}

#[tokio::test]
async fn resolving_an_already_resolved_ask_exits_non_zero() {
    let Some((pool, cluster)) = bring_up_pg("inbox-twice").await else { return };
    let task_id = seed_running_task(&pool).await;
    let raised = kastellan_db::asks::raise(
        &pool, task_id, "plan_approval", "why",
        &serde_json::json!(["approve", "deny"]), Some("d"),
        time::OffsetDateTime::now_utc() + time::Duration::hours(1),
    ).await.expect("raise");

    let id = raised.ask_id.to_string();
    assert!(run_cli(&cluster, &["inbox", "resolve", &id, "approve"]).status.success());
    // First-responder-wins is already a DB property. What this pins is that
    // the CLI REPORTS the loss rather than printing success over it.
    let second = run_cli(&cluster, &["inbox", "resolve", &id, "deny"]);
    assert!(!second.status.success(), "a lost race must not exit 0");
    let ask = kastellan_db::asks::get(&pool, raised.ask_id).await.unwrap().unwrap();
    assert_eq!(
        ask.resolution, Some(serde_json::json!({"choice": "approve"})),
        "the first answer stands",
    );
}

#[tokio::test]
async fn an_unoffered_choice_is_refused_before_it_reaches_the_database() {
    let Some((pool, cluster)) = bring_up_pg("inbox-bad").await else { return };
    let task_id = seed_running_task(&pool).await;
    let raised = kastellan_db::asks::raise(
        &pool, task_id, "plan_approval", "why",
        &serde_json::json!(["approve", "deny"]), Some("d"),
        time::OffsetDateTime::now_utc() + time::Duration::hours(1),
    ).await.expect("raise");

    let out = run_cli(&cluster, &["inbox", "resolve", &raised.ask_id.to_string(), "maybe"]);
    assert_eq!(out.status.code(), Some(2), "a usage error exits 2");
    assert_eq!(
        kastellan_db::asks::get(&pool, raised.ask_id).await.unwrap().unwrap().state,
        "pending",
        "nothing was written",
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test cli_inbox_e2e -- --nocapture
```

Expected: FAIL — `unknown subcommand: inbox`, exit 2.

- [ ] **Step 3: Implement**

Create `core/src/bin/kastellan-cli/inbox.rs`:

```rust
//! `inbox {list,show,resolve}` — the operator's answer surface for asks the
//! daemon raised (#564 slice 1b).
//!
//! Named `inbox`, not `asks`, because `kastellan-cli ask` already means
//! *submit a task*: two subcommands differing by one letter, one of which
//! approves a plan, is a trap for exactly the operator who is tired enough
//! to be answering an escalation at all.
//!
//! Resolves by row **id**, using `db::asks::resolve` rather than
//! `resolve_with_nonce`. An id has no unforgeability property, which is safe
//! only because this caller is the operator's own local binary; any caller
//! reachable from an untrusted transport must use the nonce form (slice 2).

use std::process::ExitCode;

use kastellan_core::cli_audit::CLI_AUDIT_ACTOR;
use kastellan_core::scheduler::audit::ACTION_ASK_RESOLVED;

use crate::common::{resolve_connect_spec, with_runtime};

pub(crate) fn run_inbox(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: kastellan-cli inbox <list|show|resolve> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "list" => with_runtime("inbox", inbox_list(&args[1..])),
        "show" => with_runtime("inbox", inbox_show(&args[1..])),
        "resolve" => with_runtime("inbox", inbox_resolve(&args[1..])),
        other => {
            eprintln!("inbox: unknown subcommand {other}");
            ExitCode::from(2)
        }
    }
}
```

Then the three async arms, in the same file:

```rust
/// The two answers a `plan_approval` ask offers. Checked here so a typo
/// reads as a usage error (exit 2) rather than as a database refusal —
/// `db::asks::resolve` validates against the ask's own `options` too, and
/// that check is the authoritative one.
const CHOICES: [&str; 2] = ["approve", "deny"];

async fn inbox_list(args: &[String]) -> ExitCode {
    use kastellan_db::pool::connect_runtime_pool;

    let mut limit: i64 = 20;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" => {
                limit = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(20);
                i += 2;
            }
            other => {
                eprintln!("inbox list: unknown flag {other}");
                return ExitCode::from(2);
            }
        }
    }

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("connect: {e}"); return ExitCode::from(1); }
    };

    let asks = match kastellan_db::asks::list_pending(&pool, limit).await {
        Ok(a) => a,
        Err(e) => { eprintln!("inbox list: {e}"); return ExitCode::from(1); }
    };
    if asks.is_empty() {
        println!("no pending asks");
        return ExitCode::SUCCESS;
    }
    println!("{:>6}  {:>7}  {:<20}  {}", "ASK", "TASK", "DEADLINE", "QUESTION");
    for a in &asks {
        // The question is the whole point of an inbox — clamped, never
        // omitted. `chars()` not bytes: a multibyte question must not be
        // truncated mid-codepoint.
        let q: String = a.body.chars().take(80).collect();
        let ellipsis = if a.body.chars().count() > 80 { "…" } else { "" };
        println!("{:>6}  {:>7}  {:<20}  {q}{ellipsis}", a.id, a.task_id, a.deadline_at);
    }
    println!("\nanswer with: kastellan-cli inbox resolve <ASK> approve|deny [--note \"...\"]");
    ExitCode::SUCCESS
}

async fn inbox_show(args: &[String]) -> ExitCode {
    use kastellan_db::pool::connect_runtime_pool;

    let Some(ask_id) = args.first().and_then(|s| s.parse::<i64>().ok()) else {
        eprintln!("usage: kastellan-cli inbox show <ask-id>");
        return ExitCode::from(2);
    };
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("connect: {e}"); return ExitCode::from(1); }
    };
    let ask = match kastellan_db::asks::get(&pool, ask_id).await {
        Ok(Some(a)) => a,
        Ok(None) => { eprintln!("no ask with id {ask_id}"); return ExitCode::from(1); }
        Err(e) => { eprintln!("inbox show: {e}"); return ExitCode::from(1); }
    };
    // Every field `Ask` carries. There is no nonce field on it, deliberately
    // — the plaintext is returned once by `raise` and never stored.
    println!("ask         {}", ask.id);
    println!("task        {}", ask.task_id);
    println!("kind        {}", ask.kind);
    println!("state       {}", ask.state);
    println!("created     {}", ask.created_at);
    println!("deadline    {}", ask.deadline_at);
    println!("options     {}", ask.options);
    println!("plan digest {}", ask.plan_digest.as_deref().unwrap_or("-"));
    println!("resolved at {}", ask.resolved_at.map(|t| t.to_string()).unwrap_or_else(|| "-".into()));
    println!("resolved by {}", ask.resolved_by.as_deref().unwrap_or("-"));
    println!("resolution  {}", ask.resolution.as_ref().map(|r| r.to_string()).unwrap_or_else(|| "-".into()));
    println!("\nquestion:\n{}", ask.body);
    ExitCode::SUCCESS
}

async fn inbox_resolve(args: &[String]) -> ExitCode {
    use kastellan_db::pool::connect_runtime_pool;

    let Some(ask_id) = args.first().and_then(|s| s.parse::<i64>().ok()) else {
        eprintln!("usage: kastellan-cli inbox resolve <ask-id> approve|deny [--note \"<text>\"]");
        return ExitCode::from(2);
    };
    let Some(choice) = args.get(1) else {
        eprintln!("usage: kastellan-cli inbox resolve <ask-id> approve|deny [--note \"<text>\"]");
        return ExitCode::from(2);
    };
    if !CHOICES.contains(&choice.as_str()) {
        eprintln!("inbox resolve: choice must be 'approve' or 'deny', got {choice:?}");
        return ExitCode::from(2);
    }
    let mut note: Option<String> = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--note" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--note needs value");
                    return ExitCode::from(2);
                };
                note = Some(v.clone());
                i += 2;
            }
            other => {
                eprintln!("inbox resolve: unknown flag {other}");
                return ExitCode::from(2);
            }
        }
    }

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("connect: {e}"); return ExitCode::from(1); }
    };

    // Read the ask first for its `task_id`: every other `task.*` / `ask.*`
    // audit row is keyed on it, and a row without it cannot be joined to
    // the task the decision was about.
    let task_id = match kastellan_db::asks::get(&pool, ask_id).await {
        Ok(Some(a)) => a.task_id,
        Ok(None) => { eprintln!("no ask with id {ask_id}"); return ExitCode::from(1); }
        Err(e) => { eprintln!("inbox resolve: {e}"); return ExitCode::from(1); }
    };

    // Free text is carried for the record and shown to the operator; it is
    // never interpolated into a plan (spec D10).
    let resolution = match &note {
        Some(t) => serde_json::json!({"choice": choice, "free_text": t}),
        None => serde_json::json!({"choice": choice}),
    };
    let resolved_by = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "operator".to_string());

    match kastellan_db::asks::resolve(&pool, ask_id, &resolved_by, &resolution).await {
        Ok(true) => {
            println!("ask {ask_id} resolved '{choice}'; task {task_id} returned to the queue");
        }
        Ok(false) => {
            // NOT a success. First-responder-wins is a database property;
            // printing success here would tell the operator their answer
            // stood when someone else's did.
            eprintln!(
                "ask {ask_id} was not resolvable — already answered, expired, cancelled, \
                 or past its deadline. Nothing was written."
            );
            return ExitCode::from(1);
        }
        Err(e) => { eprintln!("inbox resolve: {e}"); return ExitCode::from(1); }
    }

    let payload = serde_json::json!({
        "ask_id": ask_id,
        "task_id": task_id,
        "choice": choice,
        "resolved_by": resolved_by,
        "free_text": note,
    });
    if let Err(e) =
        kastellan_db::audit::insert(&pool, CLI_AUDIT_ACTOR, ACTION_ASK_RESOLVED, payload).await
    {
        eprintln!("warning: ask.resolved audit row failed: {e}");
    }
    ExitCode::SUCCESS
}
```

Check `list_pending`'s exact return type and `Ask`'s field names against `db/src/asks.rs` before compiling — the printing code above assumes `Vec<Ask>` with the fields as decoded by `decode_ask_row`.

In `core/src/bin/kastellan-cli/main.rs`: add `mod inbox;` in the alphabetical module list, add `"inbox" => inbox::run_inbox(&args[2..]),` to the dispatch match (beside `"tasks"`), add the three usage lines to the module doc-comment block, and add them to `help_text()` in the same style as the neighbouring entries:

```
kastellan-cli inbox list                 [-n N]
kastellan-cli inbox show    <ask-id>
kastellan-cli inbox resolve <ask-id> approve|deny [--note "<text>"]
```

- [ ] **Step 4: Run the tests to verify they pass**

```sh
cargo test -p kastellan-core --test cli_inbox_e2e -- --nocapture
cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: 3 passed; clippy exit 0.

- [ ] **Step 5: Commit**

```sh
git add core/src/bin/kastellan-cli/inbox.rs core/src/bin/kastellan-cli/main.rs \
        core/tests/cli_inbox_e2e.rs
git commit -m "feat(cli): inbox — list, show and answer the asks the daemon raised

Named inbox rather than asks because 'kastellan-cli ask' already means
submit a task, and two subcommands one letter apart — one of which approves
a plan — is a trap for exactly the operator answering an escalation.

Resolves by row id via db::asks::resolve, the path that function's doc
reserves for a trusted local caller. An id has no unforgeability property;
anything reachable from an untrusted transport must use resolve_with_nonce,
which is slice 2's job.

A lost race exits 1 rather than printing success: first-responder-wins is
already a DB property, and what the test pins is that the CLI reports the
loss instead of papering over it.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 9: Correct the prose this slice falsified, and gate

Several doc comments assert that nothing calls slice 1a. They are now false, and "prose asserting what the code does not do" is the defect family this repo keeps paying for.

**Files:**
- Modify: `core/src/cassandra/plan_digest.rs:1-10` (the "NOT YET WIRED" banner)
- Modify: `db/src/tasks.rs:~655-660` (`resume_from_ask`'s deferred budget question)
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`

- [ ] **Step 1: Replace the `plan_digest` banner**

The current banner reads `⚠️ **NOT YET WIRED — this is the primitive, slice 1b is the caller.**` followed by a paragraph explaining that nothing computes a digest in production and that the escalate path does not exist. Replace it with:

```rust
//! **WIRED since #564 slice 1b.** `scheduler::asks::raise_and_suspend`
//! computes the digest for every escalated plan and stores it in
//! `asks.plan_digest`; the `Verdict::Escalate` arm compares a replan's
//! digest against a resolved approval via `scheduler::asks::decide`. The
//! rest of this doc describes live behaviour.
//!
//! The digest is taken from the plan **as it will execute** — after
//! `apply_floor_raise`, the `data_ceiling` resolution, invoke expansion and
//! namespace completion — because the replan runs the same normalisations
//! and the two must be comparable.
```

Delete the sentence telling the reader not to go looking for the escalate path; it now exists.

- [ ] **Step 2: Answer `resume_from_ask`'s deferred question**

Its doc ends with *"Whether that reset is fine … or wrong … is slice 1b's call, not made here."* Replace that closing sentence with the decision, keeping everything above it:

```rust
/// **Slice 1b made that call** (`runner::task_exec::resume_budget`): the
/// context is seeded from this column, so it is monotonic and no longer
/// rewinds, and `max_plans` is *extended* by the carried count rather than
/// the budget being either reset or spent-from. Spending from the original
/// budget would leave a task that escalated on its last allowed plan
/// resuming with none, making the operator's approval buy nothing.
```

- [ ] **Step 3: Run the full gate — both hosts**

```sh
source "$HOME/.cargo/env"
cd /Users/hherb/src/kastellan
CARGO_TARGET_DIR=target cargo test --workspace --no-fail-fast -- --nocapture \
  > "$HOME/slice1b-gate.log" 2>&1; echo "TEST_EXIT=$?" >> "$HOME/slice1b-gate.log"
```

Rules this repo has paid for:
- **Write the log under `$HOME`, never `/tmp`.** Include the `TEST_EXIT` line.
- **Use the repo's own `target/`.** A custom `CARGO_TARGET_DIR` makes the daemon-spawning e2e tests restart-loop, producing six failures that look like a regression and are not.
- **`--no-fail-fast`** or the total is a partial count.
- Predict the count first: baseline **3416** plus the exact number of new `#[test]` functions in the diff. Reconcile any miss rather than accepting it.
- Then clippy, and **count the `Checking` lines** — a warm target dir returns exit 0 having linted a handful of crates:

```sh
CARGO_TARGET_DIR="$HOME/.cache/kastellan-slice1b-clippy" \
  cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tee "$HOME/slice1b-clippy.log"
grep -c '^ *Checking' "$HOME/slice1b-clippy.log"
```

An honest full-workspace run is ~217-250 `Checking` lines, not 5.

Then run the same two commands on the DGX (`ssh dgx '<cmd>'` — exactly that form; flags before the hostname are denied). The DGX leg is not optional: the Mac compiles `#[cfg(target_os="linux")]` items out, and this slice touches `core` and `db`, both of which have cfg-linux test files.

- [ ] **Step 4: Run the mutations**

Apply each, confirm a test fails, revert it (**copy the file aside and copy it back — never `git checkout -- <file>`, which eats uncommitted edits in the same file**):

1. `resolution_choice`: return `Some(Choice::Approve)` from the malformed arm instead of `None`.
2. `decide`: drop the `ask.plan_digest.as_deref() == Some(plan_digest)` guard so any approval matches any plan.
3. `pre_plan_outcome`: return `None` unconditionally (the deny check is gone).
4. `drain_lane`: change the `let … else { continue }` to bind `"awaiting_operator"` and fall through to finalize.
5. `resume_budget`: return `(0, max_plans)` — the old reset.
6. `resume_budget`: drop the `carried +` from the extension.
7. The `Escalate` arm's `Err` branch: replace with `ctx.blocks.push(...)` + `continue` (the old degrade).
8. `latest_resolved_for_task`: drop `AND state = 'resolved'` so an expired ask reads as a decision.
9. `sweep_loop`: `return` instead of `warn!` on a sweep error.
10. `ask_deadline_seconds`: drop the `.filter(|n| *n > 0)`.

Any survivor means a fixture is too small to fail — the finding four sessions running. Fix the test, not the mutation.

- [ ] **Step 5: Update HANDOVER.md and ROADMAP.md, then commit**

HANDOVER: replace the Current-state lead with slice 1b; add the `5378c0af`-successor row to the test-baseline table with the measured count; move the #564 bullet in *Next TODO* to slice 2 (Matrix delivery + nonce correlation) and strike #571 as closed; record the durable findings (the deny-replan hole, the plan-count rewind, the startup-only-sweep gap). ROADMAP: tick slice 1b under the Phase 3 "Operator ask channel" entry with the PR hash, one terse line. Prune both to stay under ~500 lines.

```sh
git add core/src/cassandra/plan_digest.rs db/src/tasks.rs \
        docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs: the ask path is wired, and two deferred questions are answered

plan_digest's NOT-YET-WIRED banner and resume_from_ask's 'slice 1b's call,
not made here' were both true when written and are both false now. Prose
asserting what the code does not do is the defect family this repo keeps
paying for, so it gets corrected in the same PR that falsified it.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## Live verification (before the PR is called done)

The DGX is eval-only, so this needs no confirmation and transient downtime is fine.

1. Deploy the branch by hand — `bash scripts/build-release.sh` (**not** a bare `cargo build --release --workspace`; the matrix worker needs `--features live-matrix` or the channel crash-loops), then `./target/release/kastellan-cli install --matrix-homeserver-url <url> --matrix-user <user>` (the `./target/release/` one, **not** `~/.local/bin/kastellan-cli`, which copies the installed binaries onto themselves).
2. Submit a task the deterministic policy escalates. Confirm: `tasks list` shows `awaiting_operator`, `kastellan-cli inbox list` shows the question, and the daemon log carries the raise line with **no nonce in it**.
3. `kastellan-cli inbox resolve <id> approve` → the task resumes and completes. Check `audit tail` for `ask.raised` then `ask.resolved`, and that `plan_count` did not rewind.
4. Repeat with `deny` → the task ends `blocked` with a `denied` payload.
5. Set `KASTELLAN_ASK_DEADLINE_S=120` in `kastellan.env.local`, restart, escalate, and leave it: confirm the sweep expires it inside ~3 minutes and writes `ask.expired`.

A live run gates against regression; the e2e tests are the evidence for the new paths.
