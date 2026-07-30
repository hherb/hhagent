//! PG-gated e2e for `db::pairings` (migration 0018). Exercises the authorizer
//! read path, idempotent binding, revocation, the atomic single-use `claim_code`,
//! and the `any_active_code` gate against a live cluster. Skip-as-pass without a
//! supervisor/PG (root CI container / Mac); live on the DGX.

use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

#[test]
fn pairings_and_codes_round_trip() {
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
        "pair-d",
        "pair-l",
        &format!("kastellan-supervisor-test-pg-pair-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        use kastellan_db::pairings;

        // Migrate (creates the 0018 tables) via the probe.
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "pairings-e2e"}),
        )
        .await
        .expect("probe run");

        let pool = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await
            .expect("admin pool");

        // ---- pairings: bind / idempotent / authorize / revoke ----
        assert!(!pairings::is_paired(&pool, "matrix", "@a:srv").await.unwrap());
        assert!(pairings::insert_pairing(&pool, "matrix", "@a:srv", "code").await.unwrap());
        assert!(pairings::is_paired(&pool, "matrix", "@a:srv").await.unwrap());
        // Idempotent: a second active insert is a no-op.
        assert!(!pairings::insert_pairing(&pool, "matrix", "@a:srv", "code").await.unwrap());
        // Channel-scoped: same peer on another channel is independent.
        assert!(!pairings::is_paired(&pool, "email", "@a:srv").await.unwrap());
        // Revoke → no longer recognised; re-pair allowed afterwards.
        assert!(pairings::revoke_pairing(&pool, "matrix", "@a:srv").await.unwrap());
        assert!(!pairings::is_paired(&pool, "matrix", "@a:srv").await.unwrap());
        assert!(pairings::insert_pairing(&pool, "matrix", "@a:srv", "code").await.unwrap());
        assert!(pairings::is_paired(&pool, "matrix", "@a:srv").await.unwrap());

        // ---- codes: mint / single-use claim / any_active_code ----
        let hash = "a".repeat(64);
        assert!(!pairings::any_active_code(&pool).await.unwrap());
        pairings::insert_code(&pool, &hash, Some("alice"), 10).await.unwrap();
        assert!(pairings::any_active_code(&pool).await.unwrap());
        // First claim wins.
        assert!(pairings::claim_code(&pool, &hash, "matrix/@b:srv").await.unwrap());
        // Single-use: second claim of the same code fails.
        assert!(!pairings::claim_code(&pool, &hash, "matrix/@c:srv").await.unwrap());
        // Consumed → no longer active.
        assert!(!pairings::any_active_code(&pool).await.unwrap());

        // ---- expired code never claims ----
        let expired = "b".repeat(64);
        pairings::insert_code(&pool, &expired, None, -1).await.unwrap(); // already in the past
        assert!(!pairings::any_active_code(&pool).await.unwrap());
        assert!(!pairings::claim_code(&pool, &expired, "matrix/@d:srv").await.unwrap());

        // ---- list reflects active vs revoked ----
        let active = pairings::list_pairings(&pool, false).await.unwrap();
        assert_eq!(active.len(), 1, "one active pairing (matrix/@a:srv re-paired)");
        let all = pairings::list_pairings(&pool, true).await.unwrap();
        assert!(all.len() >= 2, "all includes the revoked row too");

        pool.close().await;
    });
}

/// `token_sha256` (migration 0022) round-trips through
/// `insert_pairing_with_token` / `token_hash_for`, and the three-state return
/// (no pairing / paired-no-token / paired-with-token) stays distinguishable —
/// including after revoke, which must make the token invisible rather than
/// merely inert.
///
/// Uses the admin pool for every call, same as the round-trip test above:
/// `revoke_pairing` needs UPDATE on `pairings`, which migration 0018 REVOKEs
/// from `kastellan_runtime` on purpose (revocation is an operator-only,
/// admin-connection action) — that privilege boundary is orthogonal to what
/// this test is pinning, so exercising it over the runtime pool would fail
/// for an unrelated reason.
#[test]
fn pairing_token_hash_round_trips_and_distinguishes_absent_from_null() {
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
        "pt-d",
        "pt-l",
        &format!("kastellan-supervisor-test-pg-pt-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        use kastellan_db::pairings;

        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "pairing-token-e2e"}),
        )
        .await
        .expect("probe run");

        let pool = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await
            .expect("admin pool");

        // No pairing at all → outer None.
        let missing = pairings::token_hash_for(&pool, "email", "nobody@example.org")
            .await
            .unwrap();
        assert_eq!(missing, None, "absent pairing must be distinguishable");

        // Paired WITHOUT a token (the Matrix shape) → Some(None).
        pairings::insert_pairing_with_token(&pool, "matrix", "@me:srv", "code", None)
            .await
            .unwrap();
        let matrix = pairings::token_hash_for(&pool, "matrix", "@me:srv")
            .await
            .unwrap();
        assert_eq!(matrix, Some(None), "a NULL token must mean 'no token required'");

        // Paired WITH a token → Some(Some(hash)).
        pairings::insert_pairing_with_token(
            &pool,
            "email",
            "me@example.org",
            "operator",
            Some("abc123"),
        )
        .await
        .unwrap();
        let email = pairings::token_hash_for(&pool, "email", "me@example.org")
            .await
            .unwrap();
        assert_eq!(email, Some(Some("abc123".to_string())));

        // Revoking must make the token invisible, not merely inert.
        pairings::revoke_pairing(&pool, "email", "me@example.org")
            .await
            .unwrap();
        let revoked = pairings::token_hash_for(&pool, "email", "me@example.org")
            .await
            .unwrap();
        assert_eq!(revoked, None, "a revoked pairing must not surface its token");

        pool.close().await;
    });
}
