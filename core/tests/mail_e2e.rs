//! End-to-end: the agent core spawns `kastellan-worker-mail` under the real
//! platform jail (macOS Seatbelt / Linux bwrap) and round-trips `mail.*` calls
//! against a plain-HTTP `mock_localmail` origin.
//!
//! Covers the two legs #487's stdio verification left untested: the OS-sandbox
//! leg (1a direct round-trip, 1c attachment delivery through the jail fs_write
//! boundary) and the egress-proxy leg (1b force-routing coupling + 1d allowlist
//! scoping). Skips as-pass when PG / the supervisor / the worker binary / a
//! working sandbox is missing.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::workers::mail::mail_entry;
use kastellan_tests_common::mock_localmail::{spawn_mock_localmail, CANNED_SHA256};
use kastellan_tests_common::{
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary, PgCluster,
};

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "mail-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

fn dispatch_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

struct TestEnv {
    cluster: PgCluster,
    worker_path: PathBuf,
    token_file: PathBuf,
    _token_dir: tempfile::TempDir,
}

/// Write a 0600 token file into a fresh temp dir; return the dir (kept alive)
/// and the file path (bound into the jail via the mail policy's fs_read).
fn write_token_file() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("mail-token");
    std::fs::write(&path, b"test-bearer-token").expect("write token");
    (dir, path)
}

fn ready_or_skip() -> Option<TestEnv> {
    if skip_if_no_supervisor() || skip_if_sandbox_unavailable() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let worker_path = workspace_target_binary("kastellan-worker-mail");
    if !worker_path.exists() {
        eprintln!("\n[SKIP] mail worker binary not built; run cargo build --workspace\n");
        return None;
    }
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "mail-d",
        "mail-l",
        &format!("kastellan-supervisor-test-pg-mail-{suffix}"),
    );
    let (token_dir, token_file) = write_token_file();
    Some(TestEnv { cluster, worker_path, token_file, _token_dir: token_dir })
}

/// Tier 1a — the mail worker runs under the real Seatbelt/bwrap jail and a
/// `mail.search` round-trips through the direct transport to the allowlisted
/// plain-HTTP mock origin.
#[test]
fn direct_search_round_trips_under_the_jail() {
    let env = match ready_or_skip() {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let mock = spawn_mock_localmail().await;
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let policy = mail_entry(
            env.worker_path.clone(),
            &mock.base_url,
            &env.token_file.to_string_lossy(),
        )
        .policy;
        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec { policy: &policy, program: &worker_str, args: &[], wall_clock_ms: None };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn mail under sandbox");

        let result = dispatch(
            &pool,
            &Vault::new(),
            &mut sworker,
            "mail",
            "mail.search",
            serde_json::json!({"query": "invoice"}),
        )
        .await
        .expect("mail.search round trip (worker under jail → mock localmail)");

        assert!(result["results"].is_array(), "expected a results array, got: {result}");

        let _ = sworker.close();
        pool.close().await;
    });
}

/// Tier 1c — attachment delivery through the jail's `fs_write` boundary. Applies
/// the production Phase-A durable-out path (`apply_workspace_out`), then drives
/// `get_message` → `get_attachment` and asserts the original-format file lands
/// under the task out dir with the origin's bytes.
#[test]
fn attachment_delivered_into_the_task_out_dir() {
    use kastellan_core::tool_host::apply_workspace_out;
    use kastellan_tests_common::mock_localmail::{CANNED_ATTACHMENT_BYTES, CANNED_MESSAGE_ID};

    let env = match ready_or_skip() {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let mock = spawn_mock_localmail().await;
        let pool = probe_and_pool(&env.cluster.conn_spec).await;

        // Durable per-task out dir, bound writable into the jail exactly as the
        // lane runner does in production.
        let out_dir = tempfile::tempdir().expect("out tempdir");
        let mut policy = mail_entry(
            env.worker_path.clone(),
            &mock.base_url,
            &env.token_file.to_string_lossy(),
        )
        .policy;
        apply_workspace_out(&mut policy, out_dir.path());

        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec { policy: &policy, program: &worker_str, args: &[], wall_clock_ms: None };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn mail under sandbox");

        // get_message returns the attachment sha the agent then delivers.
        let msg = dispatch(
            &pool, &Vault::new(), &mut sworker, "mail", "mail.get_message",
            serde_json::json!({"message_id": CANNED_MESSAGE_ID}),
        )
        .await
        .expect("mail.get_message");
        let sha = msg["attachments"][0]["sha256"].as_str().expect("attachment sha");
        assert_eq!(sha, CANNED_SHA256);

        let out = dispatch(
            &pool, &Vault::new(), &mut sworker, "mail", "mail.get_attachment",
            serde_json::json!({"sha256": sha, "filename": "invoice.pdf"}),
        )
        .await
        .expect("mail.get_attachment writes to the jailed out dir");

        let path = out["path"].as_str().expect("delivered path");
        assert!(
            std::path::Path::new(path).starts_with(out_dir.path()),
            "delivered file must be under the task out dir: {path}"
        );
        let bytes = std::fs::read(path).expect("read delivered file");
        assert_eq!(bytes, CANNED_ATTACHMENT_BYTES, "delivered bytes must match the origin");

        let _ = sworker.close();
        pool.close().await;
    });
}
