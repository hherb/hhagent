//! PG-gated e2e for the channel bus: pins the real DB seams
//! (`PgChannelEvents` enqueue + audit, `PgCompletedTasks` over the
//! `tasks_completed` NOTIFY) against a live cluster. Skip-as-pass when no
//! `KASTELLAN_PG_BIN_DIR` is configured (mirrors `injection_guard_e2e`).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::HashMap;

use tokio::sync::mpsc;

use kastellan_core::channel::auth::{
    AuthDecision, DbPeerAuthorizer, PeerAuthorizer, StaticPairings, UnauthenticReason,
};
use kastellan_core::channel::bus::{
    handle_completed, handle_inbound, CompletedTasks, PgChannelEvents, PgCompletedTasks,
};
use kastellan_core::channel::ingest::sha256_hex;
use kastellan_core::channel::{
    actions, ChannelId, ConversationId, IncomingMessage, OutgoingMessage, PeerEvidence, PeerId,
};
use kastellan_db::tasks::{self, Lane};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "channel-bus-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channel_inbound_enqueues_and_completion_routes_a_reply() {
    // Skip-as-pass without a `systemd --user`/launchd supervisor (e.g. a root
    // CI container) — `bring_up_pg_cluster` needs one to run the PG service, and
    // `initdb` itself refuses to run as root. Mirrors `postgres_e2e` /
    // `injection_guard_e2e`. Live path runs on the DGX (real PG) + Mac.
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return; // skip-as-pass
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ch-d",
        "ch-l",
        &format!("kastellan-supervisor-test-pg-ch-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    // ── Inbound: a paired, clean message must enqueue a `channel` task + audit. ──
    let events = PgChannelEvents::new(pool.clone());
    let authorizer = StaticPairings::from_peers([PeerId("@me:srv".into())]);
    let msg = IncomingMessage {
        channel: ChannelId("matrix".into()),
        peer: PeerId("@me:srv".into()),
        conversation: ConversationId("!room:srv".into()),
        body: "what's on my calendar?".into(),
        evidence: None,
    };
    handle_inbound(&authorizer, None, &events, &msg).await;

    let pending = tasks::list(&pool, Some(Lane::Fast), Some("pending"), 10)
        .await
        .expect("list pending");
    assert_eq!(pending.len(), 1, "exactly one channel task enqueued");
    let task = &pending[0];
    assert_eq!(task.payload["kind"], "channel");
    assert_eq!(task.payload["instruction"], "what's on my calendar?");
    assert_eq!(task.payload["channel"], "matrix");
    assert_eq!(task.payload["peer"], "@me:srv");
    assert_eq!(task.payload["conversation"], "!room:srv");

    let audits = kastellan_db::audit::fetch_since(&pool, 0, 200).await.expect("audit fetch");
    assert!(
        audits.iter().any(|r| r.actor == "channel" && r.action == actions::RECEIVED),
        "expected a channel.received audit row"
    );

    // ── Outbound: listen, finalize the task, route the completion to a reply. ──
    // LISTEN before finalize so the NOTIFY is not missed.
    let mut completed = PgCompletedTasks::connect(pool.clone())
        .await
        .expect("connect completed-tasks listener");

    // Claim (pending → running) then finalize (running → completed, fires NOTIFY).
    let claimed = tasks::claim_one(&pool, Lane::Fast, 60)
        .await
        .expect("claim")
        .expect("a pending task to claim");
    assert_eq!(claimed.id, task.id);
    tasks::finalize(
        &pool,
        claimed.id,
        "completed",
        Some(serde_json::json!({"kind": "completed", "message": "You have 2 meetings."})),
    )
    .await
    .expect("finalize");

    let id = completed.next_completed().await.expect("a completed-task id");
    assert_eq!(id, task.id);

    let (tx, mut rx) = mpsc::channel::<OutgoingMessage>(4);
    let mut senders = HashMap::new();
    senders.insert(ChannelId("matrix".into()), tx);
    let out = handle_completed(&completed, &events, &senders, id)
        .await
        .expect("routed reply");
    assert_eq!(out.body, "You have 2 meetings.");
    assert_eq!(out.conversation, ConversationId("!room:srv".into()));
    let delivered = rx.recv().await.expect("reply delivered to channel sender");
    assert_eq!(delivered.peer, PeerId("@me:srv".into()));

    let audits = kastellan_db::audit::fetch_since(&pool, 0, 200).await.expect("audit fetch 2");
    assert!(
        audits.iter().any(|r| r.actor == "channel" && r.action == actions::REPLIED),
        "expected a channel.replied audit row"
    );

    // Drop the listener before pool.close() — `PgCompletedTasks` holds a
    // checked-out PoolConnection (sqlx 0.9 `PgListener` only releases it from
    // inside `recv()`), and `pool.close()` blocks until every connection is
    // returned, so a listener still in scope at close-time deadlocks the test.
    drop(completed);
    pool.close().await;
}

/// Exercises `DbPeerAuthorizer::authorize` — the REAL production authorizer,
/// not the `TokenAuthorizer` fake in `bus.rs`'s unit tests — against a live
/// `pairings` table, covering every decision arm of `token_hash_for`'s
/// `Ok(None)` / `Ok(Some(None))` / `Ok(Some(Some(_)))` split, INCLUDING each
/// arm's specific `UnauthenticReason` (the audit label an operator diagnoses
/// with) and the `Ok(Some(None))` + evidence guard added by the final review.
///
/// Seeding uses the ADMIN pool (`connect_admin_pool`), same rationale as
/// `db/tests/pairings_e2e.rs`'s token round-trip test: not because
/// `insert_pairing_with_token` itself needs it (migration 0018 grants
/// `kastellan_runtime` INSERT on `pairings`), but to keep every fixture-setup
/// call site in this suite using one consistent, deliberately-superuser
/// connection, matching the operator-only mental model pairing rows are
/// supposed to have. The authorizer itself is built over a **runtime** pool,
/// exactly like production (`main::matrix_boot` passes the daemon-scoped
/// runtime pool into `DbPeerAuthorizer::new`) — `token_hash_for` is a SELECT,
/// which the runtime role does have.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_peer_authorizer_covers_all_evidence_arms_against_a_real_pairing_table() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return; // skip-as-pass
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "dpa-d",
        "dpa-l",
        &format!("kastellan-supervisor-test-pg-dpa-{suffix}"),
    );
    kastellan_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "db-peer-authorizer-e2e"}),
    )
    .await
    .expect("probe run");

    let admin = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
        .await
        .expect("admin pool");
    let runtime = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");
    let authorizer = DbPeerAuthorizer::new(runtime.clone());

    // ---- 1. No active pairing at all → Rejected. ----
    let no_row = ChannelId("email".into());
    let nobody = PeerId("nobody@example.org".into());
    assert_eq!(
        authorizer.authorize(&no_row, &nobody, None).await,
        AuthDecision::Rejected,
        "an address with no pairings row at all must be Rejected"
    );

    // ---- 2. Paired, token_sha256 IS NULL (the Matrix shape) + NO evidence
    //         → Recognised. THIS assertion is the Matrix-parity pin: Matrix
    //         hard-codes `evidence: None` (`matrix/wire.rs`) and its pairing
    //         rows carry a NULL token, so this is the exact production shape.
    //         It must stay green for Matrix to be byte-identical. ----
    let matrix_ch = ChannelId("matrix".into());
    let matrix_peer = PeerId("matrix-shape@example.org".into());
    kastellan_db::pairings::insert_pairing_with_token(
        &admin,
        &matrix_ch.0,
        &matrix_peer.0,
        "code",
        None,
    )
    .await
    .expect("seed NULL-token pairing");
    assert_eq!(
        authorizer.authorize(&matrix_ch, &matrix_peer, None).await,
        AuthDecision::Recognised,
        "a NULL token_sha256 row must admit with no evidence at all (Matrix)"
    );

    // ---- 2b. Same NULL-token row, but the transport DID supply evidence →
    //          RejectedUnauthentic(PairingHasNoToken).
    //
    //          This assertion was INVERTED by the final whole-branch review
    //          (Important 1). It previously pinned `Recognised` — i.e. it
    //          pinned as *correct* the behaviour that a `channel='email'`
    //          pairing row with a NULL `token_sha256` would admit a sender
    //          whose DMARC FAILED and whose token was wrong, collapsing the
    //          entire email gate for that address. Nothing creates such a row
    //          today, but no DB CHECK, code guard, or test prevented one.
    //
    //          `evidence.is_some()` is the branch's general "this transport
    //          cannot vouch for its sender" marker (it already gates the
    //          pairing carve-out in `bus::handle_inbound`); `auth.rs` now
    //          applies the same marker here. Matrix is unaffected because it
    //          never produces evidence — which is exactly what assertion 2
    //          above pins. ----
    let hostile_evidence =
        PeerEvidence { dmarc_pass: false, presented_token: Some("wrong".into()) };
    assert_eq!(
        authorizer.authorize(&matrix_ch, &matrix_peer, Some(&hostile_evidence)).await,
        AuthDecision::RejectedUnauthentic(UnauthenticReason::PairingHasNoToken),
        "a token-less pairing row must REFUSE an evidence-bearing transport: such a \
         peer is admitted only on the strength of its token, so a row without one is \
         misconfigured for it, not permissive"
    );
    // Not just the hostile shape: even evidence that looks perfect must be
    // refused, so the guard cannot be weakened to "bad evidence only".
    let good_looking_evidence =
        PeerEvidence { dmarc_pass: true, presented_token: Some("anything".into()) };
    assert_eq!(
        authorizer.authorize(&matrix_ch, &matrix_peer, Some(&good_looking_evidence)).await,
        AuthDecision::RejectedUnauthentic(UnauthenticReason::PairingHasNoToken),
        "the refusal is about the MISSING pairing token, not about the evidence quality"
    );

    // ---- 3-6. Paired WITH a token (the email shape): four evidence arms. ----
    let email_ch = ChannelId("email".into());
    let email_peer = PeerId("token-required@example.org".into());
    let good_token = "e2e-good-token";
    let good_hash = sha256_hex(good_token.as_bytes());
    kastellan_db::pairings::insert_pairing_with_token(
        &admin,
        &email_ch.0,
        &email_peer.0,
        "operator",
        Some(&good_hash),
    )
    .await
    .expect("seed token-required pairing");

    // 3. Correct token + dmarc_pass → Recognised.
    let good = PeerEvidence { dmarc_pass: true, presented_token: Some(good_token.into()) };
    assert_eq!(
        authorizer.authorize(&email_ch, &email_peer, Some(&good)).await,
        AuthDecision::Recognised,
        "correct token + DMARC pass must admit"
    );

    // 4. Correct token but dmarc_pass == false → RejectedUnauthentic(DmarcFail).
    let bad_dmarc = PeerEvidence { dmarc_pass: false, presented_token: Some(good_token.into()) };
    assert_eq!(
        authorizer.authorize(&email_ch, &email_peer, Some(&bad_dmarc)).await,
        AuthDecision::RejectedUnauthentic(UnauthenticReason::DmarcFail),
        "a correct token with a failed DMARC verdict must not admit"
    );

    // 5. Wrong token (DMARC otherwise fine) → RejectedUnauthentic(TokenMismatch).
    let wrong_token =
        PeerEvidence { dmarc_pass: true, presented_token: Some("not-the-token".into()) };
    assert_eq!(
        authorizer.authorize(&email_ch, &email_peer, Some(&wrong_token)).await,
        AuthDecision::RejectedUnauthentic(UnauthenticReason::TokenMismatch),
        "a wrong token must not admit even with a good DMARC verdict"
    );

    // 5b. DMARC fine but NO token presented at all → RejectedUnauthentic(NoToken).
    //     Distinct from 5: an HTML-only mail (no `body_text`) lands here, and
    //     the operator needs to tell it apart from a wrong token.
    let no_token = PeerEvidence { dmarc_pass: true, presented_token: None };
    assert_eq!(
        authorizer.authorize(&email_ch, &email_peer, Some(&no_token)).await,
        AuthDecision::RejectedUnauthentic(UnauthenticReason::NoToken),
        "DMARC pass with no token presented must not admit"
    );

    // 6. Token required but the transport supplied no evidence → RejectedUnauthentic(NoEvidence).
    assert_eq!(
        authorizer.authorize(&email_ch, &email_peer, None).await,
        AuthDecision::RejectedUnauthentic(UnauthenticReason::NoEvidence),
        "a token-required pairing with no evidence at all must not admit"
    );

    admin.close().await;
    runtime.close().await;
}
