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
