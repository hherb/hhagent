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

/// Tier 1b (egress leg) + 1d (allowlist scoping). Brings up a per-worker egress
/// sidecar from MAIL's real derived allowlist via the production
/// `spawn_forced_net_worker` coupling and asserts the sidecar enforces exactly
/// mail's endpoint host:port (allowed), blocks an off-allowlist host AND a wrong
/// loopback port (403 — the 1d scoping assertion), ingests both decisions, and
/// tears down 1:1.
///
/// NOTE: a full mail-JSON round-trip through this tunnel is NOT tested and is
/// not hermetically possible — the force-routed transport (`proxy_connect.rs`)
/// is HTTPS-only and the proxy's MITM upstream (`egress-proxy/pins.rs::
/// build_upstream_client_config`) trusts webpki roots only (pins only
/// strengthen; no origin-CA knob). A plain-HTTP or self-signed loopback origin
/// is therefore unreachable — the #473 wall. The full round-trip is deferred to
/// a real publicly-trusted-cert localmail (see the spec's "Out of scope").
#[test]
fn mail_policy_force_routes_and_enforces_its_endpoint_allowlist() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use kastellan_core::egress::net_worker::{spawn_forced_net_worker, NetWorkerSpawn};
    use kastellan_sandbox::Net;
    use kastellan_tests_common::egress_forcing::{
        assert_connect_established, minted_uds, short_scratch_root,
    };
    use kastellan_tests_common::egress_proxy_bin_or_skip;

    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(proxy) = egress_proxy_bin_or_skip() else {
        return;
    };

    dispatch_runtime().block_on(async {
        let mock = spawn_mock_localmail().await;
        let (_token_dir, token_file) = write_token_file();
        let worker_path = workspace_target_binary("kastellan-worker-mail");

        // Derive the allowlist from mail's REAL manifest policy (proving the
        // manifest wiring produces a force-routable Net::Allowlist).
        let mail_policy =
            mail_entry(worker_path, &mock.base_url, &token_file.to_string_lossy()).policy;
        let allowlist: Vec<String> = match &mail_policy.net {
            Net::Allowlist(v) => v.clone(),
            other => panic!("mail must be Net::Allowlist, got {other:?}"),
        };
        // mock.base_url is http://127.0.0.1:<port>; the derived entry is that host:port.
        let endpoint_hostport = mock.base_url.strip_prefix("http://").unwrap().to_string();
        assert_eq!(
            allowlist,
            vec![endpoint_hostport.clone()],
            "1d: allowlist is exactly the endpoint"
        );

        let scratch_root = short_scratch_root(&format!("mail-{}", unique_suffix()));
        let actions = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = {
            let actions = Arc::clone(&actions);
            move |row: kastellan_core::egress::audit::EgressAuditRow| {
                actions.lock().unwrap().push(row.action);
            }
        };

        // The worker doesn't drive the proxy here (the host does); a long-lived
        // program keeps the worker + sidecar up. `/bin/sleep` resolves on both
        // macOS and Linux.
        let policy = mail_policy;
        let spec = WorkerSpec {
            policy: &policy,
            program: "/bin/sleep",
            args: &["30"],
            wall_clock_ms: None,
        };
        let backend = backend();
        let params = NetWorkerSpawn {
            backend: backend.as_ref(),
            sidecar_backend: backend.as_ref(),
            proxy_bin: &proxy,
            spec: &spec,
            allowlist: &allowlist,
            worker_name: "mail",
            secret_fingerprints: &[],
            cert_pins_json: None,
            disable_mitm: false,
        };
        let mut worker = spawn_forced_net_worker(&params, &scratch_root, sink)
            .expect("force-routed mail worker + sidecar spawn");
        let uds = minted_uds(&scratch_root);

        // Allowed: CONNECT to mail's endpoint host:port establishes a tunnel.
        let mut ok = UnixStream::connect(&uds).expect("connect coupling UDS");
        write!(ok, "CONNECT {endpoint_hostport} HTTP/1.1\r\n\r\n").unwrap();
        assert_connect_established(&mut ok);
        drop(ok);

        // 1d: an off-allowlist host is blocked (403).
        let mut bad_host = UnixStream::connect(&uds).unwrap();
        write!(bad_host, "CONNECT evil.test:443 HTTP/1.1\r\n\r\n").unwrap();
        let mut r1 = String::new();
        let _ = bad_host.read_to_string(&mut r1);
        assert!(r1.starts_with("HTTP/1.1 403"), "off-host must 403, got {r1:?}");
        drop(bad_host);

        // 1d: a wrong loopback PORT is blocked (proves port-scoping, not host-only).
        let wrong_port = format!("127.0.0.1:{}", pick_other_port(&endpoint_hostport));
        let mut bad_port = UnixStream::connect(&uds).unwrap();
        write!(bad_port, "CONNECT {wrong_port} HTTP/1.1\r\n\r\n").unwrap();
        let mut r2 = String::new();
        let _ = bad_port.read_to_string(&mut r2);
        assert!(r2.starts_with("HTTP/1.1 403"), "wrong port must 403, got {r2:?}");
        drop(bad_port);

        // Both decisions reached the ingest sink.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            {
                let seen = actions.lock().unwrap();
                let allowed = seen.iter().any(|a| a == "egress.allowed");
                let blocked = seen.iter().any(|a| a == "egress.blocked.allowlist");
                if allowed && blocked {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "ingest sink missed a decision: {:?}",
                *actions.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        // 1:1 teardown.
        worker.kill().ok();
        drop(worker);
        let down = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&uds).is_ok() {
            assert!(
                Instant::now() < down,
                "sidecar kept serving after worker drop (teardown not 1:1)"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_dir_all(&scratch_root);
    });
}

/// A loopback port guaranteed different from `hostport`'s port (for the
/// port-scoping 1d assertion). Returns `endpoint_port ^ 1` (still a valid,
/// almost-certainly-unbound port).
fn pick_other_port(hostport: &str) -> u16 {
    let p: u16 = hostport.rsplit(':').next().unwrap().parse().unwrap();
    p ^ 1
}
