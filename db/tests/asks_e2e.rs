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
