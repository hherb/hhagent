//! PG e2e for `scheduler::asks` — the raise/expire wiring and its audit rows.
//!
//! Skips silently with `[SKIP]` on hosts without Postgres; run with
//! `-- --nocapture` to see whether it ran.
//!
//! Issue #15 will eventually hoist the bring-up helpers into a shared
//! fixture; until then we copy and adapt the recipe from
//! `core/tests/scheduler_inner_loop_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use kastellan_core::cassandra::types::{DataClass, Plan, Severity};
use kastellan_core::scheduler::asks;
use kastellan_core::scheduler::audit::{ACTION_ASK_EXPIRED, ACTION_ASK_RAISED};
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
    let service_name = format!("kastellan-sched-test-pg-asks-{suffix}");
    let cluster = tokio::task::block_in_place(|| {
        bring_up_pg_cluster(&bin_dir, "asks-d", "asks-l", &service_name)
    });

    kastellan_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "scheduler-asks"}),
    )
    .await
    .ok()?;

    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .ok()?;

    Some((pool, cluster))
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
