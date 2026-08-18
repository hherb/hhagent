//! End-to-end: escalate -> suspend -> resolve -> resume, through the real
//! lane runner (#564 slice 1b).
//!
//! Driven through `spawn_scheduler` rather than `run_to_terminal`, because
//! `drain_lane`'s non-finalize branch is half of what is under test and only
//! the lane runner reaches it.
//!
//! Skips silently with `[SKIP]` on hosts without Postgres or a reachable
//! supervisor; run with `-- --nocapture` to see whether it ran.
//!
//! Issue #15 will eventually hoist the bring-up helpers into a shared
//! fixture; until then we copy and adapt the recipe from
//! `core/tests/scheduler_asks_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kastellan_core::cassandra::review::{ChainReviewStage, ReviewStage, ReviewStageContext};
use kastellan_core::cassandra::types::{DataClass, Plan, PlannedStep, Severity, Verdict};
use kastellan_core::memory::embedder::NoOpEmbedder;
use kastellan_core::scheduler::agent::{AgentError, FormulationMeta, PlanFormulator};
use kastellan_core::scheduler::audit::ACTION_TASK_FINALIZE;
use kastellan_core::scheduler::inner_loop::{StepDispatcher, StepOutcome, TaskContext};
use kastellan_core::scheduler::{spawn_scheduler, SchedulerHandle};
use kastellan_db::tasks::{self, insert_pending, Lane};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix, PgCluster,
};

/// Async helper: bring up a PG cluster (via the shared
/// [`kastellan_tests_common::bring_up_pg_cluster`]), run migrations,
/// return pool + cluster handle. The `PgCluster` carries the cleanup
/// guards internally and drops them in the right order at end of scope.
/// Returns `None` when PG or supervisor is unavailable (skip).
async fn bring_up_pg(label: &str) -> Option<(sqlx::PgPool, PgCluster)> {
    if skip_if_no_supervisor() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = format!("{}-{}", label, unique_suffix());
    let service_name = format!("kastellan-sched-test-pg-askpath-{suffix}");
    let cluster = tokio::task::block_in_place(|| {
        bring_up_pg_cluster(&bin_dir, "apd", "apl", &service_name)
    });

    kastellan_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "scheduler-ask-path"}),
    )
    .await
    .ok()?;

    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .ok()?;

    Some((pool, cluster))
}

// ---------------------------------------------------------------------------
// Stubs
// ---------------------------------------------------------------------------

/// Escalates the next `remaining` plans it sees, then approves.
///
/// Two of the tests set `remaining` to `u32::MAX` — i.e. "escalate
/// forever". That is load-bearing, not laziness: with a reviewer that
/// stops escalating after the first plan, the resumed run would simply be
/// approved by the *reviewer*, and the test would pass whether or not the
/// `Escalate` arm ever consulted the operator's decision.
struct EscalatingReview {
    remaining: Mutex<u32>,
}

#[async_trait]
impl ReviewStage for EscalatingReview {
    fn name(&self) -> &str {
        "escalating"
    }

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

/// Returns `StepOutcome::Ok` for every step without doing anything. None
/// of these tests reaches step execution on purpose; this exists so the
/// scheduler can be built.
struct NoopDispatcher;

#[async_trait]
impl StepDispatcher for NoopDispatcher {
    async fn dispatch_step(&self, _task_id: i64, _step: &PlannedStep) -> StepOutcome {
        StepOutcome::Ok(serde_json::json!("done"))
    }
}

/// One formulator covering all three variants. `vary` makes each call
/// return a plan differing in a field the digest INCLUDES; `calls` is the
/// counter the denial test reads.
struct TestFormulator {
    vary: bool,
    calls: Arc<Mutex<u32>>,
}

#[async_trait]
impl PlanFormulator for TestFormulator {
    async fn formulate_plan(
        &self,
        _ctx: &TaskContext,
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

/// The `FormulationMeta` fixture, copied from `ScriptedFormulator` in
/// `core/tests/scheduler_lanes_e2e.rs`. Nothing under test reads it; it
/// only has to be well-formed enough for the `plan.formulate` audit row.
fn test_meta() -> FormulationMeta {
    FormulationMeta {
        prompt_name: "agent_planner".into(),
        prompt_sha256: "test".into(),
        llm_model: "test-model".into(),
        llm_backend: "local".into(),
        latency_ms: 1,
        retry_count: 0,
        assembled_prompt_sha256: "test-assembled-sha".into(),
        l0_count: 0,
        l1_count: 0,
        skill_count: 0,
        recalled_memory_ids: Vec::new(),
        recall_count: 0,
        recall_query_sha256:
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
        graph_seed_entity_ids: Vec::new(),
        graph_seed_count: 0,
        graph_seed_source: kastellan_core::entity_extraction::SeedSource::None,
    }
}

// ---------------------------------------------------------------------------
// Plan factories (mirroring core/tests/scheduler_lanes_e2e.rs)
// ---------------------------------------------------------------------------

fn task_complete_plan(body: &str) -> Plan {
    Plan {
        context: "c".into(),
        decision: "task_complete".into(),
        rationale: "done".into(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": body})),
        data_ceiling: Some(DataClass::Public),
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
        python_skill: None,
    }
}

/// A non-terminal one-step plan whose `parameters` carry `n`. `parameters`
/// is one of the fields `plan_digest` INCLUDES, so two calls with different
/// `n` produce two different digests — which is what makes the "a different
/// replan re-escalates" test test anything.
fn one_step_plan_with_param(n: u32) -> Plan {
    Plan {
        context: "c".into(),
        decision: "act".into(),
        rationale: "r".into(),
        steps: vec![PlannedStep {
            tool: "sleep".into(),
            method: "doit".into(),
            parameters: serde_json::json!({"n": n}),
            returns: "x".into(),
            done_when: "x".into(),
            classification: DataClass::Public,
        }],
        result: None,
        data_ceiling: Some(DataClass::Public),
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
        python_skill: None,
    }
}

// ---------------------------------------------------------------------------
// Scheduler spawn helpers
// ---------------------------------------------------------------------------

/// The shared body. `formulator` is the only thing the three variants
/// differ in; everything else is a no-op stub.
fn spawn_with(
    pool: &sqlx::PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
) -> SchedulerHandle {
    spawn_scheduler(
        pool.clone(),
        formulator,
        review,
        Arc::new(NoopDispatcher),
        Arc::new(kastellan_core::entity_extraction::NoOpEntityExtractor::new()),
        Arc::new(NoOpEmbedder::new()),
    )
}

/// Returns the same terminal plan on every call.
fn spawn_test_scheduler(pool: &sqlx::PgPool, review: Arc<ChainReviewStage>) -> SchedulerHandle {
    spawn_with(
        pool,
        Arc::new(TestFormulator { vary: false, calls: Arc::new(Mutex::new(0)) }),
        review,
    )
}

/// As above, plus the shared counter of `formulate_plan` calls.
fn spawn_test_scheduler_counting(
    pool: &sqlx::PgPool,
    review: Arc<ChainReviewStage>,
) -> (SchedulerHandle, Arc<Mutex<u32>>) {
    let calls = Arc::new(Mutex::new(0));
    let h = spawn_with(
        pool,
        Arc::new(TestFormulator { vary: false, calls: Arc::clone(&calls) }),
        review,
    );
    (h, calls)
}

/// Returns a DIFFERENT plan each call.
fn spawn_test_scheduler_varying(
    pool: &sqlx::PgPool,
    review: Arc<ChainReviewStage>,
) -> SchedulerHandle {
    spawn_with(
        pool,
        Arc::new(TestFormulator { vary: true, calls: Arc::new(Mutex::new(0)) }),
        review,
    )
}

/// A reviewer chain of one [`EscalatingReview`].
fn escalating(times: u32) -> Arc<ChainReviewStage> {
    Arc::new(ChainReviewStage::new(vec![Arc::new(EscalatingReview {
        remaining: Mutex::new(times),
    })]))
}

/// Seed one `pending` task on the fast lane.
///
/// `max_plans` travels in the payload rather than through the spawn
/// helpers, because the lane's cap comes from `DEFAULT_MAX_PLANS_FAST` and
/// not from `spawn_scheduler`'s arguments. Setting it low means a bug that
/// loops instead of suspending hits the cap in a few plans instead of
/// running to the lane default.
async fn seed_task(pool: &sqlx::PgPool) -> i64 {
    insert_pending(
        pool,
        Lane::Fast,
        serde_json::json!({"instruction": "x", "max_plans": 3}),
    )
    .await
    .expect("insert")
}

/// Poll `tasks.state` until it equals `want`, or give up after `secs` and
/// return whatever it last saw. A fixed sleep would pass or fail on
/// machine speed rather than on behaviour.
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

async fn count_asks_for(pool: &sqlx::PgPool, task_id: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM asks WHERE task_id = $1")
        .bind(task_id)
        .fetch_one(pool)
        .await
        .expect("count asks")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_escalated_plan_suspends_the_task_and_writes_no_finalize_row() {
    let Some((pool, _cluster)) = bring_up_pg("suspend").await else {
        return; // [SKIP]
    };
    let task_id = seed_task(&pool).await;
    let handle = spawn_test_scheduler(&pool, escalating(1));

    assert_eq!(
        await_state(&pool, task_id, "awaiting_operator", 20).await,
        "awaiting_operator",
    );

    // The load-bearing negative: `drain_lane` must NOT have finalized.
    let finalize_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = $1 AND payload->>'task_id' = $2::text",
    )
    .bind(ACTION_TASK_FINALIZE)
    .bind(task_id)
    .fetch_one(&pool)
    .await
    .expect("count finalize rows");
    assert_eq!(
        finalize_rows, 0,
        "a suspended task has not finished and must not be finalized",
    );

    let pending = kastellan_db::asks::list_pending(&pool, 10).await.expect("list");
    assert_eq!(pending.len(), 1, "exactly one ask was raised");
    assert_eq!(pending[0].task_id, task_id);
    assert!(
        pending[0].plan_digest.is_some(),
        "the raised ask binds to the digest of the plan that escalated",
    );

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approval_lets_the_same_plan_through_on_resume() {
    let Some((pool, _cluster)) = bring_up_pg("approve").await else {
        return; // [SKIP]
    };
    let task_id = seed_task(&pool).await;

    // The reviewer escalates EVERY plan and the formulator returns the
    // identical plan every time. That combination is load-bearing: with a
    // reviewer that stops escalating after the first plan, the resumed run
    // would simply be approved by the reviewer and the test would pass
    // whether or not the arm ever consults the operator's approval. Here
    // the replan escalates again, so the only way the task can complete is
    // the digest matching the resolved ask.
    let handle = spawn_test_scheduler(&pool, escalating(u32::MAX));

    assert_eq!(
        await_state(&pool, task_id, "awaiting_operator", 20).await,
        "awaiting_operator",
    );
    let ask = kastellan_db::asks::list_pending(&pool, 10).await.expect("list")[0].clone();
    assert!(kastellan_db::asks::resolve(
        &pool,
        ask.id,
        "operator",
        &serde_json::json!({"choice": "approve"}),
    )
    .await
    .expect("resolve"));

    assert_eq!(await_state(&pool, task_id, "completed", 20).await, "completed");
    // Exactly one ask: the approval covered the replan rather than
    // producing a second question.
    assert_eq!(
        count_asks_for(&pool, task_id).await,
        1,
        "an approved, identical replan must not re-escalate",
    );

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denial_terminates_the_task_without_replanning() {
    let Some((pool, _cluster)) = bring_up_pg("deny").await else {
        return; // [SKIP]
    };
    let task_id = seed_task(&pool).await;

    // The formulator counts its calls so we can assert the resumed run
    // never asked for a plan.
    let (handle, formulate_calls) = spawn_test_scheduler_counting(&pool, escalating(1));

    assert_eq!(
        await_state(&pool, task_id, "awaiting_operator", 20).await,
        "awaiting_operator",
    );
    let calls_at_suspend = *formulate_calls.lock().unwrap();
    let ask = kastellan_db::asks::list_pending(&pool, 10).await.expect("list")[0].clone();
    assert!(kastellan_db::asks::resolve(
        &pool,
        ask.id,
        "operator",
        &serde_json::json!({"choice": "deny"}),
    )
    .await
    .expect("resolve"));

    assert_eq!(await_state(&pool, task_id, "blocked", 20).await, "blocked");
    assert_eq!(
        *formulate_calls.lock().unwrap(),
        calls_at_suspend,
        "a denied task must terminate BEFORE planning — this is the assertion that \
         fails if the denial only bound to the plan digest",
    );

    let t = tasks::get(&pool, task_id).await.expect("get").expect("a task");
    let r = t.result.expect("a denied task has a result");
    assert_eq!(r.get("kind").and_then(|v| v.as_str()), Some("denied"));
    assert_eq!(r.get("ask_id").and_then(|v| v.as_i64()), Some(ask.id));

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_different_replan_raises_a_second_ask_rather_than_riding_the_first_approval() {
    let Some((pool, _cluster)) = bring_up_pg("differs").await else {
        return; // [SKIP]
    };
    let task_id = seed_task(&pool).await;

    // Every plan escalates, and the formulator returns a DIFFERENT plan
    // each time (it varies `parameters`, which the digest includes — not
    // `context`, which it excludes).
    let handle = spawn_test_scheduler_varying(&pool, escalating(5));

    assert_eq!(
        await_state(&pool, task_id, "awaiting_operator", 20).await,
        "awaiting_operator",
    );
    let first = kastellan_db::asks::list_pending(&pool, 10).await.expect("list")[0].clone();
    assert!(kastellan_db::asks::resolve(
        &pool,
        first.id,
        "operator",
        &serde_json::json!({"choice": "approve"}),
    )
    .await
    .expect("resolve"));

    // The replan differs, so the approval must not cover it.
    assert_eq!(
        await_state(&pool, task_id, "awaiting_operator", 20).await,
        "awaiting_operator",
    );
    assert_eq!(
        count_asks_for(&pool, task_id).await,
        2,
        "an approval binds to a digest, not to a task",
    );

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_plan_count_is_monotonic_across_a_suspend_and_resume() {
    let Some((pool, _cluster)) = bring_up_pg("plancount").await else {
        return; // [SKIP]
    };
    let task_id = seed_task(&pool).await;

    // Always-escalating, identical plan — same reasoning as the approval
    // test: the resumed run must reach completion via the approval, not by
    // the reviewer changing its mind.
    let handle = spawn_test_scheduler(&pool, escalating(u32::MAX));

    assert_eq!(
        await_state(&pool, task_id, "awaiting_operator", 20).await,
        "awaiting_operator",
    );
    let before = tasks::get(&pool, task_id).await.unwrap().unwrap().plan_count;
    assert!(before >= 1, "at least one plan ran before the escalation");

    let ask = kastellan_db::asks::list_pending(&pool, 10).await.expect("list")[0].clone();
    assert!(kastellan_db::asks::resolve(
        &pool,
        ask.id,
        "operator",
        &serde_json::json!({"choice": "approve"}),
    )
    .await
    .expect("resolve"));
    assert_eq!(await_state(&pool, task_id, "completed", 20).await, "completed");

    let after = tasks::get(&pool, task_id).await.unwrap().unwrap().plan_count;
    assert!(
        after > before,
        "plan_count must not rewind across a resume (was {before}, now {after})",
    );

    handle.shutdown().await;
}
