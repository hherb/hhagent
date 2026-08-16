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

        // THE property: a second resolve loses CLEANLY, and changes nothing.
        //
        // `resolve` commits the `asks` UPDATE and `resume_from_ask` in ONE
        // transaction, so by the time the first `resolve` above returned,
        // the task had already left `awaiting_operator` atomically in that
        // same commit. That makes a second resolve overwriting the winner's
        // decision structurally impossible, with or without the outer
        // `state = 'pending'` guard on the `asks` row — data safety comes
        // from the transaction plus `resume_from_ask`'s inner
        // `awaiting_operator` guard, which a losing call can never satisfy.
        //
        // What the outer guard on `asks` actually buys is the CONTRACT, not
        // that safety: it is what lets a legitimate race return a clean
        // `Ok(false)` instead of falling through to `resume_from_ask`'s
        // guard and surfacing the invariant-violation `Err` meant for a
        // genuinely corrupt row. Remove the outer guard and this exact call
        // routes into that `Err` branch instead of `Ok(false)` — confirmed
        // by mutation-testing it away (see task-4-report.md's "Fix round
        // 1/5" section for the observed failure). So there are two layers
        // here, doing two different jobs: lose the outer guard and a
        // legitimate loser gets an error instead of a clean "you lost";
        // lose the inner guard (or the transaction) and data safety itself
        // is gone. Neither one substitutes for the other.
        let second = asks::resolve(
            &pool, raised.ask_id, "matrix/@someone-else:evil.example",
            &serde_json::json!({"choice": "deny"}),
        ).await;
        assert!(
            matches!(second, Ok(false)),
            "the second resolve must lose CLEANLY (Ok(false)), not error or \
             win — got {second:?}",
        );

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

        // (a) two asks already past their deadline, on two DIFFERENT tasks.
        // Two, not one: `expired.len() == 1` would pass just as well for a
        // sweep that processes only the first due row and stops, which is a
        // real bug class (a `for` loop replaced by an early `return`/`break`)
        // and was previously untested.
        let stale_task = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "stale"}),
        ).await.unwrap();
        tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();
        let stale = asks::raise(
            &pool, stale_task, "plan_approval", "approve?",
            &serde_json::json!(["approve"]), None,
            time::OffsetDateTime::now_utc() - time::Duration::seconds(1),
        ).await.unwrap();

        let stale_task2 = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "stale2"}),
        ).await.unwrap();
        tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();
        let stale2 = asks::raise(
            &pool, stale_task2, "plan_approval", "approve?",
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

        let mut expired = asks::expire_due(&pool).await.unwrap();
        assert_eq!(expired.len(), 2, "both past-deadline asks expire: {expired:?}");
        expired.sort_by_key(|e| e.ask_id); // RETURNING order is not guaranteed
        let mut want = [
            asks::ExpiredAsk { ask_id: stale.ask_id, task_id: stale_task },
            asks::ExpiredAsk { ask_id: stale2.ask_id, task_id: stale_task2 },
        ];
        want.sort_by_key(|e| e.ask_id);
        assert_eq!(expired, want, "the sweep must name both stale asks and both their tasks");

        // Both stale asks are expired and BOTH their tasks failed CLOSED
        // with a distinguishable detail — not just the first one the sweep
        // touched.
        for (ask, task_id) in [(&stale, stale_task), (&stale2, stale_task2)] {
            assert_eq!(asks::get(&pool, ask.ask_id).await.unwrap().unwrap().state, "expired");
            let t = tasks::get(&pool, task_id).await.unwrap().unwrap();
            assert_eq!(t.state, "failed");
            assert!(t.finished_at.is_some(), "a failed task must be stamped finished");
            assert_eq!(
                t.result.as_ref().and_then(|r| r.get("detail")).and_then(|d| d.as_str()),
                Some("ask_timeout"),
                "the failure must name the ask timeout, not read as a generic error: {:?}", t.result,
            );
        }

        // The fresh one is untouched.
        assert_eq!(asks::get(&pool, fresh.ask_id).await.unwrap().unwrap().state, "pending");
        assert_eq!(tasks::observe_state(&pool, fresh_task).await.unwrap(), "awaiting_operator");

        // Idempotent: a second sweep finds nothing.
        assert!(asks::expire_due(&pool).await.unwrap().is_empty());

        // An expired ask can no longer be resolved.
        assert!(!asks::resolve(
            &pool, stale.ask_id, "operator/cli", &serde_json::json!({"choice": "approve"}),
        ).await.unwrap());

        // Coverage note: this pins that the sweep processes EVERY due row
        // (a loop that stopped after the first would fail the
        // `expired.len() == 2` assertion above). It does NOT cover
        // atomicity under a mid-loop DB error — that `?` only fires on a
        // genuine database failure, and there is no fault-injection seam in
        // `tests-common` to force one, so that path stays correct by
        // construction (one transaction, `?` returns before `commit`) but
        // untested. Do not assume it is exercised here.
    });
}

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
