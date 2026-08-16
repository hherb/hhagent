# Durable Ask Record (#564 slice 1a) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the durable, correlated, deadline-bounded ask record and the `awaiting_operator` task state, so a later slice can suspend a task on a human decision instead of degrading `Verdict::Escalate` to `Block`.

**Architecture:** One new table (`asks`) plus one new `tasks` state. All `tasks` writes stay inside `db::tasks` (that module's standing contract); `db::asks` calls into it rather than writing `tasks` SQL itself. Every multi-table operation is one transaction. Resolution uses the guarded `UPDATE … WHERE state='pending'` returning rows-affected — the same race-safe idiom `memories::set_embedding` uses — which is what buys resolved-exactly-once with no lock.

**Tech Stack:** Rust, sqlx 0.8 against Postgres, `sha2` for digests and nonce hashing, `rand`'s `OsRng` for nonce generation, `time::OffsetDateTime`.

**Spec:** [`docs/superpowers/specs/2026-08-16-ask-record-slice-1a-design.md`](../specs/2026-08-16-ask-record-slice-1a-design.md) — read it before Task 1; every decision below is argued there.

## Global Constraints

- **AGPL-compatible dependencies only.** `rand` (MIT/Apache-2.0) is already a workspace dependency; adding it to `db` needs no license review. Introduce nothing else.
- **`sqlx::migrate!` embeds at COMPILE time.** After creating `db/migrations/0023_asks.sql` you MUST `touch db/src/lib.rs` or the migration silently does not apply and every e2e fails with "relation asks does not exist". See [[sqlx-migrate-embeds-at-compile-time]].
- **Never edit an applied migration.** 0023 is new, so it is editable until it is committed and run anywhere; after that, add 0024.
- **All `tasks` writes go through `db::tasks`.** That module's header states it: *"All writes go through this module; the scheduler never builds raw SQL."* `db::asks` must not contain `UPDATE tasks`.
- **Cargo is not on the non-interactive PATH.** Every shell step starts from `source "$HOME/.cargo/env"`.
- **Mac cargo blocks on rust-analyzer's `target/debug/.cargo-lock`.** Use `CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate`, under `$HOME` and never `/tmp` (macOS scrubs `/tmp` mid-run). See [[mac-cargo-buildlock-prefer-dgx]] and [[dgx-run-logs-tmp-scrubbed]].
- **Files stay under 500 lines.** `db/src/asks.rs` is budgeted at ~300 production lines with its tests in a `db/src/asks/tests.rs` sibling if it grows past that.
- **Clippy is `-D warnings` and the tree is clean.** Suppression is debt; fix rather than `#[allow]`.
- **PG e2e on this Mac need `KASTELLAN_PG_BIN_DIR`, and then they really run.** `pg_bin_dir_or_skip()` returns `None` by default because `default_pg_bin_dir_candidates()` deliberately excludes the Postgres.app paths — that is an opt-in, not an absent capability. Export the override and the suite is a real local red-green loop:
  ```sh
  export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"   # v18, port 5532
  ```
  (v16 is at `/Applications/Postgres.app/Contents/Versions/16/bin`. Mind the space in both paths — always quote.) **Run this suite on its own, never as part of a full-workspace run under the override:** a full-workspace run flakes ~4 tests in `core/tests/embedding_recall_e2e.rs` at PG bring-up from parallel `initdb`/launchd churn (issue #130 territory). They pass single-threaded and in isolation. So: targeted suites under the override on the Mac, full workspace on the DGX.
- **A `[SKIP]` line is still a pass.** If you forget the override, every PG e2e in this plan prints `[SKIP]` and the run is green having verified nothing. Check for the skip line before believing a green — that is this repo's standing reading rule.

---

### Task 1: `plan_digest` — what an approval binds to

Pure, no Postgres, fast to iterate on the Mac. Defines the meaning of the `asks.plan_digest` column created in Task 2.

**Files:**
- Create: `core/src/cassandra/plan_digest.rs`
- Modify: `core/src/cassandra/mod.rs` (add `pub mod plan_digest;`)

**Interfaces:**
- Consumes: `crate::cassandra::types::{Plan, PlannedStep, DataClass}` (existing).
- Produces: `pub fn plan_digest(plan: &Plan) -> String` — lowercase hex SHA-256, 64 chars. Task 2's migration stores it; slice 1b's `Escalate` arm calls it.

- [ ] **Step 1: Write the failing tests**

Create `core/src/cassandra/plan_digest.rs` containing ONLY the test module for now (the `plan_digest` fn comes in Step 3), plus the imports it needs:

```rust
//! placeholder — real module doc lands in Step 3

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::{DataClass, Plan, PlannedStep};
    use serde_json::json;

    /// A two-step plan used as the baseline across these tests.
    fn base_plan() -> Plan {
        Plan {
            context: "the user asked about flights".into(),
            decision: "continue".into(),
            rationale: "search mail first, then read the hit".into(),
            steps: vec![
                PlannedStep {
                    tool: "mail".into(),
                    method: "mail.search".into(),
                    parameters: json!({"query": "Qantas", "sort": "date"}),
                    returns: "a list of hits".into(),
                    done_when: "hits are non-empty".into(),
                    classification: DataClass::Personal,
                },
                PlannedStep {
                    tool: "mail".into(),
                    method: "mail.get_message".into(),
                    parameters: json!({"message_id": "20973"}),
                    returns: "the message body".into(),
                    done_when: "body is present".into(),
                    classification: DataClass::Personal,
                },
            ],
            result: None,
            data_ceiling: Some(DataClass::Personal),
            refused: None,
            floor_request: None,
        }
    }

    #[test]
    fn identical_plans_digest_identically() {
        assert_eq!(plan_digest(&base_plan()), plan_digest(&base_plan()));
    }

    #[test]
    fn digest_is_64_lowercase_hex_chars() {
        let d = plan_digest(&base_plan());
        assert_eq!(d.len(), 64, "sha256 hex is 64 chars: {d}");
        assert!(
            d.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "digest must be lowercase hex: {d}",
        );
    }

    // ---- narration is EXCLUDED (spec D2) ----------------------------------
    //
    // These four fields are regenerated differently by the planner on every
    // call and none of them is read by `dispatch_step`. If they counted, the
    // digest would essentially never match on replan and the whole binding
    // would be decorative.

    #[test]
    fn plan_level_narration_does_not_change_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.context = "totally different framing of the same request".into();
        p.rationale = "a completely rewritten rationale".into();
        assert_eq!(plan_digest(&p), before, "context/rationale must not count");
    }

    #[test]
    fn step_level_narration_does_not_change_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.steps[0].returns = "some other prose".into();
        p.steps[0].done_when = "some other condition prose".into();
        assert_eq!(plan_digest(&p), before, "returns/done_when must not count");
    }

    // ---- the executable surface is INCLUDED -------------------------------
    //
    // One test per field, so a future refactor that drops a field from the
    // canonical form fails on exactly that field rather than on a blanket
    // assertion that names nothing.

    #[test]
    fn changing_a_step_tool_changes_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.steps[0].tool = "web-fetch".into();
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn changing_a_step_method_changes_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.steps[0].method = "mail.list_folders".into();
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn changing_a_step_parameter_value_changes_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.steps[0].parameters = json!({"query": "Jetstar", "sort": "date"});
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn changing_a_step_classification_changes_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.steps[0].classification = DataClass::Secret;
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn changing_the_data_ceiling_changes_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.data_ceiling = Some(DataClass::Secret);
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn an_absent_data_ceiling_differs_from_a_present_one() {
        // #506's lesson: absence is not a value. A plan that omits the
        // ceiling must not digest the same as one that declares the
        // floor-resolved value, or an approval could carry across the
        // resolution boundary.
        let mut p = base_plan();
        p.data_ceiling = None;
        assert_ne!(plan_digest(&p), plan_digest(&base_plan()));
    }

    #[test]
    fn changing_the_floor_request_changes_the_digest() {
        // `floor_request` feeds `apply_floor_raise`, which changes the
        // classification floor the whole plan is reviewed against — so a
        // plan that drops one is materially different from the plan that
        // was approved. This is the field an inclusion-list formulation
        // silently omitted; see spec D2.
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.floor_request = Some(DataClass::ClinicalConfidential);
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn changing_the_decision_changes_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.decision = "task_complete".into();
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn changing_a_terminal_plans_result_changes_the_digest() {
        // A terminal plan has no steps, so its `result` IS what the
        // operator would be approving. Excluding it would let an approval
        // carry from "your balance is X" to "your balance is Y".
        let mut a = base_plan();
        a.steps.clear();
        a.result = Some(json!({"kind": "text", "body": "answer one"}));
        let mut b = base_plan();
        b.steps.clear();
        b.result = Some(json!({"kind": "text", "body": "answer two"}));
        assert_ne!(plan_digest(&a), plan_digest(&b));
    }

    #[test]
    fn dropping_a_step_changes_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.steps.truncate(1);
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn reordering_steps_changes_the_digest() {
        // Step order is execution order, so it is part of what was approved:
        // "search then read" is not "read then search".
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.steps.swap(0, 1);
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn an_empty_step_list_digests_stably_and_differs_from_a_populated_one() {
        let mut p = base_plan();
        p.steps.clear();
        let mut q = base_plan();
        q.steps.clear();
        assert_eq!(plan_digest(&p), plan_digest(&q));
        assert_ne!(plan_digest(&p), plan_digest(&base_plan()));
    }

    // ---- canonicality ------------------------------------------------------

    #[test]
    fn parameter_key_insertion_order_does_not_change_the_digest() {
        // LOAD-BEARING, and it guards a Cargo feature rather than our code.
        // `serde_json::Map` is a BTreeMap only while the `preserve_order`
        // feature is OFF — which it is nowhere in this workspace. Enabling it
        // anywhere would make Map an IndexMap, object keys would serialize in
        // insertion order, and two logically identical plans would digest
        // differently — silently retiring every outstanding approval. This
        // test is the tripwire for that.
        let mut a = base_plan();
        a.steps[0].parameters = json!({"query": "Qantas", "sort": "date"});
        let mut b = base_plan();
        b.steps[0].parameters = json!({"sort": "date", "query": "Qantas"});
        assert_eq!(
            plan_digest(&a),
            plan_digest(&b),
            "object key order must not affect the digest — is serde_json's \
             `preserve_order` feature enabled somewhere in the workspace?",
        );
    }

    #[test]
    fn nested_parameter_key_order_does_not_change_the_digest() {
        let mut a = base_plan();
        a.steps[0].parameters = json!({"filters": {"account_ids": [1], "folder_ids": [2]}});
        let mut b = base_plan();
        b.steps[0].parameters = json!({"filters": {"folder_ids": [2], "account_ids": [1]}});
        assert_eq!(plan_digest(&a), plan_digest(&b));
    }

    #[test]
    fn array_order_inside_parameters_is_significant() {
        // Arrays are ordered data, unlike object keys. `[1,2]` and `[2,1]`
        // are different arguments and must not share an approval.
        let mut a = base_plan();
        a.steps[0].parameters = json!({"ids": [1, 2]});
        let mut b = base_plan();
        b.steps[0].parameters = json!({"ids": [2, 1]});
        assert_ne!(plan_digest(&a), plan_digest(&b));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate
cargo test -p kastellan-core --lib plan_digest:: 2>&1 | tail -20
```

Expected: FAIL to compile — `cannot find function 'plan_digest' in this scope`. A compile failure is the correct "red" here.

- [ ] **Step 3: Write the implementation**

Replace the placeholder doc line at the top of `core/src/cassandra/plan_digest.rs` with the module and keep the test module beneath it:

```rust
//! What an operator approval binds to (#564, spec D1/D2).
//!
//! When CASSANDRA escalates a plan, the ask records a **digest** of that
//! plan rather than the plan itself. On resume the agent replans from
//! scratch — `run_one` rebuilds `TaskContext` from the task payload with
//! `plan_count: 0`, so the escalated plan is gone — and goes through review
//! again as normal. If the new plan's digest matches the approved one, the
//! `Escalate` arm consults the resolved ask instead of raising a second one.
//! A *different* plan escalates afresh.
//!
//! That keeps "every plan is reviewed" intact with no bypass carve-out, and
//! closes the approve-plan-P-run-plan-P′ gap by construction.
//!
//! # What the digest covers, and why it is written as an exclusion
//!
//! **Excluded** — exactly four fields: plan-level `context` and
//! `rationale`, per-step `returns` and `done_when`. These are narration the
//! planner regenerates differently on every call, and none is read by
//! anything that acts: `dispatch_step` uses `tool`/`method`/`parameters`,
//! and `cassandra::deterministic` reads `classification` and
//! `data_ceiling`.
//!
//! **Included** — everything else, including fields whose relevance is not
//! obvious: `floor_request` (feeds `apply_floor_raise`, so it changes the
//! floor the whole plan is reviewed against), `result` (on a terminal plan
//! with no steps, the result IS what the operator approved), `decision`,
//! and `refused`.
//!
//! **Stating it as an exclusion list is load-bearing.** An earlier draft
//! named the included fields and had already silently dropped
//! `floor_request`. An inclusion list makes *forgetting* the failure mode,
//! and forgetting fails unsafely — an approval carrying to a plan that
//! differs in the forgotten field. As an exclusion list, a new `Plan` field
//! defaults to counted, so a future omission merely re-escalates a plan
//! that did not need it.
//!
//! The trade-off still cuts both ways. Digest everything including prose
//! and it never matches on replan, so approvals never carry and the binding
//! is decorative. Digest too little and an approval covers a plan that does
//! something else.
//!
//! ⚠️ **This selection is PROVISIONAL and has to prove itself in real use.**
//! The revisit trigger is the first real escalation that re-escalates on a
//! semantically identical replan — boundary too wide, and with an exclusion
//! list that is the expected direction to be wrong in. The opposite signal,
//! an approval carrying to a plan the operator would not recognise, is far
//! more serious. Whichever fires first, re-derive this list from what
//! `dispatch_step` and `cassandra::deterministic` read *at that time*, not
//! from this comment.
//!
//! # Canonicality
//!
//! The digest is SHA-256 over `serde_json`'s serialization of a reduced
//! `Value`. This is canonical **because `serde_json::Map` is a `BTreeMap`**
//! — the `preserve_order` feature is not enabled anywhere in this workspace
//! — so object keys serialize in sorted order regardless of how the planner
//! happened to emit them. `parameter_key_insertion_order_does_not_change_the_digest`
//! is the tripwire that fires if anyone ever turns that feature on.

use sha2::{Digest, Sha256};

use super::types::Plan;

/// Lowercase hex SHA-256 (64 chars) over the plan's executable surface.
///
/// See the module docs for exactly which fields count and why. Two plans
/// that would execute identically produce the same digest even if their
/// prose differs entirely.
pub fn plan_digest(plan: &Plan) -> String {
    // `to_vec` on a Value built from owned data cannot fail: there are no
    // non-string map keys and no NaN/Inf floats can reach here from a
    // parsed plan (serde_json rejects them at parse time). `expect` rather
    // than a silent fallback — a digest that quietly became a constant
    // would make every approval match every plan.
    let bytes = serde_json::to_vec(&canonical_form(plan))
        .expect("canonical_form yields plain JSON values, which always serialize");

    let mut h = Sha256::new();
    h.update(&bytes);
    let digest = h.finalize();

    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        write!(s, "{b:02x}").expect("write to String cannot fail");
    }
    s
}

/// Reduce a plan to what the digest covers: everything except the four
/// narration fields.
///
/// Written as an explicit construction rather than a serialize-then-delete
/// so the compiler names any new `Plan` field here — a missing field is a
/// compile error at the destructuring below, not a silent exclusion.
fn canonical_form(plan: &Plan) -> serde_json::Value {
    // Destructured, so adding a field to `Plan` fails to compile until
    // someone decides whether it counts. `..` is deliberately NOT used.
    let Plan {
        context: _,   // narration — excluded
        rationale: _, // narration — excluded
        decision,
        steps,
        result,
        data_ceiling,
        refused,
        floor_request,
    } = plan;

    let steps: Vec<serde_json::Value> = steps
        .iter()
        .map(|s| {
            // Same treatment for PlannedStep.
            let crate::cassandra::types::PlannedStep {
                tool,
                method,
                parameters,
                returns: _,   // narration — excluded
                done_when: _, // narration — excluded
                classification,
            } = s;
            serde_json::json!({
                "tool":           tool,
                "method":         method,
                "parameters":     parameters,
                "classification": classification,
            })
        })
        .collect();

    serde_json::json!({
        // Every Option serializes to `null` when absent, deliberately:
        // absence must not digest the same as any present value (#506's
        // "absence is not a value" lesson, applied here to `data_ceiling`
        // and `floor_request` alike).
        "decision":      decision,
        "steps":         steps,
        "result":        result,
        "data_ceiling":  data_ceiling,
        "refused":       refused,
        "floor_request": floor_request,
    })
}
```

Then add to `core/src/cassandra/mod.rs`, alphabetically among the existing `pub mod` lines:

```rust
pub mod plan_digest;
```

- [ ] **Step 4: Run the tests to verify they pass**

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate
cargo test -p kastellan-core --lib plan_digest:: 2>&1 | tail -20
```

Expected: PASS, 24 tests (19 as listed, plus the five per-field tests added in fix round 1). If `refused` or another `Plan` field is missing from `base_plan()`, the compile error names it — add it as `None` rather than changing the digest.

- [ ] **Step 5: Clippy, then commit**

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate
cargo clippy -p kastellan-core --lib -- -D warnings 2>&1 | tail -5
git add core/src/cassandra/plan_digest.rs core/src/cassandra/mod.rs
git commit -m "feat(cassandra): plan_digest — what an operator approval binds to

Pure SHA-256 over a plan's EXECUTABLE surface (per step tool / method /
parameters / classification, plus data_ceiling), excluding the narration
the planner regenerates every call (context, rationale, returns,
done_when) — none of which dispatch_step or deterministic reads.

Digest the whole plan and it never matches on replan, so approvals never
carry; digest too little and an approval covers a plan that does
something else. Marked provisional in the module doc with an explicit
revisit trigger.

Canonicality rests on serde_json::Map being a BTreeMap, i.e. on the
preserve_order feature staying off workspace-wide. That is guarded by a
test, not by a comment.

Refs #564"
```

---

### Task 2: Migration 0023 — the `asks` table and the `awaiting_operator` state

**Files:**
- Create: `db/migrations/0023_asks.sql`
- Create: `db/tests/asks_e2e.rs`
- Modify: `db/src/lib.rs` (touch only — see the constraint)

**Interfaces:**
- Consumes: the existing `tasks` table and `kastellan_runtime` role.
- Produces: table `asks`; `tasks.state` accepting `'awaiting_operator'`; the `tasks_resumed` NOTIFY channel. Tasks 3–7 all read and write these.

- [ ] **Step 1: Write the failing schema e2e**

Create `db/tests/asks_e2e.rs`:

```rust
//! PG-gated e2e for `db::asks` and the `awaiting_operator` task state
//! (migration 0023, issue #564 slice 1a). Skip-as-pass without a
//! supervisor/PG (Mac without a cluster, root CI container); live on the DGX.

use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

#[test]
fn asks_schema_and_task_state_round_trip() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "asks-d",
        "asks-l",
        &format!("kastellan-supervisor-test-pg-asks-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "asks-e2e"}),
        )
        .await
        .expect("probe run");

        let pool = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await
            .expect("admin pool");

        // The table exists and is empty.
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM asks")
            .fetch_one(&pool)
            .await
            .expect("asks table must exist after migration 0023");
        assert_eq!(n, 0);

        // `tasks_state_check` accepts the new suspended state.
        let task_id = kastellan_db::tasks::insert_pending(
            &pool,
            kastellan_db::tasks::Lane::Fast,
            serde_json::json!({"instruction": "schema probe"}),
        )
        .await
        .expect("insert pending");

        sqlx::query("UPDATE tasks SET state = 'awaiting_operator' WHERE id = $1")
            .bind(task_id)
            .execute(&pool)
            .await
            .expect("tasks_state_check must accept 'awaiting_operator'");

        let state = kastellan_db::tasks::observe_state(&pool, task_id)
            .await
            .expect("observe_state");
        assert_eq!(state, "awaiting_operator");

        // The ask state CHECK rejects a value outside the closed set.
        let bad = sqlx::query(
            "INSERT INTO asks (task_id, kind, body, options, nonce_sha256, deadline_at, state) \
             VALUES ($1, 'plan_approval', 'b', '[]'::jsonb, 'h', now() + interval '1 hour', 'bogus')",
        )
        .bind(task_id)
        .execute(&pool)
        .await;
        assert!(bad.is_err(), "asks.state CHECK must reject an unknown state");
    });
}
```

- [ ] **Step 2: Run it to verify it fails**

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
cargo test -p kastellan-db --test asks_e2e -- --nocapture 2>&1 | tail -20
```

Expected: FAIL with `relation "asks" does not exist`.

If you instead see a `[SKIP]` line and a pass, `KASTELLAN_PG_BIN_DIR` is not set (or points at a bin dir without `initdb`) — fix that before continuing, because every remaining task in this plan would otherwise "pass" without running.

- [ ] **Step 3: Write the migration**

Create `db/migrations/0023_asks.sql`:

```sql
-- 0023_asks.sql
--
-- The durable ask record (#564 slice 1a): a correlated, deadline-bounded
-- question the daemon raises for a human, plus the `awaiting_operator`
-- state a task sits in while one is outstanding.
--
-- Design: docs/superpowers/specs/2026-08-16-ask-record-slice-1a-design.md
--
-- Three parts:
--   1. the `asks` table + its two indexes
--   2. `tasks_state_check` widened with 'awaiting_operator'
--   3. the `tasks_resumed` NOTIFY trigger (awaiting_operator -> pending)
--
-- `notify_task_completed` (0005, last replaced in 0012) is deliberately
-- NOT touched. 'awaiting_operator' is not terminal, so it must not appear
-- in that function's NEW.state list — and because it is also absent from
-- the OLD.state list, an expiry transition awaiting_operator -> failed
-- still fires `tasks_completed` exactly as it should.

BEGIN;

-- (1) The record. `nonce_sha256` is a HASH, never the nonce: the plaintext
-- is returned to the caller once by `db::asks::raise` and never stored, so
-- a DB read cannot recover a live token. Same posture as
-- `pairing_codes.code_sha256` in 0018.
--
-- `plan_digest` is nullable because it is meaningful only for kinds that
-- bind to a plan ('plan_approval' today). A future 'ask_user' kind binds to
-- no plan and stores NULL.
--
-- `resolution` is a CLOSED set: {choice} indexing into `options`, plus an
-- optional free_text kept for the audit row and shown to the operator.
-- Free text is never interpolated into a plan — otherwise the ask channel
-- becomes an injection funnel aimed at the reviewer's own decision.
CREATE TABLE asks (
    id            BIGSERIAL   PRIMARY KEY,
    task_id       BIGINT      NOT NULL REFERENCES tasks(id),
    kind          TEXT        NOT NULL,
    body          TEXT        NOT NULL,
    options       JSONB       NOT NULL,
    plan_digest   TEXT,
    nonce_sha256  TEXT        NOT NULL,
    state         TEXT        NOT NULL DEFAULT 'pending'
                  CHECK (state IN ('pending','resolved','expired','cancelled')),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    deadline_at   TIMESTAMPTZ NOT NULL,
    resolved_at   TIMESTAMPTZ,
    resolved_by   TEXT,
    resolution    JSONB
);

-- Partial index: the expiry sweep only ever scans pending rows. Mirrors
-- `pairing_codes_claimable` from 0018.
CREATE INDEX asks_pending_deadline ON asks (deadline_at) WHERE state = 'pending';
-- Every read from the task side ("does this task have an open ask?").
CREATE INDEX asks_task ON asks (task_id);

-- (2) The suspended task state.
ALTER TABLE tasks DROP CONSTRAINT tasks_state_check;
ALTER TABLE tasks
    ADD CONSTRAINT tasks_state_check CHECK (state IN
        ('pending','running','completed','failed','cancelled',
         'blocked','timed_out','crashed','refused','awaiting_operator'));

-- (3) Resume wake-up. `tasks_inserted` fires AFTER INSERT only, so an
-- awaiting_operator -> pending UPDATE wakes nobody and the resumed task
-- waits out the lane runner's 30 s HEARTBEAT. A dedicated channel rather
-- than overloading `tasks_inserted`: a channel name that no longer
-- describes what it carries is the trap that broke upgrade_from_git.sh's
-- own post-deploy check in the #516 arc.
CREATE OR REPLACE FUNCTION notify_task_resumed()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF NEW.state = 'pending' AND OLD.state = 'awaiting_operator' THEN
        PERFORM pg_notify('tasks_resumed', NEW.id::text);
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS tasks_notify_resumed ON tasks;
CREATE TRIGGER tasks_notify_resumed
    AFTER UPDATE OF state ON tasks FOR EACH ROW
    EXECUTE FUNCTION notify_task_resumed();

-- (4) Grants. No DELETE: an ask transitions through terminal states and
-- stays, mirroring the append-only-by-GRANT posture `tasks` and
-- `audit_log` already take.
GRANT  SELECT, INSERT, UPDATE ON asks TO kastellan_runtime;
GRANT  USAGE, SELECT ON SEQUENCE asks_id_seq TO kastellan_runtime;
REVOKE DELETE, TRUNCATE ON asks FROM kastellan_runtime;

COMMIT;
```

- [ ] **Step 4: Force the migration to re-embed, then run the test**

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate
touch db/src/lib.rs          # REQUIRED: sqlx::migrate! embeds at compile time
cargo test -p kastellan-db --test asks_e2e -- --nocapture 2>&1 | tail -20
```

Expected: PASS (or `[SKIP]` on a PG-less host).

- [ ] **Step 5: Commit**

```sh
git add db/migrations/0023_asks.sql db/tests/asks_e2e.rs
git commit -m "feat(db): migration 0023 — the asks table and awaiting_operator

The durable ask record plus the task state a suspended task sits in.
nonce_sha256 is a hash, never the nonce. A dedicated tasks_resumed
channel rather than overloading tasks_inserted, whose AFTER INSERT
trigger cannot see a state UPDATE at all.

notify_task_completed is deliberately untouched: awaiting_operator is
not terminal, and its absence from the OLD.state list is what keeps the
expiry transition awaiting_operator -> failed firing tasks_completed.

Refs #564"
```

---

### Task 3: Suspend — `tasks::suspend_for_ask` and `asks::raise`

**Files:**
- Create: `db/src/asks.rs`
- Modify: `db/src/lib.rs` (add `pub mod asks;`)
- Modify: `db/Cargo.toml` (add `rand = { workspace = true }`)
- Modify: `db/src/tasks.rs` (add `suspend_for_ask`)
- Modify: `db/tests/asks_e2e.rs` (add the raise test)

**Interfaces:**
- Consumes: Task 2's schema.
- Produces:
  - `db::tasks::suspend_for_ask<'e, E>(executor: E, task_id: i64) -> Result<bool, DbError>`
  - `db::asks::Ask` (struct), `db::asks::RaisedAsk { ask_id: i64, nonce: String }`
  - `db::asks::raise(pool, task_id, kind, body, options, plan_digest, deadline_at) -> Result<RaisedAsk, DbError>`
  - `db::asks::get(pool, ask_id) -> Result<Option<Ask>, DbError>`
  - `db::asks::sha256_hex(input: &str) -> String`

- [ ] **Step 1: Write the failing test**

Append to `db/tests/asks_e2e.rs`:

```rust
#[test]
fn raise_suspends_the_task_and_releases_the_lease() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "askr-d",
        "askr-l",
        &format!("kastellan-supervisor-test-pg-askr-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        use kastellan_db::{asks, tasks};
        use kastellan_db::tasks::Lane;

        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "asks-raise-e2e"}),
        )
        .await
        .expect("probe run");
        let pool = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await
            .expect("admin pool");

        let task_id = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "raise probe"}),
        ).await.expect("insert");

        // An ask may only be raised against a RUNNING task.
        let too_early = asks::raise(
            &pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve", "deny"]), Some("digest0"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(60),
        ).await;
        assert!(
            too_early.is_err(),
            "raising against a pending task must fail rather than orphan an ask",
        );
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM asks")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "the failed raise must not have left a row behind");

        // Claim it so it is running with a lease.
        let claimed = tasks::claim_one(&pool, Lane::Fast, 60)
            .await.expect("claim").expect("a pending task");
        assert_eq!(claimed.id, task_id);
        assert!(claimed.lease_expires_at.is_some(), "claim sets a lease");

        let raised = asks::raise(
            &pool, task_id, "plan_approval", "approve this plan?",
            &serde_json::json!(["approve", "deny"]), Some("digest1"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(60),
        ).await.expect("raise");

        // The task is suspended AND its lease is released. The lease matters:
        // a suspended task holding one is a lie to `any_live_worker`.
        let after = tasks::get(&pool, task_id).await.unwrap().unwrap();
        assert_eq!(after.state, "awaiting_operator");
        assert!(
            after.lease_expires_at.is_none(),
            "raise must release the lease, got {:?}", after.lease_expires_at,
        );
        assert!(!tasks::any_live_worker(&pool).await.unwrap());

        // The nonce is returned in plaintext exactly once and stored hashed.
        assert_eq!(raised.nonce.len(), 64, "32 bytes hex-encoded");
        let stored: String = sqlx::query_scalar("SELECT nonce_sha256 FROM asks WHERE id = $1")
            .bind(raised.ask_id).fetch_one(&pool).await.unwrap();
        assert_eq!(stored, asks::sha256_hex(&raised.nonce));
        assert_ne!(stored, raised.nonce, "the plaintext nonce must not be stored");

        // Two raises never mint the same nonce.
        let t2 = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "second"}),
        ).await.unwrap();
        tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised2 = asks::raise(
            &pool, t2, "plan_approval", "b", &serde_json::json!(["approve"]), None,
            time::OffsetDateTime::now_utc() + time::Duration::seconds(60),
        ).await.expect("raise 2");
        assert_ne!(raised.nonce, raised2.nonce);

        // The decoded row carries what was written — and no nonce field.
        let got = asks::get(&pool, raised.ask_id).await.unwrap().unwrap();
        assert_eq!(got.task_id, task_id);
        assert_eq!(got.kind, "plan_approval");
        assert_eq!(got.state, "pending");
        assert_eq!(got.plan_digest.as_deref(), Some("digest1"));
        assert!(got.resolved_at.is_none());

        // A suspended task is invisible to the lane runner and the sweep.
        assert!(
            tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().is_none(),
            "claim_one must never return an awaiting_operator task",
        );
        sqlx::query("UPDATE tasks SET lease_expires_at = now() - interval '1 hour' WHERE id = $1")
            .bind(task_id).execute(&pool).await.unwrap();
        let swept = tasks::sweep_crashed(&pool).await.unwrap();
        assert!(
            !swept.iter().any(|t| t.id == task_id),
            "sweep_crashed must never reap an awaiting_operator task",
        );
        assert_eq!(tasks::observe_state(&pool, task_id).await.unwrap(), "awaiting_operator");
    });
}
```

- [ ] **Step 2: Run it to verify it fails**

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
cargo test -p kastellan-db --test asks_e2e raise_suspends -- --nocapture 2>&1 | tail -20
```

Expected: FAIL to compile — `could not find 'asks' in 'kastellan_db'`.

- [ ] **Step 3: Add the dependency and `tasks::suspend_for_ask`**

In `db/Cargo.toml`, in `[dependencies]` after the `aes-gcm` block:

```toml
# `OsRng` for ask correlation nonces (`asks::raise`). The nonce is a
# security token matched against an untrusted inbound message, so it uses
# the OS CSPRNG directly — same choice `core::secrets::vault` makes for
# secret refs.
rand = { workspace = true }
```

In `db/src/tasks.rs`, after `increment_plan_count`:

```rust
/// Suspend a `running` task while an operator ask is outstanding
/// (#564). Sets `state = 'awaiting_operator'` and **releases the lease**.
///
/// Returns `true` iff a row moved. `false` means the task was not
/// `running` — already terminal, cancelled out from under the caller, or
/// never claimed — and the caller must treat that as a refusal to
/// suspend, not as success.
///
/// Releasing the lease is load-bearing, not tidiness. `any_live_worker`
/// counts `running` rows with an unexpired lease as evidence a daemon is
/// alive and consuming a lane; a suspended task that kept its lease would
/// make a completely idle daemon look busy to `memory l3 run`'s
/// busy-vs-absent discrimination. (The crash sweep is a separate story
/// and needs no help here: `sweep_crashed` filters `state = 'running'`,
/// so a suspended task is already outside it.)
///
/// Executor-generic so `asks::raise` can call it inside its transaction —
/// the ask INSERT and this UPDATE must commit together.
pub async fn suspend_for_ask<'e, E>(executor: E, task_id: i64) -> Result<bool, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let r = sqlx::query(
        "UPDATE tasks \
         SET state = 'awaiting_operator', \
             lease_expires_at = NULL, \
             updated_at = now() \
         WHERE id = $1 AND state = 'running'",
    )
    .bind(task_id)
    .execute(executor)
    .await
    .map_err(|e| DbError::Query(format!("tasks suspend_for_ask: {e}")))?;
    Ok(r.rows_affected() == 1)
}
```

- [ ] **Step 4: Write `db/src/asks.rs`**

```rust
//! Typed CRUD against the `asks` table (#564 slice 1a).
//!
//! An **ask** is a durable, correlated, deadline-bounded question the
//! daemon raises for a human. It exists because CASSANDRA's
//! `Verdict::Escalate` — "a human must decide this" — had nowhere to go:
//! `channel/bus.rs` is strictly inbound-message → task → outbound-reply,
//! with no way for core to initiate a question and suspend a task on the
//! answer. See
//! `docs/superpowers/specs/2026-08-16-ask-record-slice-1a-design.md`.
//!
//! # Invariants this module maintains
//!
//! * **An ask and its task move together.** Every operation that changes
//!   an ask's state also moves its task, in ONE transaction. A resolved
//!   ask whose task never resumed is a wedged task; an expired ask whose
//!   task stayed suspended is the same bug.
//! * **`tasks` writes go through [`crate::tasks`].** That module owns all
//!   `tasks` SQL; this one calls its executor-generic helpers inside its
//!   own transactions rather than writing `UPDATE tasks` here.
//! * **Resolution happens exactly once.** [`resolve`] guards on
//!   `state = 'pending'` and reports rows-affected, so the first responder
//!   wins and every later one is told it lost — no lock, same idiom as
//!   `memories::set_embedding`.
//! * **The nonce is never readable.** Only its hash is stored, and
//!   [`Ask`] deliberately has no nonce field. Slice 2 matches an inbound
//!   nonce with a `WHERE nonce_sha256 = $1` predicate, never by reading
//!   the stored value out and comparing in Rust.

use sha2::{Digest, Sha256};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};
use time::OffsetDateTime;

use crate::tasks;
use crate::DbError;

/// Entropy in an ask nonce. 32 bytes → 64 hex chars, the same width as
/// the SHA-256 it is stored under, and far past guessing range for a
/// token that gates someone else's approval.
const NONCE_BYTES: usize = 32;

/// One decoded `asks` row.
///
/// **No nonce field, deliberately.** The plaintext is returned once by
/// [`raise`] and never stored; the hash stays in SQL where the only thing
/// that needs it is a `WHERE` predicate. A struct field would invite a
/// Rust-side comparison, and that is how timing-unsafe token checks get
/// written.
#[derive(Clone, Debug)]
pub struct Ask {
    pub id: i64,
    pub task_id: i64,
    pub kind: String,
    pub body: String,
    pub options: serde_json::Value,
    pub plan_digest: Option<String>,
    pub state: String,
    pub created_at: OffsetDateTime,
    pub deadline_at: OffsetDateTime,
    pub resolved_at: Option<OffsetDateTime>,
    pub resolved_by: Option<String>,
    pub resolution: Option<serde_json::Value>,
}

/// What [`raise`] hands back: the row id, and the correlation nonce **in
/// plaintext, exactly once**. Nothing persists the plaintext — if the
/// caller drops it, the ask can never be resolved through a nonce-bearing
/// transport again and must be expired or cancelled.
#[derive(Clone, Debug)]
pub struct RaisedAsk {
    pub ask_id: i64,
    pub nonce: String,
}

/// One row [`expire_due`] retired, for the caller's audit emission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpiredAsk {
    pub ask_id: i64,
    pub task_id: i64,
}

/// Lowercase hex SHA-256. Public because callers matching an inbound
/// nonce need to hash it the same way this module did.
pub fn sha256_hex(input: &str) -> String {
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        write!(s, "{b:02x}").expect("write to String cannot fail");
    }
    s
}

/// Mint a fresh correlation nonce as lowercase hex.
///
/// `OsRng` rather than `thread_rng`: this token is matched against an
/// untrusted inbound message and is the only thing standing between a
/// peer and someone else's approval, so it takes the OS CSPRNG directly
/// — the same choice `core::secrets::vault` makes for secret refs.
fn generate_nonce() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; NONCE_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut s = String::with_capacity(NONCE_BYTES * 2);
    for b in bytes {
        use std::fmt::Write;
        write!(s, "{b:02x}").expect("write to String cannot fail");
    }
    s
}

fn decode_ask_row(row: &PgRow) -> Result<Ask, DbError> {
    Ok(Ask {
        id: row.try_get("id")
            .map_err(|e| DbError::Query(format!("decode asks.id: {e}")))?,
        task_id: row.try_get("task_id")
            .map_err(|e| DbError::Query(format!("decode asks.task_id: {e}")))?,
        kind: row.try_get("kind")
            .map_err(|e| DbError::Query(format!("decode asks.kind: {e}")))?,
        body: row.try_get("body")
            .map_err(|e| DbError::Query(format!("decode asks.body: {e}")))?,
        options: row.try_get("options")
            .map_err(|e| DbError::Query(format!("decode asks.options: {e}")))?,
        plan_digest: row.try_get("plan_digest")
            .map_err(|e| DbError::Query(format!("decode asks.plan_digest: {e}")))?,
        state: row.try_get("state")
            .map_err(|e| DbError::Query(format!("decode asks.state: {e}")))?,
        created_at: row.try_get("created_at")
            .map_err(|e| DbError::Query(format!("decode asks.created_at: {e}")))?,
        deadline_at: row.try_get("deadline_at")
            .map_err(|e| DbError::Query(format!("decode asks.deadline_at: {e}")))?,
        resolved_at: row.try_get("resolved_at")
            .map_err(|e| DbError::Query(format!("decode asks.resolved_at: {e}")))?,
        resolved_by: row.try_get("resolved_by")
            .map_err(|e| DbError::Query(format!("decode asks.resolved_by: {e}")))?,
        resolution: row.try_get("resolution")
            .map_err(|e| DbError::Query(format!("decode asks.resolution: {e}")))?,
    })
}

const ASK_COLUMNS: &str = "id, task_id, kind, body, options, plan_digest, state, \
                           created_at, deadline_at, resolved_at, resolved_by, resolution";

/// Raise an ask against a **running** task and suspend that task.
///
/// One transaction: the task is suspended first (its `state = 'running'`
/// guard is what makes the whole operation conditional on the task being
/// ours to suspend), then the row is inserted. Ordering them the other way
/// would leave an orphan ask behind whenever the guard fails.
///
/// Errors — rather than returning an `Option` — when the task is not
/// `running`. There is no benign reading of that: the caller believed it
/// held a claimed task and did not, so a silent `None` would let a plan
/// proceed as though the human had been asked.
///
/// `plan_digest` is `Some` for kinds that bind to a plan and `None`
/// otherwise; see `core::cassandra::plan_digest` for what the value means.
pub async fn raise(
    pool: &PgPool,
    task_id: i64,
    kind: &str,
    body: &str,
    options: &serde_json::Value,
    plan_digest: Option<&str>,
    deadline_at: OffsetDateTime,
) -> Result<RaisedAsk, DbError> {
    let nonce = generate_nonce();
    let nonce_hash = sha256_hex(&nonce);

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| DbError::Query(format!("asks raise begin: {e}")))?;

    if !tasks::suspend_for_ask(&mut *tx, task_id).await? {
        // Dropping `tx` rolls back. Nothing was written.
        return Err(DbError::Other(format!(
            "asks raise: task {task_id} is not running, so it cannot be suspended \
             for an ask (already terminal, cancelled, or never claimed)"
        )));
    }

    let row = sqlx::query(
        "INSERT INTO asks (task_id, kind, body, options, plan_digest, nonce_sha256, deadline_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         RETURNING id",
    )
    .bind(task_id)
    .bind(kind)
    .bind(body)
    .bind(options)
    .bind(plan_digest)
    .bind(&nonce_hash)
    .bind(deadline_at)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| DbError::Query(format!("asks raise insert: {e}")))?;

    let ask_id: i64 = row
        .try_get("id")
        .map_err(|e| DbError::Query(format!("decode asks.id: {e}")))?;

    tx.commit()
        .await
        .map_err(|e| DbError::Query(format!("asks raise commit: {e}")))?;

    Ok(RaisedAsk { ask_id, nonce })
}

/// Fetch one ask by id, in any state.
pub async fn get(pool: &PgPool, ask_id: i64) -> Result<Option<Ask>, DbError> {
    let row = sqlx::query(&format!("SELECT {ASK_COLUMNS} FROM asks WHERE id = $1"))
        .bind(ask_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| DbError::Query(format!("asks get: {e}")))?;
    row.as_ref().map(decode_ask_row).transpose()
}
```

In `db/src/lib.rs`, add `pub mod asks;` alphabetically (before `pub mod audit;`).

- [ ] **Step 5: Run the test to verify it passes**

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
cargo test -p kastellan-db --test asks_e2e -- --nocapture 2>&1 | tail -20
cargo clippy -p kastellan-db --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: both tests PASS (or `[SKIP]`), clippy exit 0.

- [ ] **Step 6: Commit**

```sh
git add db/Cargo.toml db/src/asks.rs db/src/lib.rs db/src/tasks.rs db/tests/asks_e2e.rs
git commit -m "feat(db): asks::raise + tasks::suspend_for_ask

Raising an ask suspends its task and releases the lease, in one
transaction, with the task suspended FIRST so a failed state guard
cannot leave an orphan ask behind. Not-running is an Err, not a None:
a silent None would let a plan proceed as though the human had been
asked.

The lease release is load-bearing — any_live_worker treats a leased
running row as 'a daemon is consuming a lane', so a suspended task
holding one makes an idle daemon look busy. (sweep_crashed needs no
help; it already filters state='running'.)

Ask has no nonce field by design: the plaintext is returned once, the
hash stays in SQL where only a WHERE predicate touches it.

Refs #564"
```

---

### Task 4: Resolve — exactly-once, first-responder-wins

**Files:**
- Modify: `db/src/tasks.rs` (add `resume_from_ask`)
- Modify: `db/src/asks.rs` (add `resolve`)
- Modify: `db/tests/asks_e2e.rs` (add the resolve test)

**Interfaces:**
- Consumes: Task 3's `raise`, `get`.
- Produces:
  - `db::tasks::resume_from_ask<'e, E>(executor: E, task_id: i64) -> Result<bool, DbError>`
  - `db::asks::resolve(pool, ask_id, resolved_by: &str, resolution: &serde_json::Value) -> Result<bool, DbError>`

- [ ] **Step 1: Write the failing test**

Append to `db/tests/asks_e2e.rs`:

```rust
#[test]
fn resolve_is_exactly_once_and_re_enqueues_the_task() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "askv-d",
        "askv-l",
        &format!("kastellan-supervisor-test-pg-askv-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        use kastellan_db::{asks, tasks};
        use kastellan_db::tasks::Lane;

        kastellan_db::probe::run(
            &cluster.conn_spec, "core", "startup",
            serde_json::json!({"version": "test", "purpose": "asks-resolve-e2e"}),
        ).await.expect("probe run");
        let pool = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await.expect("admin pool");

        let task_id = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "resolve probe"}),
        ).await.unwrap();
        tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            &pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve", "deny"]), Some("digest1"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        ).await.unwrap();

        // First responder wins.
        let won = asks::resolve(
            &pool, raised.ask_id, "matrix/@horst:kastellan.dev",
            &serde_json::json!({"choice": "approve"}),
        ).await.unwrap();
        assert!(won, "the first resolve must win");

        // The task is back in the queue, claimable again.
        assert_eq!(tasks::observe_state(&pool, task_id).await.unwrap(), "pending");
        let reclaimed = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap();
        assert_eq!(reclaimed.map(|t| t.id), Some(task_id));

        // The ask carries who decided and what they chose.
        let got = asks::get(&pool, raised.ask_id).await.unwrap().unwrap();
        assert_eq!(got.state, "resolved");
        assert_eq!(got.resolved_by.as_deref(), Some("matrix/@horst:kastellan.dev"));
        assert_eq!(got.resolution, Some(serde_json::json!({"choice": "approve"})));
        assert!(got.resolved_at.is_some());

        // THE property: a second resolve loses, and changes nothing. Without
        // the `AND state='pending'` guard this would overwrite the first
        // decision and re-enqueue a task that is already running.
        let lost = asks::resolve(
            &pool, raised.ask_id, "matrix/@someone-else:evil.example",
            &serde_json::json!({"choice": "deny"}),
        ).await.unwrap();
        assert!(!lost, "the second resolve must lose");

        let after = asks::get(&pool, raised.ask_id).await.unwrap().unwrap();
        assert_eq!(after.resolved_by.as_deref(), Some("matrix/@horst:kastellan.dev"),
            "a losing resolve must not overwrite the winner");
        assert_eq!(after.resolution, Some(serde_json::json!({"choice": "approve"})));
        assert_eq!(tasks::observe_state(&pool, task_id).await.unwrap(), "running",
            "a losing resolve must not re-enqueue the already-reclaimed task");

        // An unknown ask id is a loss, not an error.
        assert!(!asks::resolve(
            &pool, 999_999, "operator/cli", &serde_json::json!({"choice": "approve"}),
        ).await.unwrap());
    });
}
```

- [ ] **Step 2: Run it to verify it fails**

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
cargo test -p kastellan-db --test asks_e2e resolve_is_exactly_once -- --nocapture 2>&1 | tail -20
```

Expected: FAIL to compile — no function `resolve` in `asks`.

- [ ] **Step 3: Implement**

In `db/src/tasks.rs`, after `suspend_for_ask`:

```rust
/// Return a suspended task to the queue after its ask resolved (#564).
///
/// Guarded on `awaiting_operator` so it cannot resurrect a task that was
/// cancelled or expired while the ask was outstanding. Returns `true` iff
/// a row moved.
///
/// The `tasks_notify_resumed` trigger fires `pg_notify('tasks_resumed', id)`
/// on this transition, which is what wakes the lane runner immediately
/// rather than at its next 30 s heartbeat.
///
/// `started_at` and `plan_count` are deliberately left alone: the resumed
/// run is a continuation of the same task, and `plan_count` is the
/// plans-so-far counter the CLI shows.
///
/// Executor-generic so `asks::resolve` can call it inside its transaction.
pub async fn resume_from_ask<'e, E>(executor: E, task_id: i64) -> Result<bool, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let r = sqlx::query(
        "UPDATE tasks \
         SET state = 'pending', \
             updated_at = now() \
         WHERE id = $1 AND state = 'awaiting_operator'",
    )
    .bind(task_id)
    .execute(executor)
    .await
    .map_err(|e| DbError::Query(format!("tasks resume_from_ask: {e}")))?;
    Ok(r.rows_affected() == 1)
}
```

In `db/src/asks.rs`, after `raise`:

```rust
/// Resolve a pending ask and return its task to the queue.
///
/// Returns `true` iff **this** call resolved it. `false` means the ask was
/// not `pending` — already resolved by someone else, expired, cancelled,
/// or absent — and nothing was written.
///
/// The guard is `WHERE id = $1 AND state = 'pending'` with rows-affected as
/// the answer: the same race-safe idiom `memories::set_embedding` uses.
/// That is what makes resolution exactly-once and first-responder-wins
/// across surfaces (a Matrix reply and a CLI resolve racing each other)
/// with no lock and no read-then-write window.
///
/// `resolution` is a closed set — `{"choice": …}` indexing into the ask's
/// `options`, optionally with `free_text` for the audit row. Free text is
/// stored and shown, never fed back into a plan.
pub async fn resolve(
    pool: &PgPool,
    ask_id: i64,
    resolved_by: &str,
    resolution: &serde_json::Value,
) -> Result<bool, DbError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| DbError::Query(format!("asks resolve begin: {e}")))?;

    let row = sqlx::query(
        "UPDATE asks \
         SET state = 'resolved', \
             resolved_at = now(), \
             resolved_by = $2, \
             resolution = $3 \
         WHERE id = $1 AND state = 'pending' \
         RETURNING task_id",
    )
    .bind(ask_id)
    .bind(resolved_by)
    .bind(resolution)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| DbError::Query(format!("asks resolve: {e}")))?;

    let Some(row) = row else {
        // Lost the race, or no such ask. Dropping `tx` rolls back; nothing
        // was written either way.
        return Ok(false);
    };
    let task_id: i64 = row
        .try_get("task_id")
        .map_err(|e| DbError::Query(format!("decode asks.task_id: {e}")))?;

    if !tasks::resume_from_ask(&mut *tx, task_id).await? {
        // A pending ask whose task is NOT awaiting_operator is an invariant
        // violation: `cancel_for_task` cancels the ask with the task, and
        // `expire_due` expires it with the timeout, so no other path should
        // be able to separate them. Fail closed and loudly — the rollback
        // leaves the ask `pending`, which is recoverable, where committing
        // would leave it resolved with no task to resume.
        return Err(DbError::Other(format!(
            "asks resolve: ask {ask_id} was pending but task {task_id} is not \
             awaiting_operator — refusing to resolve an ask whose task cannot resume"
        )));
    }

    tx.commit()
        .await
        .map_err(|e| DbError::Query(format!("asks resolve commit: {e}")))?;
    Ok(true)
}
```

- [ ] **Step 4: Run the test to verify it passes**

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
cargo test -p kastellan-db --test asks_e2e -- --nocapture 2>&1 | tail -20
```

Expected: 3 tests PASS (or `[SKIP]`).

- [ ] **Step 5: Commit**

```sh
git add db/src/asks.rs db/src/tasks.rs db/tests/asks_e2e.rs
git commit -m "feat(db): asks::resolve — exactly-once, first-responder-wins

Guarded UPDATE ... WHERE state='pending' with rows-affected as the
answer, the same idiom memories::set_embedding uses. A Matrix reply and
a CLI resolve racing each other cannot both win, and the loser is told
so rather than silently overwriting the decision.

A pending ask whose task is not awaiting_operator is an invariant
violation, not a state to paper over: it fails closed and rolls back,
leaving the ask pending (recoverable) rather than resolved with no task
to resume.

Refs #564"
```

---

### Task 5: Expiry — an ask that nobody answers must not wedge a task

**Files:**
- Modify: `db/src/tasks.rs` (add `fail_awaiting_operator`)
- Modify: `db/src/asks.rs` (add `expire_due`)
- Modify: `db/tests/asks_e2e.rs` (add the expiry test)

**Interfaces:**
- Consumes: Tasks 3–4.
- Produces:
  - `db::tasks::fail_awaiting_operator<'e, E>(executor: E, task_id: i64, detail: &str) -> Result<bool, DbError>`
  - `db::asks::expire_due(pool) -> Result<Vec<ExpiredAsk>, DbError>`

- [ ] **Step 1: Write the failing test**

Append to `db/tests/asks_e2e.rs`:

```rust
#[test]
fn expire_due_fails_the_task_closed_and_leaves_others_alone() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "aske-d",
        "aske-l",
        &format!("kastellan-supervisor-test-pg-aske-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        use kastellan_db::{asks, tasks};
        use kastellan_db::tasks::Lane;

        kastellan_db::probe::run(
            &cluster.conn_spec, "core", "startup",
            serde_json::json!({"version": "test", "purpose": "asks-expire-e2e"}),
        ).await.expect("probe run");
        let pool = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await.expect("admin pool");

        // (a) an ask already past its deadline
        let stale_task = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "stale"}),
        ).await.unwrap();
        tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();
        let stale = asks::raise(
            &pool, stale_task, "plan_approval", "approve?",
            &serde_json::json!(["approve"]), None,
            time::OffsetDateTime::now_utc() - time::Duration::seconds(1),
        ).await.unwrap();

        // (b) an ask with plenty of time left
        let fresh_task = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "fresh"}),
        ).await.unwrap();
        tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();
        let fresh = asks::raise(
            &pool, fresh_task, "plan_approval", "approve?",
            &serde_json::json!(["approve"]), None,
            time::OffsetDateTime::now_utc() + time::Duration::seconds(3600),
        ).await.unwrap();

        let expired = asks::expire_due(&pool).await.unwrap();
        assert_eq!(expired.len(), 1, "only the past-deadline ask expires: {expired:?}");
        assert_eq!(expired[0].ask_id, stale.ask_id);
        assert_eq!(expired[0].task_id, stale_task);

        // The stale ask is expired and its task failed CLOSED with a
        // distinguishable detail.
        assert_eq!(asks::get(&pool, stale.ask_id).await.unwrap().unwrap().state, "expired");
        let t = tasks::get(&pool, stale_task).await.unwrap().unwrap();
        assert_eq!(t.state, "failed");
        assert!(t.finished_at.is_some(), "a failed task must be stamped finished");
        assert_eq!(
            t.result.as_ref().and_then(|r| r.get("detail")).and_then(|d| d.as_str()),
            Some("ask_timeout"),
            "the failure must name the ask timeout, not read as a generic error: {:?}", t.result,
        );

        // The fresh one is untouched.
        assert_eq!(asks::get(&pool, fresh.ask_id).await.unwrap().unwrap().state, "pending");
        assert_eq!(tasks::observe_state(&pool, fresh_task).await.unwrap(), "awaiting_operator");

        // Idempotent: a second sweep finds nothing.
        assert!(asks::expire_due(&pool).await.unwrap().is_empty());

        // An expired ask can no longer be resolved.
        assert!(!asks::resolve(
            &pool, stale.ask_id, "operator/cli", &serde_json::json!({"choice": "approve"}),
        ).await.unwrap());
    });
}
```

- [ ] **Step 2: Run it to verify it fails**

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
cargo test -p kastellan-db --test asks_e2e expire_due -- --nocapture 2>&1 | tail -20
```

Expected: FAIL to compile — no function `expire_due`.

- [ ] **Step 3: Implement**

In `db/src/tasks.rs`, after `resume_from_ask`:

```rust
/// Terminal write for a task whose ask expired (#564).
///
/// Separate from [`finalize`] rather than widening its guard. `finalize`
/// means "the lane runner finished a task it was running" and matches
/// `state = 'running'`; keeping that true is worth one small function,
/// because a widened guard would also let a stray `finalize` terminalise a
/// task that is merely suspended.
///
/// The result payload matches `Outcome::Failed`'s shape
/// (`{"kind":"error","detail":…}`) so a reader does not have to know which
/// path produced it. State is `failed` rather than `timed_out`: `timed_out`
/// means the task's own wall-clock deadline elapsed while it was working,
/// and conflating the two would make lane-latency queries count tasks that
/// spent their time waiting on a human.
pub async fn fail_awaiting_operator<'e, E>(
    executor: E,
    task_id: i64,
    detail: &str,
) -> Result<bool, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let r = sqlx::query(
        "UPDATE tasks \
         SET state = 'failed', \
             result = $2, \
             finished_at = now(), \
             updated_at = now() \
         WHERE id = $1 AND state = 'awaiting_operator'",
    )
    .bind(task_id)
    .bind(serde_json::json!({"kind": "error", "detail": detail}))
    .execute(executor)
    .await
    .map_err(|e| DbError::Query(format!("tasks fail_awaiting_operator: {e}")))?;
    Ok(r.rows_affected() == 1)
}
```

In `db/src/asks.rs`, after `resolve`:

```rust
/// The detail string a task's result carries when its ask timed out.
/// A named const because slice 1b's audit rows and any operator query
/// will both want to match on it, and two spellings would silently
/// partition the same population.
pub const ASK_TIMEOUT_DETAIL: &str = "ask_timeout";

/// Expire every ask past its deadline and fail its task closed.
///
/// A headless daemon cannot leave a question pending forever the way a
/// desktop app can — there is no window a human will eventually look at.
/// Without this, a raised ask nobody answers is a permanently wedged task.
///
/// One transaction over the whole sweep: a partially-applied sweep would
/// leave asks expired with their tasks still suspended, which is exactly
/// the wedge this exists to prevent.
///
/// Idempotent — the `state = 'pending'` guard means a second call finds
/// nothing. Returns the retired rows so the caller can emit one audit row
/// each; an empty vec is the normal case.
pub async fn expire_due(pool: &PgPool) -> Result<Vec<ExpiredAsk>, DbError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| DbError::Query(format!("asks expire_due begin: {e}")))?;

    let rows = sqlx::query(
        "UPDATE asks \
         SET state = 'expired' \
         WHERE state = 'pending' AND deadline_at < now() \
         RETURNING id, task_id",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| DbError::Query(format!("asks expire_due: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let ask_id: i64 = row
            .try_get("id")
            .map_err(|e| DbError::Query(format!("decode asks.id: {e}")))?;
        let task_id: i64 = row
            .try_get("task_id")
            .map_err(|e| DbError::Query(format!("decode asks.task_id: {e}")))?;

        // Best-effort on the task side by design: if the task already left
        // `awaiting_operator` (cancelled in the same instant, say) the ask
        // still expires. What must not happen is the reverse — an expired
        // ask with a still-suspended task — and that is what the shared
        // transaction guarantees.
        tasks::fail_awaiting_operator(&mut *tx, task_id, ASK_TIMEOUT_DETAIL).await?;

        out.push(ExpiredAsk { ask_id, task_id });
    }

    tx.commit()
        .await
        .map_err(|e| DbError::Query(format!("asks expire_due commit: {e}")))?;
    Ok(out)
}
```

- [ ] **Step 4: Run the test to verify it passes**

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
cargo test -p kastellan-db --test asks_e2e -- --nocapture 2>&1 | tail -20
```

Expected: 4 tests PASS (or `[SKIP]`).

- [ ] **Step 5: Commit**

```sh
git add db/src/asks.rs db/src/tasks.rs db/tests/asks_e2e.rs
git commit -m "feat(db): asks::expire_due — a question nobody answers must not wedge a task

A headless daemon has no window a human eventually looks at, so an ask
without enforced expiry is a permanent wedge. The whole sweep is one
transaction: a partial one leaves asks expired with tasks still
suspended, which is the wedge itself.

Terminal state is 'failed' with detail 'ask_timeout', not 'timed_out' —
that state means the task's own wall clock elapsed while working, and
conflating them makes lane-latency queries count time spent waiting on
a human. fail_awaiting_operator is separate from finalize rather than
widening its running-only guard.

Refs #564"
```

---

### Task 6: Cancel — a dead task must not leave a live ask

**Files:**
- Modify: `db/src/asks.rs` (add `cancel_for_task`)
- Modify: `db/src/tasks.rs` (`mark_cancelled` → transactional + widened)
- Modify: `db/tests/asks_e2e.rs` (add the cancel test)

**Interfaces:**
- Consumes: Tasks 3–5.
- Produces: `db::asks::cancel_for_task<'e, E>(executor: E, task_id: i64) -> Result<u64, DbError>`. `mark_cancelled`'s signature is unchanged — `(pool, task_id) -> Result<Option<Task>, DbError>` — so its existing callers need no edits.

- [ ] **Step 1: Write the failing test**

Append to `db/tests/asks_e2e.rs`:

```rust
#[test]
fn cancelling_a_suspended_task_cancels_its_ask() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "askc-d",
        "askc-l",
        &format!("kastellan-supervisor-test-pg-askc-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        use kastellan_db::{asks, tasks};
        use kastellan_db::tasks::Lane;

        kastellan_db::probe::run(
            &cluster.conn_spec, "core", "startup",
            serde_json::json!({"version": "test", "purpose": "asks-cancel-e2e"}),
        ).await.expect("probe run");
        let pool = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await.expect("admin pool");

        let task_id = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "cancel probe"}),
        ).await.unwrap();
        tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            &pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve"]), None,
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        ).await.unwrap();

        // Without the widening, awaiting_operator would be a state from
        // which a task cannot be cancelled at all — a wedge this slice
        // would have introduced.
        let cancelled = tasks::mark_cancelled(&pool, task_id).await.unwrap();
        assert!(cancelled.is_some(), "an awaiting_operator task must be cancellable");
        assert_eq!(cancelled.unwrap().state, "cancelled");

        // The ask goes with it. A pending ask on a dead task stays
        // resolvable, and resolving it would try to re-enqueue a cancelled
        // task — which `resolve` now refuses, loudly, for nothing.
        let got = asks::get(&pool, raised.ask_id).await.unwrap().unwrap();
        assert_eq!(got.state, "cancelled");
        assert!(!asks::resolve(
            &pool, raised.ask_id, "operator/cli", &serde_json::json!({"choice": "approve"}),
        ).await.unwrap(), "a cancelled ask must not be resolvable");

        // A cancelled ask is not expiry's business either.
        assert!(asks::expire_due(&pool).await.unwrap().is_empty());

        // Unchanged behaviour for the pre-existing states.
        let plain = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "plain"}),
        ).await.unwrap();
        assert!(tasks::mark_cancelled(&pool, plain).await.unwrap().is_some());
        // Already terminal → idempotent no-op.
        assert!(tasks::mark_cancelled(&pool, plain).await.unwrap().is_none());
    });
}
```

- [ ] **Step 2: Run it to verify it fails**

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
cargo test -p kastellan-db --test asks_e2e cancelling_a_suspended -- --nocapture 2>&1 | tail -20
```

Expected: FAIL — `mark_cancelled` returns `None` for an `awaiting_operator` task, so `assert!(cancelled.is_some())` fires.

- [ ] **Step 3: Implement**

In `db/src/asks.rs`, after `expire_due`:

```rust
/// Cancel every pending ask belonging to a task. Returns how many moved.
///
/// Called from [`crate::tasks::mark_cancelled`] inside its transaction —
/// see the note there for why it lives inside the cancel path rather than
/// in a separate cancel-both helper.
///
/// Executor-generic, and takes a `task_id` rather than an ask id: the
/// caller is cancelling a *task* and does not know or care how many asks
/// it has.
pub async fn cancel_for_task<'e, E>(executor: E, task_id: i64) -> Result<u64, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let r = sqlx::query(
        "UPDATE asks \
         SET state = 'cancelled' \
         WHERE task_id = $1 AND state = 'pending'",
    )
    .bind(task_id)
    .execute(executor)
    .await
    .map_err(|e| DbError::Query(format!("asks cancel_for_task: {e}")))?;
    Ok(r.rows_affected())
}
```

In `db/src/tasks.rs`, replace the whole `mark_cancelled` function (keeping its existing doc comment and appending the new paragraphs):

```rust
/// Producer-side cancellation. Sets `state = 'cancelled'` only if the
/// task is still in `pending`, `running` or `awaiting_operator`; the
/// trigger fires the `tasks_cancelled` NOTIFY.
///
/// Returns the post-update row via `RETURNING` so the caller can emit
/// one producer-side audit row (e.g. `actor='cli' action='task.cancelled'`)
/// without a follow-up SELECT. `None` means the row was not in a
/// cancellable state (already terminal, or does not exist) — idempotent.
///
/// Mirrors the shape [`sweep_crashed`] took on 2026-05-12 for the same
/// reason: an audit emitter downstream needs the row's `lane` and
/// `plan_count` to build the canonical lifecycle payload.
///
/// # Why this also cancels the task's asks (#564)
///
/// `awaiting_operator` was added so a task can suspend on a human
/// decision. Cancelling such a task while leaving its ask `pending` would
/// leave a live question attached to a dead task: still resolvable, and
/// resolving it would try to re-enqueue something already cancelled.
///
/// The ask write lives **inside** this function, in the same transaction,
/// rather than in a separate cancel-both helper — which is why `db::tasks`
/// depends on `db::asks`. With a separate helper, any caller reaching for
/// plain `mark_cancelled` (and the CLI cancel path is one) would silently
/// strand the ask. One cancel path that cannot be bypassed is worth the
/// coupling; same argument `AllowlistDecl` made in #545 for making the
/// half-declared state unrepresentable.
pub async fn mark_cancelled(pool: &PgPool, task_id: i64) -> Result<Option<Task>, DbError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| DbError::Query(format!("tasks mark_cancelled begin: {e}")))?;

    let row = sqlx::query(
        "UPDATE tasks \
         SET state = 'cancelled', \
             finished_at = now(), \
             updated_at = now() \
         WHERE id = $1 AND state IN ('pending', 'running', 'awaiting_operator') \
         RETURNING id, state, lane, created_at, updated_at, started_at, \
                   finished_at, lease_expires_at, plan_count, payload, result",
    )
    .bind(task_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| DbError::Query(format!("tasks mark_cancelled: {e}")))?;

    let Some(row) = row else {
        // Not cancellable. Dropping `tx` rolls back; nothing was written.
        return Ok(None);
    };
    let task = decode_task_row(&row)?;

    crate::asks::cancel_for_task(&mut *tx, task_id).await?;

    tx.commit()
        .await
        .map_err(|e| DbError::Query(format!("tasks mark_cancelled commit: {e}")))?;
    Ok(Some(task))
}
```

- [ ] **Step 4: Run the full db suite to verify nothing regressed**

`mark_cancelled` has existing callers and existing tests. Run everything, not just the new file:

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
cargo test -p kastellan-db -- --nocapture 2>&1 | tail -25
cargo test -p kastellan-core --lib -- --nocapture 2>&1 | tail -10
cargo clippy -p kastellan-db -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: 5 `asks_e2e` tests PASS (or `[SKIP]`), every pre-existing test still passes, clippy exit 0.

- [ ] **Step 5: Commit**

```sh
git add db/src/asks.rs db/src/tasks.rs db/tests/asks_e2e.rs
git commit -m "feat(db): cancelling a task cancels its pending asks

Without widening mark_cancelled, awaiting_operator would be a state from
which a task cannot be cancelled at all — a wedge this slice would have
introduced. And a pending ask on a cancelled task stays resolvable,
where resolving it tries to re-enqueue something already dead.

The ask write goes inside mark_cancelled, coupling db::tasks to
db::asks, rather than into a separate cancel-both helper: with a helper,
any caller reaching for plain mark_cancelled strands the ask. One cancel
path that cannot be bypassed.

Refs #564"
```

---

### Task 7: The resume NOTIFY, `list_pending`, and the lane-runner LISTEN

**Files:**
- Modify: `db/src/asks.rs` (add `list_pending`)
- Modify: `db/tests/asks_e2e.rs` (add the NOTIFY test)
- Modify: `core/src/scheduler/runner.rs:115-122` (add the third LISTEN)

**Interfaces:**
- Consumes: Tasks 3–6.
- Produces: `db::asks::list_pending(pool, limit: i64) -> Result<Vec<Ask>, DbError>`; the lane runner waking on `tasks_resumed`.

- [ ] **Step 1: Write the failing test**

Append to `db/tests/asks_e2e.rs`:

```rust
#[test]
fn resolving_fires_tasks_resumed_and_pending_asks_are_listable() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "askn-d",
        "askn-l",
        &format!("kastellan-supervisor-test-pg-askn-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        use kastellan_db::{asks, tasks};
        use kastellan_db::tasks::Lane;

        kastellan_db::probe::run(
            &cluster.conn_spec, "core", "startup",
            serde_json::json!({"version": "test", "purpose": "asks-notify-e2e"}),
        ).await.expect("probe run");
        let pool = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await.expect("admin pool");

        let task_id = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "notify probe"}),
        ).await.unwrap();
        tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            &pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve"]), Some("d1"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        ).await.unwrap();

        // list_pending surfaces it for an operator inbox.
        let pending = asks::list_pending(&pool, 50).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, raised.ask_id);

        // LISTEN before resolving: PG does not queue notifications for
        // late subscribers, so subscribing afterwards would prove nothing.
        let mut listener = sqlx::postgres::PgListener::connect_with(&pool)
            .await.expect("listener");
        listener.listen("tasks_resumed").await.expect("LISTEN tasks_resumed");

        asks::resolve(
            &pool, raised.ask_id, "operator/cli",
            &serde_json::json!({"choice": "approve"}),
        ).await.unwrap();

        // Without the trigger, a resumed task waits out the lane runner's
        // 30 s HEARTBEAT. Five seconds is generous for a local socket and
        // still far under that, so a timeout here means the trigger is
        // missing rather than that the box is slow.
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), listener.recv())
            .await
            .expect("tasks_resumed must fire within 5s of a resolve")
            .expect("notification");
        assert_eq!(got.channel(), "tasks_resumed");
        assert_eq!(got.payload(), task_id.to_string());

        // Resolved asks leave the pending list.
        assert!(asks::list_pending(&pool, 50).await.unwrap().is_empty());
    });
}
```

- [ ] **Step 2: Run it to verify it fails**

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
cargo test -p kastellan-db --test asks_e2e resolving_fires -- --nocapture 2>&1 | tail -20
```

Expected: FAIL to compile — no function `list_pending`.

- [ ] **Step 3: Implement**

In `db/src/asks.rs`, after `get`:

```rust
/// Every ask still awaiting a human, oldest first — the operator inbox
/// read. Capped at `limit`; `created_at ASC` because the oldest question
/// is the one holding a task up longest.
pub async fn list_pending(pool: &PgPool, limit: i64) -> Result<Vec<Ask>, DbError> {
    let limit = limit.max(0); // LIMIT -1 is a PG error
    let rows = sqlx::query(&format!(
        "SELECT {ASK_COLUMNS} FROM asks \
         WHERE state = 'pending' \
         ORDER BY created_at ASC \
         LIMIT $1"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::Query(format!("asks list_pending: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(decode_ask_row(row)?);
    }
    Ok(out)
}
```

In `core/src/scheduler/runner.rs`, after the existing `tasks_cancelled` LISTEN block (currently lines 119-122):

```rust
    // A task resumed from `awaiting_operator` (#564) is an UPDATE, which
    // the INSERT-only `tasks_inserted` trigger cannot see — without this
    // the resumed task waits out a full HEARTBEAT. Its own channel rather
    // than overloading `tasks_inserted`, whose name would then no longer
    // describe what it carries.
    if let Err(e) = listener.listen("tasks_resumed").await {
        eprintln!("scheduler[{}]: LISTEN tasks_resumed failed: {e}", lane.as_sql());
        return;
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate
export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
cargo test -p kastellan-db --test asks_e2e -- --nocapture 2>&1 | tail -25
cargo test -p kastellan-core --test scheduler_lanes_e2e -- --nocapture 2>&1 | tail -10
cargo clippy -p kastellan-db -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: 6 `asks_e2e` tests PASS (or `[SKIP]`); `scheduler_lanes_e2e` unchanged.

**Known evidence gap, to state in the PR rather than leave implicit:** the `tasks_resumed` *trigger* is proven by the test above, but the lane runner's `LISTEN` on it has no test in this slice — nothing raises an ask in production yet, so there is no resumed task for a scheduler-level test to observe without an elaborate fixture. Its failure mode is bounded and benign: a ≤30 s delay before the heartbeat drains the task, never a wedge. Slice 1b's end-to-end (escalate → resolve → task completes) is where it gets real evidence.

- [ ] **Step 5: Commit**

```sh
git add db/src/asks.rs db/tests/asks_e2e.rs core/src/scheduler/runner.rs
git commit -m "feat(db,scheduler): tasks_resumed NOTIFY + asks::list_pending

The lane runner listens on tasks_resumed so a resolved ask re-enqueues
its task immediately instead of waiting out the 30 s heartbeat.
tasks_inserted is AFTER INSERT only and cannot see the state UPDATE at
all, and overloading its name is the trap that broke
upgrade_from_git.sh's post-deploy check in the #516 arc.

The test LISTENs before resolving: PG does not queue notifications for
late subscribers, so subscribing afterwards would pass vacuously.

Refs #564"
```

---

### Task 8: Mutation-test the guards, gate both hosts, update the docs

No new behaviour. This task proves the tests can actually fail and records the result.

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

**Interfaces:**
- Consumes: everything above.
- Produces: a gated, documented branch ready for PR.

- [ ] **Step 1: Run the four mutations**

For each: make the edit, run the named test, confirm it FAILS, then **revert by re-editing the file back** — never `git checkout -- <file>`, which restores the committed version and eats any uncommitted work in that file ([[mutation-revert-never-git-checkout]]). Record each result.

| # | Mutation | Must fail |
| --- | --- | --- |
| 1 | `asks::resolve` — drop `AND state = 'pending'` | `resolve_is_exactly_once_and_re_enqueues_the_task`. **Expect `Ok(false)` → `Err("task N is not awaiting_operator")`, NOT a silent overwrite.** The write is transactional and `resume_from_ask`'s inner guard makes an overwrite structurally impossible, so the outer guard buys the clean-loss *contract*, not data safety. Verified in Task 4's fix round |
| 2 | `tasks::suspend_for_ask` — drop `lease_expires_at = NULL` | `raise_suspends_the_task_and_releases_the_lease` |
| 3 | `tasks::mark_cancelled` — drop `'awaiting_operator'` from the `IN` list | `cancelling_a_suspended_task_cancels_its_ask` |
| 4 | `asks::expire_due` — replace the `deadline_at < now()` predicate with `TRUE` | `expire_due_fails_the_task_closed_and_leaves_others_alone` (the fresh ask would expire too) |

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate
# after each edit:
cargo test -p kastellan-db --test asks_e2e -- --nocapture 2>&1 | tail -15
```

If any mutation does NOT fail its test, the test is not pinning what it claims — fix the test before continuing. That is the whole point of this step.

- [ ] **Step 2: Predict the test-count delta, then gate the Mac**

Count the new `#[test]` functions in the diff: 24 in `plan_digest` + 6 in `asks_e2e` = **30**. Predict `Mac baseline + 30` and reconcile the actual exactly — an unexplained delta means a test is not being compiled, which is the failure the platform split produces.

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cargo-target-kastellan-gate
cargo test --workspace > $HOME/kastellan-mac-gate.log 2>&1; echo "TEST_EXIT=$?"
grep -c "^test result" $HOME/kastellan-mac-gate.log
tail -30 $HOME/kastellan-mac-gate.log
```

Then clippy — and **count the `Checking` lines, do not trust the exit code**. A warm target dir returns exit 0 having linted a handful of crates:

```sh
cargo clippy --workspace --all-targets -- -D warnings > $HOME/kastellan-mac-clippy.log 2>&1; echo "CLIPPY_EXIT=$?"
grep -c "^ *Checking" $HOME/kastellan-mac-clippy.log   # expect ~27 workspace crates, not 3
```

- [ ] **Step 3: Gate the DGX — this is the authoritative run**

The Mac's full-workspace run skips every PG e2e in this branch (the override is only safe for targeted suites — see Global Constraints), so **the Mac leg alone does not cover this slice**. Run it as exactly `ssh dgx '<cmd>'` (the allow rule is a prefix match; flags before the hostname get denied), write logs under `$HOME` and never `/tmp`, and include an explicit exit-code line plus a DONE sentinel.

```sh
ssh dgx 'cd ~/src/kastellan && git fetch && git checkout feat/564-slice-1a-ask-record && source ~/.cargo/env && cargo test --workspace -- --nocapture > ~/gate-564.log 2>&1; echo "TEST_EXIT=$?" >> ~/gate-564.log; echo DONE >> ~/gate-564.log'
ssh dgx 'grep -c "^==== MARKER" ~/gate-564.log; tail -5 ~/gate-564.log; grep -c "\[SKIP\]" ~/gate-564.log'
```

Confirm, and record each in the handover table:
- one marker pair in the log (two runs appended to one log give two different wrong totals)
- `TEST_EXIT=0`
- the total is the DGX baseline **3268 + 30 = 3298**, reconciled exactly
- **all 6 `asks_e2e` tests actually RAN** — they are the only evidence for this whole slice, and a skip-as-pass is indistinguishable from a pass in the count:
  ```sh
  ssh dgx 'grep "asks_e2e" -A 12 ~/gate-564.log | head -20'
  ```
- `[SKIP]` count is exactly 4, all `KASTELLAN_GLINER_RELEX_ENABLE` — **not** the bwrap-userns skip

Then clippy in a fresh target dir, counting `Checking` lines:

```sh
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && CARGO_TARGET_DIR=~/.cargo-target-564 cargo clippy --workspace --all-targets -- -D warnings > ~/clippy-564.log 2>&1; echo "CLIPPY_EXIT=$?" >> ~/clippy-564.log'
ssh dgx 'grep -c "^ *Checking" ~/clippy-564.log; tail -3 ~/clippy-564.log'
```

- [ ] **Step 4: Update HANDOVER.md and ROADMAP.md**

In `HANDOVER.md`: add a **Current state** entry for this slice; add both gate rows to the [Test baseline](#test-baseline-authoritative) table with the reconciled counts; update the header's `main` HEAD / baseline line once merged; move the #564 Next-TODO bullet to name slice 1b as what remains. Keep the file under 500 lines.

In `ROADMAP.md`: tick the Phase 3 ask-channel entry with this slice's commit and one line on what slice 1b still owes.

State the two things this slice does **not** prove: the lane runner's `tasks_resumed` LISTEN has no test (Task 7 Step 4), and `plan_digest`'s field selection is provisional until real escalations exercise it.

- [ ] **Step 5: Commit and open the PR**

```sh
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover,roadmap): record #564 slice 1a and its two-host gate"
git push -u origin feat/564-slice-1a-ask-record
gh pr create --base main --title "feat(db): the durable ask record — #564 slice 1a" --body "<see below>"
```

The PR body must cover: what slice 1a is and why it stops where it does (the plumbing-without-a-producer argument), the eight spec decisions in brief, the four mutations and their results, both hosts' gates with reconciled counts, the two things not proven, and `Refs #564` (**not** `Closes` — slices 1b, 2 and 3 remain).

---

## Self-review

**Spec coverage** — every section maps to a task: D1/D2 → Task 1; schema, D4's trigger, D6's column → Task 2; D3 nonce + suspend → Task 3; D5 transactions + resolve → Task 4; D6 expiry + D7 `fail_awaiting_operator` → Task 5; D8 cancel → Task 6; D4's LISTEN + `list_pending` → Task 7; the spec's mutation list → Task 8. The spec's "what this slice deliberately excludes" is enforced by no task touching `Outcome`, `drain_lane`, or the `Escalate` arm.

**Type consistency** — `suspend_for_ask` / `resume_from_ask` / `fail_awaiting_operator` / `cancel_for_task` are all executor-generic (`E: sqlx::Executor<'e, Database = sqlx::Postgres>`) because each is called inside a caller's transaction; `raise` / `resolve` / `expire_due` / `get` / `list_pending` take `&PgPool` because each owns its transaction. `ASK_COLUMNS` is shared by `get` and `list_pending` so a column rename breaks one place. `mark_cancelled`'s signature is unchanged, so its existing callers are untouched.

**One deliberate deviation from the issue**, carried from the approved spec: expiry is in this slice rather than slice 2 (spec D6).
