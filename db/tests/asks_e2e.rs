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
             VALUES ($1, 'plan_approval', 'b', '[\"approve\"]'::jsonb, 'h', now() + interval '1 hour', 'bogus')",
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
            "an ask may only be raised against a RUNNING task",
        );
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM asks")
            .fetch_one(&pool).await.unwrap();
        // This pins that `raise` is ATOMIC, not that its statement ordering
        // is what prevents an orphan: the INSERT and the suspend share one
        // transaction, so a failed guard rolls back either way round. (An
        // earlier version of this message claimed the ordering was doing the
        // work, echoing a since-corrected comment on `raise` itself.)
        assert_eq!(n, 0, "a failed raise must not leave a row behind");

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
        assert_eq!(raised.nonce.expose().len(), 64, "32 bytes hex-encoded");
        let stored: String = sqlx::query_scalar("SELECT nonce_sha256 FROM asks WHERE id = $1")
            .bind(raised.ask_id).fetch_one(&pool).await.unwrap();
        assert_eq!(stored, asks::sha256_hex(raised.nonce.expose()));
        assert_ne!(stored, raised.nonce.expose(), "the plaintext nonce must not be stored");

        // Two raises never mint the same nonce.
        let t2 = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "second"}),
        ).await.unwrap();
        tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised2 = asks::raise(
            &pool, t2, "plan_approval", "b", &serde_json::json!(["approve"]), None,
            time::OffsetDateTime::now_utc() + time::Duration::seconds(60),
        ).await.expect("raise 2");
        assert_ne!(raised.nonce.expose(), raised2.nonce.expose());

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
        // routes into that `Err` branch instead of `Ok(false)`: the observed
        // failure is `asks resolve: ask N was pending but task M is not
        // awaiting_operator`, raised at a legitimate lost race rather than at
        // a corrupt row. So there are two layers
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
        //
        // Raised with a FUTURE deadline and then backdated by raw SQL:
        // `asks_deadline_after_created` now rejects a past deadline at the
        // INSERT, so an already-due ask is a state the API deliberately
        // will not produce. Same escape hatch, and same reasoning, as the
        // cancel test below.
        let stale_task = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "stale"}),
        ).await.unwrap();
        tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();
        let stale = asks::raise(
            &pool, stale_task, "plan_approval", "approve?",
            &serde_json::json!(["approve"]), None,
            time::OffsetDateTime::now_utc() + time::Duration::seconds(3600),
        ).await.unwrap();

        let stale_task2 = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "stale2"}),
        ).await.unwrap();
        tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();
        let stale2 = asks::raise(
            &pool, stale_task2, "plan_approval", "approve?",
            &serde_json::json!(["approve"]), None,
            time::OffsetDateTime::now_utc() + time::Duration::seconds(3600),
        ).await.unwrap();

        // Backdate BOTH columns: `asks_deadline_after_created` rejects a
        // deadline earlier than the row's own creation, so an overdue ask is
        // one that was CREATED long ago — which is also what a real one looks
        // like.
        sqlx::query(
            "UPDATE asks SET created_at = now() - interval '2 hours', \
                             deadline_at = now() - interval '1 hour' WHERE id = ANY($1)",
        )
            .bind(vec![stale.ask_id, stale2.ask_id])
            .execute(&pool)
            .await
            .expect("backdate the two stale deadlines");

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
        let cancelled = cancelled.expect("an awaiting_operator task must be cancellable");
        assert_eq!(cancelled.task.state, "cancelled");
        // The pre-cancel state is carried out, because the producer audit
        // emitter cannot recover it afterwards — a task cancelled out of
        // `running` and one cancelled out of `awaiting_operator` both have
        // `started_at` set, and only the first has a live inner loop that
        // will write its own `task.finalize`.
        assert_eq!(
            cancelled.previous_state, "awaiting_operator",
            "mark_cancelled must report the state it cancelled OUT OF",
        );
        assert_eq!(
            cancelled.asks_cancelled, 1,
            "the one pending ask must be counted, so the audit row can name it",
        );

        // The ask goes with it. A pending ask on a dead task stays
        // resolvable, and resolving it would try to re-enqueue a cancelled
        // task — which `resolve` now refuses, loudly, for nothing.
        let got = asks::get(&pool, raised.ask_id).await.unwrap().unwrap();
        assert_eq!(got.state, "cancelled");
        assert!(!asks::resolve(
            &pool, raised.ask_id, "operator/cli", &serde_json::json!({"choice": "approve"}),
        ).await.unwrap(), "a cancelled ask must not be resolvable");

        // A cancelled ask is not expiry's business either — even when it is
        // ALSO past its deadline. Force the deadline into the past (raw SQL:
        // deliberately reaching past the API to construct a state the API
        // will not itself produce) so the sweep's `deadline_at < now()`
        // predicate would take this row too, if it were still `pending`.
        // The only thing left to skip it is `cancel_for_task` having moved
        // it out of `pending` — which is what this asserts, and what makes
        // the assertion falsifiable: with the ask still `pending`, the sweep
        // would take it and fail the task.
        sqlx::query(
            "UPDATE asks SET created_at = now() - interval '3 hours', \
                             deadline_at = now() - interval '1 hour' WHERE id = $1",
        )
            .bind(raised.ask_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(asks::expire_due(&pool).await.unwrap().is_empty());
        assert_eq!(
            tasks::observe_state(&pool, task_id).await.unwrap(), "cancelled",
            "the sweep must not overwrite a cancelled task with failed",
        );

        // Unchanged behaviour for the pre-existing states.
        let plain = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "plain"}),
        ).await.unwrap();
        let plain_cancel = tasks::mark_cancelled(&pool, plain)
            .await
            .unwrap()
            .expect("a pending task is cancellable");
        assert_eq!(plain_cancel.previous_state, "pending");
        assert_eq!(
            plain_cancel.asks_cancelled, 0,
            "a never-claimed task has no ask to cancel",
        );
        // Already terminal → idempotent no-op.
        assert!(tasks::mark_cancelled(&pool, plain).await.unwrap().is_none());
    });
}

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

// ---------------------------------------------------------------------
// `resolve_with_nonce` must be the easy, safe
// path for an untrusted (channel/transport) caller. These mirror
// `resolve_is_exactly_once_and_re_enqueues_the_task` above but drive the
// nonce-keyed entry point instead of the by-id one.
// ---------------------------------------------------------------------

#[test]
fn resolve_with_nonce_succeeds_and_re_enqueues_the_task() {
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
        "asko-d",
        "asko-l",
        &format!("kastellan-supervisor-test-pg-asko-{suffix}"),
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
            serde_json::json!({"version": "test", "purpose": "asks-resolve-nonce-e2e"}),
        ).await.expect("probe run");
        let pool = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await.expect("admin pool");

        let task_id = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "nonce resolve probe"}),
        ).await.unwrap();
        tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            &pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve", "deny"]), Some("digest1"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        ).await.unwrap();

        let won = asks::resolve_with_nonce(
            &pool, &raised.nonce, "matrix/@horst:kastellan.dev",
            &serde_json::json!({"choice": "approve"}),
        ).await.unwrap();
        assert!(won, "resolving with the correct plaintext nonce must succeed");

        // The task is back in the queue, claimable again — same outcome
        // as the by-id path.
        assert_eq!(tasks::observe_state(&pool, task_id).await.unwrap(), "pending");
        let reclaimed = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap();
        assert_eq!(reclaimed.map(|t| t.id), Some(task_id));

        let got = asks::get(&pool, raised.ask_id).await.unwrap().unwrap();
        assert_eq!(got.state, "resolved");
        assert_eq!(got.resolved_by.as_deref(), Some("matrix/@horst:kastellan.dev"));
        assert_eq!(got.resolution, Some(serde_json::json!({"choice": "approve"})));
    });
}

#[test]
fn resolve_with_nonce_rejects_a_wrong_nonce_and_leaves_the_ask_pending() {
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
        "askw-d",
        "askw-l",
        &format!("kastellan-supervisor-test-pg-askw-{suffix}"),
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
            serde_json::json!({"version": "test", "purpose": "asks-resolve-nonce-wrong-e2e"}),
        ).await.expect("probe run");
        let pool = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await.expect("admin pool");

        let task_id = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "wrong nonce probe"}),
        ).await.unwrap();
        tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            &pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve", "deny"]), Some("digest1"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        ).await.unwrap();

        // Syntactically valid — 64 lowercase hex chars, the exact shape a
        // real nonce takes — but never issued by `raise`. Deliberately NOT
        // a garbage string: this exercises the hash-mismatch path, not a
        // decode/format failure.
        let unissued_nonce = "0".repeat(64);
        assert_ne!(
            unissued_nonce, raised.nonce.expose(),
            "test setup: must not collide with the real nonce",
        );

        let lost = asks::resolve_with_nonce(
            &pool, &asks::Nonce::from_wire(unissued_nonce.clone()), "matrix/@stranger:evil.example",
            &serde_json::json!({"choice": "approve"}),
        ).await.unwrap();
        assert!(!lost, "an unissued nonce must not resolve the ask");

        // Nothing moved: the ask is still pending, unresolved, and the
        // task is still suspended.
        let got = asks::get(&pool, raised.ask_id).await.unwrap().unwrap();
        assert_eq!(got.state, "pending");
        assert!(got.resolved_at.is_none());
        assert!(got.resolved_by.is_none());
        assert_eq!(tasks::observe_state(&pool, task_id).await.unwrap(), "awaiting_operator");

        // The real nonce still works afterwards — a failed guess must not
        // have consumed or corrupted the row.
        let won = asks::resolve_with_nonce(
            &pool, &raised.nonce, "matrix/@horst:kastellan.dev",
            &serde_json::json!({"choice": "approve"}),
        ).await.unwrap();
        assert!(won, "the correct nonce must still resolve the ask after a wrong guess");
    });
}

#[test]
fn resolve_with_nonce_on_an_already_resolved_ask_returns_false() {
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
        "askz-d",
        "askz-l",
        &format!("kastellan-supervisor-test-pg-askz-{suffix}"),
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
            serde_json::json!({"version": "test", "purpose": "asks-resolve-nonce-twice-e2e"}),
        ).await.expect("probe run");
        let pool = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await.expect("admin pool");

        let task_id = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "double nonce resolve probe"}),
        ).await.unwrap();
        tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            &pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve", "deny"]), Some("digest1"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        ).await.unwrap();

        let first = asks::resolve_with_nonce(
            &pool, &raised.nonce, "matrix/@horst:kastellan.dev",
            &serde_json::json!({"choice": "approve"}),
        ).await.unwrap();
        assert!(first, "the first resolve must win");

        // Same nonce, second call: the ask is no longer `pending`, so the
        // guarded UPDATE finds nothing — clean `Ok(false)`, not an error,
        // and the winner's decision is untouched.
        let second = asks::resolve_with_nonce(
            &pool, &raised.nonce, "matrix/@someone-else:evil.example",
            &serde_json::json!({"choice": "deny"}),
        ).await.unwrap();
        assert!(!second, "resolving an already-resolved ask by nonce must lose cleanly");

        let got = asks::get(&pool, raised.ask_id).await.unwrap().unwrap();
        assert_eq!(got.resolved_by.as_deref(), Some("matrix/@horst:kastellan.dev"),
            "a losing nonce resolve must not overwrite the winner");
        assert_eq!(got.resolution, Some(serde_json::json!({"choice": "approve"})));
    });
}

// ---------------------------------------------------------------------
// the one-pending-ask-per-task invariant is now
// a database fact (`asks_one_pending_per_task`), not just a theorem about
// `raise`'s guard.
// ---------------------------------------------------------------------

#[test]
fn the_one_pending_ask_per_task_index_rejects_a_second_pending_ask() {
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
        "askp-d",
        "askp-l",
        &format!("kastellan-supervisor-test-pg-askp-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        use kastellan_db::tasks;
        use kastellan_db::tasks::Lane;

        kastellan_db::probe::run(
            &cluster.conn_spec, "core", "startup",
            serde_json::json!({"version": "test", "purpose": "asks-one-pending-e2e"}),
        ).await.expect("probe run");
        let pool = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await.expect("admin pool");

        let task_id = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "double pending probe"}),
        ).await.unwrap();

        // Insert directly via SQL, bypassing `raise`'s application-level
        // guard entirely — this test is about the DATABASE constraint,
        // not the API that happens to also prevent this today.
        sqlx::query(
            "INSERT INTO asks (task_id, kind, body, options, nonce_sha256, deadline_at) \
             VALUES ($1, 'plan_approval', 'first', '[\"approve\"]'::jsonb, 'one-pending-nonce-hash-1', \
                     now() + interval '1 hour')",
        )
        .bind(task_id)
        .execute(&pool)
        .await
        .expect("first pending ask insert");

        let second = sqlx::query(
            "INSERT INTO asks (task_id, kind, body, options, nonce_sha256, deadline_at) \
             VALUES ($1, 'plan_approval', 'second', '[\"approve\"]'::jsonb, 'one-pending-nonce-hash-2', \
                     now() + interval '1 hour')",
        )
        .bind(task_id)
        .execute(&pool)
        .await;
        let err = second.expect_err(
            "a second PENDING ask on the same task must be rejected by asks_one_pending_per_task",
        );
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("asks_one_pending_per_task") || msg.contains("duplicate key"),
            "unexpected error (expected a unique-violation on asks_one_pending_per_task): {msg}",
        );

        // Exactly one row exists — the rejected INSERT wrote nothing.
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM asks WHERE task_id = $1")
            .bind(task_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
    });
}

// ---------------------------------------------------------------------
// all `asks_e2e` tests above run as the cluster
// superuser via `connect_admin_pool`. The daemon runs every `asks`
// statement as `kastellan_runtime` — this pins the grants 0023 actually
// declares, mirroring `postgres_e2e.rs`'s
// `runtime_role_audit_log_revoke_is_enforced`.
// ---------------------------------------------------------------------

#[test]
fn asks_table_grants_match_the_runtime_role_contract() {
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
        "askg-d",
        "askg-l",
        &format!("kastellan-supervisor-test-pg-askg-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        use kastellan_db::tasks;
        use kastellan_db::tasks::Lane;

        // Probe applies all migrations (incl. 0023) and creates the
        // kastellan_runtime role + its asks grants.
        kastellan_db::probe::run(
            &cluster.conn_spec, "core", "startup",
            serde_json::json!({"version": "test", "purpose": "asks-grants-e2e"}),
        ).await.expect("probe run");

        let pool = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await.expect("admin pool");

        // Seed a task as the (superuser) admin pool before dropping
        // privilege — `asks.task_id` has a FK to `tasks`, and INSERT into
        // `tasks` is not what this test is checking.
        let task_id = tasks::insert_pending(
            &pool, Lane::Fast, serde_json::json!({"instruction": "grants probe"}),
        ).await.unwrap();

        // ---------- SET ROLE on a held connection ----------
        let mut held = pool.acquire().await.expect("acquire connection");
        sqlx::query(sqlx::AssertSqlSafe(kastellan_db::conn::set_role_runtime_statement()))
            .execute(&mut *held)
            .await
            .expect("SET ROLE kastellan_runtime");

        // ---------- positive path: INSERT/SELECT/UPDATE succeed ----------
        let inserted: (i64,) = sqlx::query_as(
            "INSERT INTO asks (task_id, kind, body, options, nonce_sha256, deadline_at) \
             VALUES ($1, 'plan_approval', 'b', '[\"approve\"]'::jsonb, 'grants-probe-nonce-hash', \
                     now() + interval '1 hour') \
             RETURNING id",
        )
        .bind(task_id)
        .fetch_one(&mut *held)
        .await
        .expect("INSERT asks under runtime role");
        let ask_id = inserted.0;

        let (state,): (String,) = sqlx::query_as("SELECT state FROM asks WHERE id = $1")
            .bind(ask_id)
            .fetch_one(&mut *held)
            .await
            .expect("SELECT asks under runtime role");
        assert_eq!(state, "pending");

        sqlx::query("UPDATE asks SET state = 'cancelled' WHERE id = $1")
            .bind(ask_id)
            .execute(&mut *held)
            .await
            .expect("UPDATE asks under runtime role");

        // ---------- negative path: DELETE denied ----------
        let del_err = sqlx::query("DELETE FROM asks WHERE id = $1")
            .bind(ask_id)
            .execute(&mut *held)
            .await
            .expect_err("DELETE asks must be rejected under runtime role");
        let del_msg = del_err.to_string().to_lowercase();
        assert!(
            del_msg.contains("permission denied"),
            "expected 'permission denied' in error, got: {del_msg}"
        );
    });
}

// ---------------------------------------------------------------------------
// Fix-wave 2 (post-whole-branch review). Each of these closes a gap where the
// production code could be mutated without any existing test noticing.
// ---------------------------------------------------------------------------

/// Boilerplate shared by the tests below: bring up a cluster and a runtime.
///
/// Extracted only after the eleventh copy — the earlier tests keep their
/// inline form so this refactor stays additive and cannot perturb them.
///
/// Deliberately does **not** hold the `PgPool`. Struct fields drop in
/// declaration order, so a pool stored beside the cluster would outlive it
/// on some orderings and be dropped outside the runtime on all of them —
/// tearing PG down under live connections. Each test builds its pool inside
/// `rt.block_on`, exactly as the inline tests above do, so it is dropped on
/// the runtime and before the cluster.
struct Harness {
    cluster: kastellan_tests_common::PgCluster,
    rt: tokio::runtime::Runtime,
}

impl Harness {
    /// Apply migrations and hand back an admin pool. Call inside
    /// `rt.block_on` so the pool is a local of the async block and drops on
    /// the runtime, before the cluster.
    async fn migrated_pool(&self, purpose: &str) -> sqlx::PgPool {
        kastellan_db::probe::run(
            &self.cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": purpose}),
        )
        .await
        .expect("probe run");
        kastellan_db::pool::connect_admin_pool(&self.cluster.conn_spec)
            .await
            .expect("admin pool")
    }
}

fn harness(tag: &str) -> Option<Harness> {
    if skip_if_no_supervisor() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("{tag}-d"),
        &format!("{tag}-l"),
        &format!("kastellan-supervisor-test-pg-{tag}-{suffix}"),
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    Some(Harness { cluster, rt })
}

/// A task may escalate MORE THAN ONCE over its life, and that is what makes
/// `asks_one_pending_per_task` partial rather than a plain unique index.
///
/// Nothing pinned the partiality before this: the index's own test inserts
/// two `pending` rows, which a plain `UNIQUE (task_id)` rejects identically,
/// and every other test used a fresh task per ask. So dropping
/// `WHERE state = 'pending'` from the migration passed the entire suite —
/// while in production it would let a task's FIRST escalation succeed and
/// every later one fail forever, which is a mainline slice-1b flow (approved
/// on plan 2, escalating again on plan 4).
#[test]
fn a_task_can_raise_a_second_ask_once_the_first_is_resolved() {
    let Some(h) = harness("askrr") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-re-raise-e2e").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let task_id = tasks::insert_pending(
            pool, Lane::Fast, serde_json::json!({"instruction": "re-raise probe"}),
        ).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();

        let first = asks::raise(
            pool, task_id, "plan_approval", "approve plan 2?",
            &serde_json::json!(["approve", "deny"]), Some("digest-plan-2"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        ).await.expect("first raise");

        assert!(asks::resolve_with_nonce(
            pool, &first.nonce, "operator/cli", &serde_json::json!({"choice": "approve"}),
        ).await.unwrap());

        // Resolved → task back to `pending` → re-claim → escalate again.
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();
        let second = asks::raise(
            pool, task_id, "plan_approval", "approve plan 4?",
            &serde_json::json!(["approve", "deny"]), Some("digest-plan-4"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        ).await.expect(
            "a task whose first ask is RESOLVED must be able to raise another — \
             if this fails, asks_one_pending_per_task lost its WHERE clause",
        );

        assert_ne!(first.ask_id, second.ask_id, "a distinct row, not an overwrite");
        assert_ne!(
            first.nonce.expose(), second.nonce.expose(),
            "each ask gets its own capability",
        );

        // The history accumulates: no DELETE grant, so both rows survive.
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM asks WHERE task_id = $1")
            .bind(task_id).fetch_one(pool).await.unwrap();
        assert_eq!(rows, 2, "the task's ask history must accumulate, not be replaced");
        assert_eq!(asks::get(pool, first.ask_id).await.unwrap().unwrap().state, "resolved");
        assert_eq!(asks::get(pool, second.ask_id).await.unwrap().unwrap().state, "pending");

        // And the resolved one stays resolved with its ORIGINAL digest — the
        // second raise must not have disturbed it.
        assert_eq!(
            asks::get(pool, first.ask_id).await.unwrap().unwrap().plan_digest.as_deref(),
            Some("digest-plan-2"),
        );
    });
}

/// `mark_cancelled`'s ask-cancel must roll back when the task turns out not
/// to be cancellable — the central claim of its lock-order doc block, and
/// previously untested because every test only exercised the *cancellable*
/// branch.
///
/// The mutation this kills: passing `&*pool` instead of `&mut *tx` to
/// `cancel_for_task`. That compiled while `E: Executor` accepted a pool, and
/// the whole suite passed — leaving a `mark_cancelled` that returns
/// `Ok(None)` having nonetheless committed the ask cancel, i.e. an ask
/// cancelled out from under a task that is still alive. (The signature is
/// now `&mut PgConnection`, so that exact mutation no longer compiles; this
/// pins the *behaviour* so a future widening cannot silently restore it.)
#[test]
fn an_uncancellable_task_rolls_its_ask_cancel_back() {
    let Some(h) = harness("askrb") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-cancel-rollback-e2e").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let task_id = tasks::insert_pending(
            pool, Lane::Fast, serde_json::json!({"instruction": "rollback probe"}),
        ).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve"]), None,
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        ).await.unwrap();

        // Move the task to a terminal state behind the API's back, so
        // `mark_cancelled`'s guarded UPDATE matches nothing while the ask is
        // still pending. Raw SQL deliberately: this is a state the API will
        // not itself produce, and constructing it is the whole point.
        sqlx::query("UPDATE tasks SET state = 'failed' WHERE id = $1")
            .bind(task_id).execute(pool).await.unwrap();

        assert!(
            tasks::mark_cancelled(pool, task_id).await.unwrap().is_none(),
            "a failed task is not cancellable",
        );

        // THE assertion: the ask cancel went with the rollback.
        assert_eq!(
            asks::get(pool, raised.ask_id).await.unwrap().unwrap().state, "pending",
            "the ask cancel must roll back with the task UPDATE that never happened",
        );
    });
}

/// A nonce is the capability for **one** ask, and the doc claims a peer
/// holding one "cannot resolve (or even discover) anyone else's".
///
/// The existing wrong-nonce test aims an *unissued* nonce at a table holding
/// exactly one ask, which proves something weaker. This constructs the actual
/// threat: two live asks, and a legitimate nonce for A pointed at B.
#[test]
fn a_nonce_resolves_only_its_own_ask() {
    let Some(h) = harness("asksx") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-cross-nonce-e2e").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let raise_one = |instruction: &'static str| {
            let pool = pool.clone();
            async move {
                let tid = tasks::insert_pending(
                    &pool, Lane::Fast, serde_json::json!({"instruction": instruction}),
                ).await.unwrap();
                tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();
                let r = asks::raise(
                    &pool, tid, "plan_approval", "approve?",
                    &serde_json::json!(["approve", "deny"]), None,
                    time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
                ).await.unwrap();
                (tid, r)
            }
        };
        let (task_a, ask_a) = raise_one("peer A's task").await;
        let (task_b, ask_b) = raise_one("peer B's task").await;

        // A's nonce resolves A...
        assert!(asks::resolve_with_nonce(
            pool, &ask_a.nonce, "matrix/@a:kastellan.dev",
            &serde_json::json!({"choice": "approve"}),
        ).await.unwrap());

        // ...and B is untouched: still pending, still suspended, and its own
        // nonce still works. If `resolve_with_nonce` matched on anything but
        // the nonce, B would have moved too.
        let b = asks::get(pool, ask_b.ask_id).await.unwrap().unwrap();
        assert_eq!(b.state, "pending", "resolving A must not touch B");
        assert!(b.resolved_by.is_none());
        assert_eq!(tasks::observe_state(pool, task_b).await.unwrap(), "awaiting_operator");
        assert_eq!(tasks::observe_state(pool, task_a).await.unwrap(), "pending");

        // A's (now spent) nonce must not open B either.
        assert!(
            !asks::resolve_with_nonce(
                pool, &ask_a.nonce, "matrix/@a:kastellan.dev",
                &serde_json::json!({"choice": "approve"}),
            ).await.unwrap(),
            "a spent nonce must not resolve anything, least of all another ask",
        );
        assert_eq!(asks::get(pool, ask_b.ask_id).await.unwrap().unwrap().state, "pending");

        // B's own nonce still works, so the failed attempt cost B nothing.
        assert!(asks::resolve_with_nonce(
            pool, &ask_b.nonce, "matrix/@b:kastellan.dev",
            &serde_json::json!({"choice": "deny"}),
        ).await.unwrap());
    });
}

/// `list_pending` is the operator inbox: oldest first, tie-broken by id,
/// capped at `limit`.
///
/// Every previous assertion ran against a table holding exactly ONE ask, so
/// `ORDER BY … DESC`, dropping the `id ASC` tiebreaker, and removing the
/// `LIMIT` all passed.
#[test]
fn list_pending_is_oldest_first_tie_broken_by_id_and_honours_the_limit() {
    let Some(h) = harness("asklp") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-list-pending-e2e").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let mut ids = Vec::new();
        for i in 0..4 {
            let tid = tasks::insert_pending(
                pool, Lane::Fast, serde_json::json!({"instruction": format!("inbox {i}")}),
            ).await.unwrap();
            tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();
            let r = asks::raise(
                pool, tid, "plan_approval", &format!("question {i}"),
                &serde_json::json!(["approve"]), None,
                time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
            ).await.unwrap();
            ids.push(r.ask_id);
        }

        // Distinct created_at for the first two, and a deliberate TIE for the
        // last two so the `id ASC` tiebreaker is the only thing deciding
        // their order. Without it that pair is nondeterministic across calls,
        // which is exactly what the production doc claims the tiebreaker
        // prevents.
        sqlx::query("UPDATE asks SET created_at = now() - interval '3 hours' WHERE id = $1")
            .bind(ids[0]).execute(pool).await.unwrap();
        sqlx::query("UPDATE asks SET created_at = now() - interval '2 hours' WHERE id = $1")
            .bind(ids[1]).execute(pool).await.unwrap();
        sqlx::query(
            "UPDATE asks SET created_at = now() - interval '1 hour' WHERE id = ANY($1)",
        ).bind(vec![ids[2], ids[3]]).execute(pool).await.unwrap();

        let all = asks::list_pending(pool, 100).await.unwrap();
        assert_eq!(
            all.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![ids[0], ids[1], ids[2], ids[3]],
            "oldest first, with the tied pair ordered by id",
        );

        // The cap withholds the NEWEST, never the oldest — the oldest ask is
        // the one holding a task up longest.
        let capped = asks::list_pending(pool, 2).await.unwrap();
        assert_eq!(
            capped.iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![ids[0], ids[1]],
            "the limit must keep the two OLDEST",
        );

        // A negative limit clamps to zero rather than erroring out of PG.
        assert!(asks::list_pending(pool, -1).await.unwrap().is_empty());

        // Resolved asks leave the inbox.
        let first_nonce_ask = all[0].id;
        sqlx::query(
            "UPDATE asks SET state='cancelled' WHERE id = $1",
        ).bind(first_nonce_ask).execute(pool).await.unwrap();
        let after = asks::list_pending(pool, 100).await.unwrap();
        assert_eq!(after.len(), 3, "a non-pending ask must leave the inbox");
        assert!(!after.iter().any(|a| a.id == first_nonce_ask));
    });
}

/// The deadline is enforced by the RESOLVERS, not only by `expire_due`.
///
/// Without the `deadline_at > now()` predicate the bound is only as good as
/// the sweeper's liveness — and a nonce delivered into durable Matrix room
/// history stays a live approval token for as long as the sweep is down.
/// Nothing calls `expire_due` in production yet, which makes this the only
/// thing currently bounding an approval in time.
#[test]
fn an_ask_past_its_deadline_cannot_be_resolved_even_before_the_sweep_runs() {
    let Some(h) = harness("askdl") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-deadline-e2e").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let task_id = tasks::insert_pending(
            pool, Lane::Fast, serde_json::json!({"instruction": "deadline probe"}),
        ).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve", "deny"]), None,
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        ).await.unwrap();

        // Backdate ONLY the deadline. The ask stays `pending` and the task
        // stays `awaiting_operator` — the sweep has deliberately NOT run, so
        // the only thing that can refuse this resolve is the predicate.
        sqlx::query(
            // Both columns: `asks_deadline_after_created` rejects a deadline
            // earlier than the row's own creation.
            "UPDATE asks SET created_at = now() - interval '2 hours', \
                             deadline_at = now() - interval '1 hour' WHERE id = $1",
        ).bind(raised.ask_id).execute(pool).await.unwrap();
        assert_eq!(asks::get(pool, raised.ask_id).await.unwrap().unwrap().state, "pending");

        assert!(
            !asks::resolve_with_nonce(
                pool, &raised.nonce, "matrix/@late:kastellan.dev",
                &serde_json::json!({"choice": "approve"}),
            ).await.unwrap(),
            "the correct nonce must NOT resolve an ask past its deadline",
        );
        assert!(
            !asks::resolve(
                pool, raised.ask_id, "operator/cli",
                &serde_json::json!({"choice": "approve"}),
            ).await.unwrap(),
            "nor must the operator-CLI by-id path",
        );

        // Nothing was written, and the task did NOT resume — a late approval
        // must not re-enqueue a plan the operator never validly approved.
        let after = asks::get(pool, raised.ask_id).await.unwrap().unwrap();
        assert_eq!(after.state, "pending");
        assert!(after.resolved_by.is_none(), "a refused resolve must write nothing");
        assert!(after.resolution.is_none());
        assert_eq!(tasks::observe_state(pool, task_id).await.unwrap(), "awaiting_operator");

        // The sweep then does its own half of the job: unwedge the task.
        let expired = asks::expire_due(pool).await.unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(tasks::observe_state(pool, task_id).await.unwrap(), "failed");
    });
}

/// `resolution` is a CLOSED set: `choice` must name one of the ask's own
/// `options`. The schema said so in a comment and nothing enforced it, which
/// matters because the idiomatic slice-1b read
/// (`…get("choice")…as_str() == Some("deny")`) puts every malformed value in
/// the PROCEED arm.
#[test]
fn a_choice_outside_the_asks_own_options_is_refused() {
    let Some(h) = harness("askch") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-choice-e2e").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let task_id = tasks::insert_pending(
            pool, Lane::Fast, serde_json::json!({"instruction": "choice probe"}),
        ).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve", "deny"]), None,
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        ).await.unwrap();

        // Each of these is a DIFFERENT way to miss, and each used to be
        // stored verbatim.
        for bad in [
            serde_json::json!({"choice": "aprove"}),        // typo
            serde_json::json!({"choice": "escalate"}),      // never offered
            serde_json::json!({"free_text": "yes please"}), // no choice at all
            serde_json::json!({}),                          // empty
            serde_json::json!({"choice": 1}),               // right key, wrong type
        ] {
            let err = asks::resolve_with_nonce(
                pool, &raised.nonce, "matrix/@peer:evil.example", &bad,
            ).await.expect_err(
                "a resolution naming no offered option must be an Err, not a stored decision",
            );
            let msg = err.to_string();
            assert!(
                msg.contains(&raised.ask_id.to_string()),
                "the error must name the ask: {msg}",
            );

            // Rolled back: still pending, nothing written, task still parked.
            let after = asks::get(pool, raised.ask_id).await.unwrap().unwrap();
            assert_eq!(after.state, "pending", "a refused resolution writes nothing");
            assert!(after.resolution.is_none(), "left {:?}", after.resolution);
            assert!(after.resolved_by.is_none());
            assert_eq!(
                tasks::observe_state(pool, task_id).await.unwrap(), "awaiting_operator",
                "a refused resolution must not resume the task",
            );
        }

        // And a legitimate choice still works, so the guard is not simply
        // refusing everything.
        assert!(asks::resolve_with_nonce(
            pool, &raised.nonce, "matrix/@horst:kastellan.dev",
            &serde_json::json!({"choice": "deny", "free_text": "not this time"}),
        ).await.unwrap());
        let done = asks::get(pool, raised.ask_id).await.unwrap().unwrap();
        assert_eq!(done.state, "resolved");
        assert_eq!(
            done.resolution,
            Some(serde_json::json!({"choice": "deny", "free_text": "not this time"})),
            "free_text rides along with a valid choice",
        );
    });
}

/// The schema's own invariants, each asserted against a direct INSERT/UPDATE
/// so the CHECK is what rejects it — not a Rust-side guard that happens to
/// run first.
#[test]
fn the_asks_schema_rejects_the_states_its_checks_exist_to_forbid() {
    let Some(h) = harness("askck") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-checks-e2e").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let task_id = tasks::insert_pending(
            pool, Lane::Fast, serde_json::json!({"instruction": "checks probe"}),
        ).await.unwrap();

        let insert = |body: &'static str, kind: &'static str, options: &'static str,
                      deadline: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "INSERT INTO asks (task_id, kind, body, options, nonce_sha256, deadline_at) \
                     VALUES ($1, '{kind}', '{body}', '{options}'::jsonb, '{body}-hash', {deadline})"
                )))
                .bind(task_id)
                .execute(&pool)
                .await
            }
        };

        // kind is a closed set — a typo'd kind is a silent no-op in 1b's
        // dispatch, on a question a human was asked to answer.
        let e = insert("k1", "plan_aproval", r#"["approve"]"#, "now() + interval '1 hour'")
            .await.expect_err("an unknown kind must be rejected");
        assert!(e.to_string().to_lowercase().contains("asks_kind_check"), "{e}");

        // options must be a NON-EMPTY array — an ask with no answers is
        // unanswerable, so the task waits out the full deadline for a reply
        // that cannot be given.
        for (label, opts) in [("empty", "[]"), ("scalar", r#""approve""#), ("null", "null")] {
            let e = insert("k2", "plan_approval", opts, "now() + interval '1 hour'")
                .await
                .unwrap_err();
            // Name the constraint, not just "options": a NOT NULL violation
            // would also mention the column, so a substring match on it
            // would pass for the wrong reason.
            assert!(
                e.to_string().to_lowercase().contains("asks_options_check"),
                "{label} options must be rejected by asks_options_check, got: {e}",
            );
        }

        // A deadline that has already passed is always a caller bug, and is
        // compared against PG's clock so a daemon whose clock trails the
        // database's cannot mint an already-expirable ask.
        let e = insert("k3", "plan_approval", r#"["approve"]"#, "now() - interval '1 second'")
            .await.expect_err("a past deadline must be rejected");
        assert!(
            e.to_string().to_lowercase().contains("asks_deadline_after_created"),
            "{e}",
        );

        // `resolved` is all-or-nothing. `kastellan_runtime` holds blanket
        // UPDATE, so "only the Rust path writes all four together" is a
        // property of today's callers, not of the table.
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve"]), None,
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        ).await.unwrap();

        let e = sqlx::query("UPDATE asks SET state = 'resolved' WHERE id = $1")
            .bind(raised.ask_id).execute(pool).await
            .expect_err("a half-resolved ask must be rejected");
        assert!(
            e.to_string().to_lowercase().contains("asks_resolved_is_complete"),
            "{e}",
        );

        // ...and the complete form is accepted, so the CHECK is not simply
        // forbidding `resolved`.
        sqlx::query(
            "UPDATE asks SET state='resolved', resolved_at=now(), resolved_by='operator/cli', \
             resolution='{\"choice\":\"approve\"}'::jsonb WHERE id = $1",
        ).bind(raised.ask_id).execute(pool).await.expect("a complete resolution is legal");
    });
}

/// Expiry must fire `tasks_completed`, because that is the notification the
/// Matrix/email reply pump (`core::channel::bus`) and `memory l3 run` both
/// wait on. Migration 0023 asserts this in prose — that `awaiting_operator`
/// is absent from `notify_task_completed`'s OLD.state list, so the
/// `awaiting_operator → failed` transition still fires — and nothing tested
/// it. If it did not fire, `expire_due`'s whole purpose (unwedging a task
/// nobody answered) would be defeated for the human, who would never be told.
#[test]
fn expiring_an_ask_fires_tasks_completed_so_the_channel_can_reply() {
    let Some(h) = harness("askntc") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-notify-completed-e2e").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};
        use sqlx::postgres::PgListener;

        let task_id = tasks::insert_pending(
            pool, Lane::Fast, serde_json::json!({"instruction": "notify probe"}),
        ).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve"]), None,
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        ).await.unwrap();

        // Subscribe BEFORE the transition: PG does not queue notifications
        // for a listener that was not yet listening, so a LISTEN afterwards
        // would pass whether or not the trigger fired.
        let mut listener = PgListener::connect_with(pool).await.expect("listener");
        listener.listen("tasks_completed").await.expect("LISTEN tasks_completed");

        sqlx::query(
            // Both columns: `asks_deadline_after_created` rejects a deadline
            // earlier than the row's own creation.
            "UPDATE asks SET created_at = now() - interval '2 hours', \
                             deadline_at = now() - interval '1 hour' WHERE id = $1",
        ).bind(raised.ask_id).execute(pool).await.unwrap();
        assert_eq!(asks::expire_due(pool).await.unwrap().len(), 1);

        let notification = tokio::time::timeout(
            std::time::Duration::from_secs(5), listener.recv(),
        ).await
            .expect("tasks_completed must fire within 5 s — a longer wait would be a slow box, \
                    no notification at all is a missing trigger")
            .expect("listener recv");
        assert_eq!(notification.channel(), "tasks_completed");
        assert_eq!(
            notification.payload(), task_id.to_string(),
            "the notification must name the expired task",
        );
    });
}

/// **The C1 race, reproduced as a test.** A `mark_cancelled` racing a
/// `raise` on the same task must not leave a `pending` ask on a `cancelled`
/// task.
///
/// This is the defect the asks→tasks lock reorder introduced while fixing a
/// real deadlock, and it was reproduced against a live PG 18 before the fix
/// was written. The window: `mark_cancelled` sweeps `asks` BEFORE it holds
/// the tasks row lock, so under READ COMMITTED an ask a concurrent `raise`
/// is about to insert is invisible to that sweep, while the tasks UPDATE
/// afterwards re-checks against the now-committed `awaiting_operator` and
/// cancels anyway. Result before the fix: task `cancelled`, ask `pending` —
/// a live question on a dead task, resolvable, and resolving it returns the
/// invariant-violation `Err` rather than a clean `false`.
///
/// The interleaving is **established, not hoped for**: the test waits until
/// `mark_cancelled` is provably blocked on the tasks row (via
/// `pg_stat_activity`) before letting the raiser insert its ask. Without
/// that wait this test would pass whether or not the second sweep exists,
/// because a `mark_cancelled` that had not yet started would see the ask in
/// its FIRST sweep — a check that cannot fail.
///
/// Mutation that must fail this test: delete the second
/// `cancel_for_task` call in `tasks::mark_cancelled`.
#[test]
fn a_cancel_racing_a_raise_does_not_strand_the_ask() {
    let Some(h) = harness("askrace") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-cancel-race-e2e").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let task_id = tasks::insert_pending(
            pool, Lane::Fast, serde_json::json!({"instruction": "race probe"}),
        ).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();

        // Connection A plays `asks::raise`'s first half: take the tasks row
        // lock and suspend, but do NOT commit yet.
        let mut raiser = pool.acquire().await.expect("raiser connection");
        sqlx::query("BEGIN").execute(&mut *raiser).await.unwrap();
        let suspended = sqlx::query(
            "UPDATE tasks SET state='awaiting_operator', lease_expires_at=NULL \
             WHERE id = $1 AND state = 'running'",
        ).bind(task_id).execute(&mut *raiser).await.unwrap();
        assert_eq!(suspended.rows_affected(), 1, "test setup: the task must suspend");

        // Now run the cancel concurrently. It sweeps `asks` (finding
        // nothing — the ask is not inserted yet) and then blocks on the
        // tasks row that `raiser` holds.
        let cancel_pool = pool.clone();
        let canceller =
            tokio::spawn(async move { tasks::mark_cancelled(&cancel_pool, task_id).await });

        // Prove it is actually blocked before proceeding. Polling rather
        // than sleeping: a fixed sleep that is too short makes this test
        // silently vacuous instead of failing.
        //
        // On its OWN pool. `connect_admin_pool` caps at 2 connections, and
        // both are already spoken for here — `raiser` holds one and the
        // blocked `mark_cancelled` holds the other — so observing through
        // `pool` starves on its own setup and times out.
        let observer = kastellan_db::pool::connect_admin_pool(&h.cluster.conn_spec)
            .await
            .expect("observer pool");
        let mut blocked = false;
        for _ in 0..200 {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() \
                   AND wait_event_type = 'Lock' \
                   AND state = 'active'",
            ).fetch_one(&observer).await.unwrap();
            if waiting >= 1 {
                blocked = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            blocked,
            "mark_cancelled never blocked on the tasks row within 10 s — the interleaving \
             this test exists to construct did not happen, so a pass would prove nothing",
        );

        // `raise`'s second half: the ask lands AFTER the cancel's first
        // sweep already ran and found nothing.
        sqlx::query(
            "INSERT INTO asks (task_id, kind, body, options, nonce_sha256, deadline_at) \
             VALUES ($1, 'plan_approval', 'approve?', '[\"approve\"]'::jsonb, \
                     'race-nonce-hash', now() + interval '1 hour')",
        ).bind(task_id).execute(&mut *raiser).await.unwrap();
        sqlx::query("COMMIT").execute(&mut *raiser).await.unwrap();
        drop(raiser);

        let cancellation = canceller
            .await
            .expect("cancel task join")
            .expect("mark_cancelled must not error")
            .expect("the task was awaiting_operator, so it is cancellable");

        assert_eq!(cancellation.task.state, "cancelled");

        // THE assertion: the ask raised inside the window went with it.
        let ask_state: String =
            sqlx::query_scalar("SELECT state FROM asks WHERE task_id = $1")
                .bind(task_id).fetch_one(pool).await.unwrap();
        assert_eq!(
            ask_state, "cancelled",
            "an ask committed while mark_cancelled was blocked must still be cancelled — \
             otherwise a live question is left attached to a dead task",
        );
        assert_eq!(
            cancellation.asks_cancelled, 1,
            "the second sweep's count must reach the caller, so the audit row names it",
        );

        // And the pre-cancel state is the one the UPDATE actually saw, not
        // the stale snapshot from before the raiser committed. A plain
        // (non-`FOR UPDATE`) read reports `running` here, which would tell
        // the audit emitter a scheduler inner loop is going to finalize this
        // task when there is none.
        assert_eq!(
            cancellation.previous_state, "awaiting_operator",
            "previous_state must reflect the committed state the cancel raced with",
        );

        // Nothing is left resolvable.
        assert!(
            !asks::resolve(
                pool,
                sqlx::query_scalar::<_, i64>("SELECT id FROM asks WHERE task_id = $1")
                    .bind(task_id).fetch_one(pool).await.unwrap(),
                "operator/cli",
                &serde_json::json!({"choice": "approve"}),
            ).await.unwrap(),
            "a cancelled ask must not be resolvable",
        );
    });
}

/// `finish_resolve`'s fail-closed `Err` arm — a pending ask whose task is
/// NOT `awaiting_operator` — is never reached by any other test, so
/// replacing its guard with `let _ = resume_from_ask(...)` changed no
/// outcome anywhere.
///
/// The arm is defensive and, since `asks_one_pending_per_task`, close to
/// unreachable through the API. Testing it rather than deleting it is the
/// better call: it also pins that the rollback leaves the ask **pending**
/// (recoverable) rather than resolved-with-no-task-to-resume, which is the
/// half that actually matters if the invariant ever does break.
#[test]
fn resolving_an_ask_whose_task_cannot_resume_fails_closed_and_rolls_back() {
    let Some(h) = harness("askfc") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-fail-closed-e2e").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let task_id = tasks::insert_pending(
            pool, Lane::Fast, serde_json::json!({"instruction": "fail-closed probe"}),
        ).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve"]), None,
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        ).await.unwrap();

        // Separate the pair behind the API's back: the ask stays `pending`
        // while its task leaves `awaiting_operator`. No supported path can
        // produce this — `cancel_for_task` and `expire_due` both move the
        // ask with the task — which is exactly why it needs raw SQL.
        sqlx::query("UPDATE tasks SET state = 'running' WHERE id = $1")
            .bind(task_id).execute(pool).await.unwrap();

        let err = asks::resolve_with_nonce(
            pool, &raised.nonce, "operator/cli", &serde_json::json!({"choice": "approve"}),
        ).await.expect_err(
            "a pending ask whose task cannot resume must fail LOUDLY, not return Ok(false) — \
             Ok(false) reads as 'you lost a race' and would hide a corrupt pair",
        );
        let msg = err.to_string();
        assert!(msg.contains(&raised.ask_id.to_string()), "error must name the ask: {msg}");
        assert!(msg.contains(&task_id.to_string()), "error must name the task: {msg}");

        // THE other half: the rollback left the ask recoverable. Committing
        // instead would have left it `resolved` with no task to resume.
        let after = asks::get(pool, raised.ask_id).await.unwrap().unwrap();
        assert_eq!(
            after.state, "pending",
            "the rollback must leave the ask pending (recoverable), not resolved",
        );
        assert!(after.resolved_by.is_none());
        assert!(after.resolution.is_none());
        assert_eq!(tasks::observe_state(pool, task_id).await.unwrap(), "running");
    });
}

/// `raise` must persist the `body` and `options` it was given.
///
/// Neither field was ever read back: the decode assertions covered
/// `task_id`/`kind`/`state`/`plan_digest`/`resolved_at` only, so binding
/// `kind` twice (making `body` the literal `"plan_approval"`) or a constant
/// `options` survived the whole suite. `options` is the set the operator's
/// `choice` indexes into and which `resolve` now validates against, so a
/// constant there would show the operator the wrong question AND silently
/// change which answers are accepted.
#[test]
fn raise_persists_the_body_and_options_it_was_given() {
    let Some(h) = harness("askbo") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-body-options-e2e").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let task_id = tasks::insert_pending(
            pool, Lane::Fast, serde_json::json!({"instruction": "body probe"}),
        ).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();

        let body = "CASSANDRA escalated: send 2 clinical PDFs to an off-allowlist address?";
        let options = serde_json::json!(["approve", "deny", "approve_without_attachments"]);
        let raised = asks::raise(
            pool, task_id, "plan_approval", body, &options, Some("digest-xyz"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        ).await.unwrap();

        let got = asks::get(pool, raised.ask_id).await.unwrap().unwrap();
        assert_eq!(got.body, body, "the operator must be shown the question that was asked");
        assert_eq!(got.options, options, "verbatim, in order");
        assert_eq!(got.kind, "plan_approval");
        assert_eq!(got.plan_digest.as_deref(), Some("digest-xyz"));
        assert!(got.created_at <= got.deadline_at);

        // The third option is genuinely accepted — proving `options` reached
        // the resolver's validation, not just the row.
        assert!(asks::resolve_with_nonce(
            pool, &raised.nonce, "operator/cli",
            &serde_json::json!({"choice": "approve_without_attachments"}),
        ).await.unwrap());
    });
}

/// A `plan_digest` of `None` must round-trip as SQL `NULL`, not `""`.
///
/// `raised2` in the raise test is created with `None` and never read back,
/// so `.bind(plan_digest.unwrap_or_default())` — storing the empty string —
/// survived. Migration 0023 makes the nullability an explicit design point
/// for future non-plan ask kinds, and `""` would compare equal to no digest
/// at all in slice 1b's match.
#[test]
fn a_none_plan_digest_round_trips_as_sql_null() {
    let Some(h) = harness("asknd") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-null-digest-e2e").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let task_id = tasks::insert_pending(
            pool, Lane::Fast, serde_json::json!({"instruction": "null digest probe"}),
        ).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve"]), None,
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        ).await.unwrap();

        assert!(asks::get(pool, raised.ask_id).await.unwrap().unwrap().plan_digest.is_none());
        // Asserted at the SQL level too: `Option<String>` decoding cannot
        // tell `NULL` from `''` once it is in Rust.
        let is_null: bool = sqlx::query_scalar(
            "SELECT plan_digest IS NULL FROM asks WHERE id = $1",
        ).bind(raised.ask_id).fetch_one(pool).await.unwrap();
        assert!(is_null, "an absent digest must be SQL NULL, never the empty string");
    });
}

/// Insert a pending task and claim it, so it is `running` — the state
/// `asks::raise` requires. Shared by the `resolved_for_task` tests
/// below, each of which needs a running task before it can raise anything.
async fn seed_running_task(pool: &sqlx::PgPool) -> i64 {
    use kastellan_db::tasks::{self, Lane};

    let task_id = tasks::insert_pending(
        pool, Lane::Fast, serde_json::json!({"instruction": "resolved_for_task probe"}),
    ).await.expect("insert pending");
    tasks::claim_one(pool, Lane::Fast, 60).await.expect("claim").expect("a pending task");
    task_id
}

/// A `pending` ask is not a resolved one — `resolved_for_task` must
/// not surface a question nobody has answered yet.
#[test]
fn resolved_for_task_is_empty_when_nothing_is_resolved() {
    let Some(h) = harness("asklrn") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-latest-none").await;
        let pool = &pool;
        use kastellan_db::asks;

        let task_id = seed_running_task(pool).await;

        // A pending ask is not a resolved ask.
        let _ = asks::raise(
            pool, task_id, "plan_approval", "why", &serde_json::json!(["approve", "deny"]),
            Some("digest-a"), time::OffsetDateTime::now_utc() + time::Duration::hours(1),
        ).await.expect("raise");

        let got = asks::resolved_for_task(pool, task_id).await.expect("read");
        assert!(got.is_empty(), "a pending ask must not be returned as resolved");
    });
}

/// A task that escalates twice must see BOTH decisions, newest first.
///
/// The count is the load-bearing half. A `LIMIT 1` read returns the second
/// ask here too and would satisfy any assertion about `[0]` alone — which is
/// exactly how the two-escalation livelock survived review. Assert the
/// length, then the order.
#[test]
fn resolved_for_task_returns_every_resolution_newest_first() {
    let Some(h) = harness("asklrr") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-latest-recent").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let task_id = seed_running_task(pool).await;

        // First ask: raised, approved. Resolving it returns the task to
        // `pending`.
        let first = asks::raise(
            pool, task_id, "plan_approval", "first concern",
            &serde_json::json!(["approve", "deny"]), Some("digest-a"),
            time::OffsetDateTime::now_utc() + time::Duration::hours(1),
        ).await.expect("raise 1");
        assert!(asks::resolve(pool, first.ask_id, "operator",
            &serde_json::json!({"choice": "approve"})).await.expect("resolve 1"));

        // Second ask on the same task, denied. `raise` needs `running`, so
        // re-claim the task the resolve just re-enqueued.
        tasks::claim_one(pool, Lane::Fast, 60).await.expect("claim").expect("a task");
        let second = asks::raise(
            pool, task_id, "plan_approval", "second concern",
            &serde_json::json!(["approve", "deny"]), Some("digest-b"),
            time::OffsetDateTime::now_utc() + time::Duration::hours(1),
        ).await.expect("raise 2");
        assert!(asks::resolve(pool, second.ask_id, "operator",
            &serde_json::json!({"choice": "deny"})).await.expect("resolve 2"));

        let got = asks::resolved_for_task(pool, task_id).await.expect("read");
        assert_eq!(
            got.len(), 2,
            "BOTH resolutions must come back — a LIMIT 1 read is what makes a task that \
             escalates at two plans re-ask the first one forever",
        );
        assert_eq!(got[0].id, second.ask_id, "newest first");
        assert_eq!(got[0].plan_digest.as_deref(), Some("digest-b"));
        assert_eq!(got[1].id, first.ask_id, "the older decision is still visible");
        assert_eq!(got[1].plan_digest.as_deref(), Some("digest-a"));
    });
}

/// An `expired` ask is a timeout, not a decision — `resolved_for_task`
/// must not let one read as an answer.
#[test]
fn resolved_for_task_ignores_expired_and_cancelled_asks() {
    let Some(h) = harness("asklrs") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-latest-states").await;
        let pool = &pool;
        use kastellan_db::{asks, tasks};

        // ---- expired half: deadline one second out, then swept. `expire_due`
        // fails the task closed (terminal), so this needs its own task —
        // a cancelled ask cannot follow it on the same one.
        let expired_task_id = seed_running_task(pool).await;
        let _ = asks::raise(
            pool, expired_task_id, "plan_approval", "why",
            &serde_json::json!(["approve", "deny"]),
            Some("digest-a"), time::OffsetDateTime::now_utc() + time::Duration::seconds(1),
        ).await.expect("raise");
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let expired = asks::expire_due(pool).await.expect("expire");
        assert_eq!(expired.len(), 1, "the ask must have been swept");

        let got = asks::resolved_for_task(pool, expired_task_id).await.expect("read");
        assert!(got.is_empty(), "an expired ask is not a resolution and must not be returned");

        // ---- cancelled half: reached via the real production path —
        // `tasks::mark_cancelled` cancelling a task that has a pending ask,
        // exactly as `asks::cancel_for_task`'s only caller does it — not a
        // hand-crafted `state = 'cancelled'` row.
        let cancelled_task_id = seed_running_task(pool).await;
        let raised = asks::raise(
            pool, cancelled_task_id, "plan_approval", "why too",
            &serde_json::json!(["approve", "deny"]),
            Some("digest-b"), time::OffsetDateTime::now_utc() + time::Duration::hours(1),
        ).await.expect("raise");
        let cancelled = tasks::mark_cancelled(pool, cancelled_task_id).await.expect("cancel")
            .expect("an awaiting_operator task must be cancellable");
        assert_eq!(cancelled.asks_cancelled, 1, "the pending ask must have gone with it");
        assert_eq!(
            asks::get(pool, raised.ask_id).await.unwrap().unwrap().state, "cancelled",
            "the ask must actually have reached the cancelled state for this test to mean anything",
        );

        let got = asks::resolved_for_task(pool, cancelled_task_id).await.expect("read");
        assert!(got.is_empty(), "a cancelled ask is not a resolution and must not be returned");
    });
}

/// `resolved_at` is `now()` at resolve time, so two asks resolved inside one
/// transaction tick genuinely CAN tie — the `, id DESC` half of the ORDER BY
/// is what breaks it, not decoration. Racing the clock to produce a real tie
/// would be flaky, so this forces one directly (per the review finding: `UPDATE
/// asks SET resolved_at = $1 WHERE task_id = $2`) and asserts the higher-id ask
/// wins, repeatably — a missing tiebreaker would make the result
/// nondeterministic across calls (dependent on physical row/index order)
/// rather than simply wrong once, so a single call would not reliably catch
/// its absence.
#[test]
fn resolved_for_task_breaks_a_resolved_at_tie_by_higher_id() {
    let Some(h) = harness("asklrt") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-latest-tie").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let task_id = seed_running_task(pool).await;

        let first = asks::raise(
            pool, task_id, "plan_approval", "first concern",
            &serde_json::json!(["approve", "deny"]), Some("digest-a"),
            time::OffsetDateTime::now_utc() + time::Duration::hours(1),
        ).await.expect("raise 1");
        assert!(asks::resolve(pool, first.ask_id, "operator",
            &serde_json::json!({"choice": "approve"})).await.expect("resolve 1"));

        tasks::claim_one(pool, Lane::Fast, 60).await.expect("claim").expect("a task");
        let second = asks::raise(
            pool, task_id, "plan_approval", "second concern",
            &serde_json::json!(["approve", "deny"]), Some("digest-b"),
            time::OffsetDateTime::now_utc() + time::Duration::hours(1),
        ).await.expect("raise 2");
        assert!(asks::resolve(pool, second.ask_id, "operator",
            &serde_json::json!({"choice": "deny"})).await.expect("resolve 2"));
        assert!(
            second.ask_id > first.ask_id,
            "the second raise must get the higher id for this test to mean anything",
        );

        // Force the tie directly rather than racing the clock.
        let tied_at = time::OffsetDateTime::now_utc();
        sqlx::query("UPDATE asks SET resolved_at = $1 WHERE task_id = $2")
            .bind(tied_at)
            .bind(task_id)
            .execute(pool)
            .await
            .expect("force the resolved_at tie");
        let (a, b): (time::OffsetDateTime, time::OffsetDateTime) = sqlx::query_as(
            "SELECT \
             (SELECT resolved_at FROM asks WHERE id = $1), \
             (SELECT resolved_at FROM asks WHERE id = $2)",
        )
        .bind(first.ask_id)
        .bind(second.ask_id)
        .fetch_one(pool)
        .await
        .expect("read back the tied resolved_at");
        assert_eq!(a, b, "both asks must share exactly one resolved_at instant");

        // The higher-id ask must win, on repeated calls — a missing
        // `id DESC` tiebreaker is nondeterministic, not just wrong once.
        for attempt in 0..3 {
            let got = asks::resolved_for_task(pool, task_id).await.expect("read");
            assert_eq!(got.len(), 2, "attempt {attempt}: both resolutions are still returned");
            assert_eq!(
                got[0].id, second.ask_id,
                "attempt {attempt}: with a tied resolved_at, the higher id must sort first \
                 on every call",
            );
        }
    });
}
