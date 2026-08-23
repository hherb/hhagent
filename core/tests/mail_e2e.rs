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
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("mail-token");
    std::fs::write(&path, b"test-bearer-token").expect("write token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("chmod token 0600");
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
            None, // guard tier: not exercised by this suite
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
            &pool, &Vault::new(), None, &mut sworker, "mail", "mail.get_message",
            serde_json::json!({"message_id": CANNED_MESSAGE_ID}),
        )
        .await
        .expect("mail.get_message");
        let sha = msg["attachments"][0]["sha256"].as_str().expect("attachment sha");
        assert_eq!(sha, CANNED_SHA256);

        let out = dispatch(
            &pool, &Vault::new(), None, &mut sworker, "mail", "mail.get_attachment",
            serde_json::json!({"sha256": sha, "filename": "invoice.pdf"}),
        )
        .await
        .expect("mail.get_attachment writes to the jailed out dir");

        let path = out["path"].as_str().expect("delivered path");
        // Canonicalize both sides before the prefix check: on macOS the tempdir
        // lives under `/var/folders/...` which is a symlink to `/private/var/...`,
        // so a raw `starts_with` can spuriously fail if the delivered path was
        // resolved through the symlink.
        let delivered = std::fs::canonicalize(path).expect("canonicalize delivered path");
        let out_root = std::fs::canonicalize(out_dir.path()).expect("canonicalize out dir");
        assert!(
            delivered.starts_with(&out_root),
            "delivered file must be under the task out dir: {path}"
        );
        let bytes = std::fs::read(&delivered).expect("read delivered file");
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
    use kastellan_core::egress::spawn::Mitm;
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
            mitm: Mitm::Intercept { upstream_extra_ca: None },
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

/// Drive a force-routed `mail.search` through a real MITM egress sidecar to a
/// self-signed HTTPS localmail mock. `with_extra_ca` toggles whether the proxy is
/// given the mock's cert as its upstream extra CA. Returns the dispatch result
/// (error mapped to String) and the captured egress decisions. Shared by the
/// positive round-trip and the negative control so they differ only in the one
/// variable under test.
async fn run_forced_mail_search_over_tls(
    proxy: &std::path::Path,
    bin_dir: &std::path::Path,
    with_extra_ca: bool,
) -> (
    Result<serde_json::Value, String>,
    Vec<kastellan_core::egress::audit::EgressAuditRow>,
) {
    use std::sync::{Arc, Mutex};

    use kastellan_core::egress::net_worker::{spawn_forced_net_worker, NetWorkerSpawn};
    use kastellan_core::egress::spawn::Mitm;
    use kastellan_sandbox::Net;
    use kastellan_tests_common::egress_forcing::short_scratch_root;
    use kastellan_tests_common::mock_localmail::spawn_mock_localmail_tls;

    let (mock, cert_pem) = spawn_mock_localmail_tls().await;
    // Write the mock's cert where the sandboxed proxy can fs_read it.
    let ca_dir = tempfile::tempdir().expect("ca tempdir");
    let ca_path = ca_dir.path().join("localmail-ca.pem");
    std::fs::write(&ca_path, &cert_pem).expect("write ca pem");

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        bin_dir,
        "mailrt-d",
        "mailrt-l",
        &format!("kastellan-supervisor-test-pg-mailrt-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    let (_token_dir, token_file) = write_token_file();
    let worker_path = workspace_target_binary("kastellan-worker-mail");
    let mail_policy =
        mail_entry(worker_path.clone(), &mock.base_url, &token_file.to_string_lossy()).policy;
    let allowlist: Vec<String> = match &mail_policy.net {
        Net::Allowlist(v) => v.clone(),
        other => panic!("mail must be Net::Allowlist, got {other:?}"),
    };

    let scratch_root = short_scratch_root(&format!("mailrt-{}", unique_suffix()));
    let rows = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let rows = Arc::clone(&rows);
        move |row: kastellan_core::egress::audit::EgressAuditRow| rows.lock().unwrap().push(row)
    };

    let worker_str = worker_path.to_string_lossy().into_owned();
    let spec = WorkerSpec { policy: &mail_policy, program: &worker_str, args: &[], wall_clock_ms: None };
    let backend = backend();
    let params = NetWorkerSpawn {
        backend: backend.as_ref(),
        sidecar_backend: backend.as_ref(),
        proxy_bin: proxy,
        spec: &spec,
        allowlist: &allowlist,
        worker_name: "mail",
        secret_fingerprints: &[],
        cert_pins_json: None,
        // MITM ON — mail's real posture.
        mitm: Mitm::Intercept { upstream_extra_ca: with_extra_ca.then_some(ca_path.as_path()) },
    };
    let mut worker = spawn_forced_net_worker(&params, &scratch_root, sink)
        .expect("force-routed mail worker + sidecar spawn");

    let result = dispatch(
        &pool,
        &Vault::new(),
        None, // guard tier: not exercised by this suite
        &mut worker,
        "mail",
        "mail.search",
        serde_json::json!({"query": "invoice"}),
    )
    .await
    .map_err(|e| e.to_string());

    let _ = worker.close();
    // The decision-ingest thread is deliberately DETACHED (see `EgressSidecar`),
    // so `close()` only *starts* its drain: the proxy dies, the thread sees EOF on
    // its stdout, flushes the decision lines still buffered, and exits. Reading
    // `rows` straight after the close would race that drain — which matters most
    // for the LAST decision of a connection (the negative control's
    // `mitm_failed: …`, emitted only after the upstream handshake fails). Poll to
    // quiescence instead: the count must hold steady across two consecutive polls.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let (mut last_len, mut stable) = (usize::MAX, 0u8);
    while std::time::Instant::now() < deadline && stable < 2 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let len = rows.lock().unwrap().len();
        if len == last_len && len > 0 {
            stable += 1;
        } else {
            (last_len, stable) = (len, 0);
        }
    }
    pool.close().await;
    let _ = std::fs::remove_dir_all(&scratch_root);
    let captured = std::mem::take(&mut *rows.lock().unwrap());
    (result, captured)
}

/// Hermetic full round-trip: the REAL mail worker, force-routed in MITM mode,
/// drives mail.search through the sidecar to a self-signed HTTPS localmail mock;
/// the proxy MITM-terminates and re-originates TLS validated against the
/// operator-provided upstream extra CA. The #491 deliverable tier 1b could not cover.
///
/// The load-bearing assertion is `results` round-tripping: bytes only reach the
/// worker if the proxy's upstream handshake against the self-signed origin
/// validated. `tls_intercepted: true` is asserted as well, but note it is a
/// *weaker* signal than it looks — the proxy emits that decision when it takes
/// the MITM branch, BEFORE `run_mitm` performs the upstream handshake (see
/// `egress-proxy::proxy`), so on its own it proves only "not transparently
/// tunnelled", not "re-origination succeeded".
#[test]
fn force_routed_search_round_trips_through_mitm_sidecar() {
    use kastellan_tests_common::egress_proxy_bin_or_skip;

    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(proxy) = egress_proxy_bin_or_skip() else {
        return;
    };
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    if !workspace_target_binary("kastellan-worker-mail").exists() {
        eprintln!("\n[SKIP] mail worker binary not built; run cargo build --workspace\n");
        return;
    }

    dispatch_runtime().block_on(async {
        let (result, rows) = run_forced_mail_search_over_tls(&proxy, &bin_dir, true).await;
        let value = result.expect("mail.search must round-trip through the MITM sidecar");
        assert!(value["results"].is_array(), "expected results array, got {value}");
        assert!(
            rows.iter().any(|r| r.action == "egress.allowed"
                && r.payload["tls_intercepted"] == serde_json::Value::Bool(true)),
            "expected an MITM-intercepted allow decision (tls_intercepted: true); got {:?}",
            rows.iter().map(|r| (r.action.clone(), r.payload.clone())).collect::<Vec<_>>()
        );
    });
}

/// Negative control: the identical round-trip with NO upstream extra CA must
/// FAIL — the proxy re-originates against webpki roots only and rejects the
/// self-signed origin. Proves the extra-CA seam is load-bearing (the round-trip
/// does not "accidentally" work without it).
///
/// `is_err()` alone would be satisfied by ANY failure (worker crash, PG hiccup,
/// dispatch timeout), so the control would silently stop being a control the day
/// something upstream of TLS broke. So we also pin the failure to the
/// re-origination leg, and pin it *precisely*: the assertion matches
/// `mitm_failed: origin TLS handshake` rather than the bare `mitm_failed:`
/// prefix. `classify_mitm_error` (`workers/egress-proxy/src/proxy.rs`) stamps
/// that prefix onto every non-pin intercept failure, and `mitm.rs` reaches it
/// from five sites — including `worker TLS handshake: …`, which fires BEFORE the
/// origin is dialled, and `dial origin …: connection refused`. Matching the bare
/// prefix would therefore also accept broken per-instance CA provisioning or an
/// origin that never bound, neither of which is what this control tests.
#[test]
fn force_routed_search_fails_without_upstream_extra_ca() {
    use kastellan_tests_common::egress_proxy_bin_or_skip;

    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(proxy) = egress_proxy_bin_or_skip() else {
        return;
    };
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    if !workspace_target_binary("kastellan-worker-mail").exists() {
        eprintln!("\n[SKIP] mail worker binary not built; run cargo build --workspace\n");
        return;
    }

    dispatch_runtime().block_on(async {
        let (result, rows) = run_forced_mail_search_over_tls(&proxy, &bin_dir, false).await;
        assert!(
            result.is_err(),
            "without the upstream extra CA the MITM re-origination must reject the \
             self-signed origin; got Ok: {result:?}"
        );
        // Pin the failure to the re-origination leg, not to any incidental error.
        assert!(
            rows.iter().any(|r| r.payload["reason"]
                .as_str()
                .is_some_and(|reason| reason.starts_with("mitm_failed: origin TLS handshake"))),
            "the failure must be the proxy's UPSTREAM handshake rejecting the \
             self-signed origin (a `mitm_failed: origin TLS handshake: …` decision) — \
             a bare `mitm_failed:` would also match a worker-side handshake or a \
             refused dial, which this control is not about; got {:?}",
            rows.iter().map(|r| (r.action.clone(), r.payload.clone())).collect::<Vec<_>>()
        );
    });
}

/// Live #[ignore] DGX tier: the same MITM round-trip against the REAL localmail
/// running on the DGX (self-signed cert), validating the extra-CA seam against a
/// real cert + the real archive. Env-gated — skip-as-pass unless the operator
/// sets all three live vars. Run on the DGX:
///
///   KASTELLAN_MAIL_LIVE_ENDPOINT=https://127.0.0.1:8443 \
///   KASTELLAN_MAIL_LIVE_CA=$HOME/.config/localmail/tls/cert.pem \
///   KASTELLAN_MAIL_LIVE_TOKEN=<bearer from POST /v1/auth/login> \
///   cargo test -p kastellan-core --test mail_e2e -- --ignored --nocapture \
///     force_routed_search_against_real_localmail
///
/// The endpoint host MUST match an IP/DNS SAN in the cert. On the DGX, localmail
/// binds `10.0.0.3:8443` only (not loopback), so use `https://10.0.0.3:8443` — the
/// proxy's allowlisted-IP carve-out dials that private literal (live-verified
/// 2026-07-26, no SSRF block). The origin MUST serve a **non-CA leaf** cert:
/// rustls-webpki (the proxy's re-origination validator) rejects a self-signed cert
/// marked `basicConstraints CA:TRUE` with `CaUsedAsEndEntity`, even though openssl
/// accepts it — a self-signed cert with `CA:FALSE` (like the hermetic mock)
/// validates. Token is pre-obtained to keep the password out of the test process.
#[test]
#[ignore = "live DGX localmail; set KASTELLAN_MAIL_LIVE_ENDPOINT/CA/TOKEN"]
fn force_routed_search_against_real_localmail() {
    use std::sync::{Arc, Mutex};

    use kastellan_core::egress::net_worker::{spawn_forced_net_worker, NetWorkerSpawn};
    use kastellan_core::egress::spawn::Mitm;
    use kastellan_sandbox::Net;
    use kastellan_tests_common::egress_forcing::short_scratch_root;
    use kastellan_tests_common::egress_proxy_bin_or_skip;

    let (Some(endpoint), Some(ca), Some(token)) = (
        std::env::var("KASTELLAN_MAIL_LIVE_ENDPOINT").ok(),
        std::env::var("KASTELLAN_MAIL_LIVE_CA").ok(),
        std::env::var("KASTELLAN_MAIL_LIVE_TOKEN").ok(),
    ) else {
        eprintln!("\n[SKIP] live localmail vars unset (KASTELLAN_MAIL_LIVE_ENDPOINT/CA/TOKEN)\n");
        return;
    };
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(proxy) = egress_proxy_bin_or_skip() else {
        return;
    };
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let worker_path = workspace_target_binary("kastellan-worker-mail");
    if !worker_path.exists() {
        eprintln!("\n[SKIP] mail worker binary not built\n");
        return;
    }

    dispatch_runtime().block_on(async {
        use std::os::unix::fs::PermissionsExt;
        let token_dir = tempfile::tempdir().expect("token tempdir");
        let token_file = token_dir.path().join("mail-token");
        std::fs::write(&token_file, token.trim()).expect("write token");
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();

        let ca_path = std::path::PathBuf::from(&ca);
        assert!(ca_path.exists(), "KASTELLAN_MAIL_LIVE_CA does not exist: {ca}");

        let suffix = unique_suffix();
        let cluster = bring_up_pg_cluster(
            &bin_dir,
            "maillive-d",
            "maillive-l",
            &format!("kastellan-supervisor-test-pg-maillive-{suffix}"),
        );
        let pool = probe_and_pool(&cluster.conn_spec).await;

        let mail_policy =
            mail_entry(worker_path.clone(), &endpoint, &token_file.to_string_lossy()).policy;
        let allowlist: Vec<String> = match &mail_policy.net {
            Net::Allowlist(v) => v.clone(),
            other => panic!("mail must be Net::Allowlist, got {other:?}"),
        };

        let scratch_root = short_scratch_root(&format!("maillive-{}", unique_suffix()));
        let rows = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let rows = Arc::clone(&rows);
            move |row: kastellan_core::egress::audit::EgressAuditRow| rows.lock().unwrap().push(row)
        };

        let worker_str = worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec { policy: &mail_policy, program: &worker_str, args: &[], wall_clock_ms: None };
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
            mitm: Mitm::Intercept { upstream_extra_ca: Some(ca_path.as_path()) },
        };
        let mut worker = spawn_forced_net_worker(&params, &scratch_root, sink)
            .expect("force-routed mail worker + sidecar spawn");

        let value = dispatch(
            &pool,
            &Vault::new(),
            None, // guard tier: not exercised by this suite
            &mut worker,
            "mail",
            "mail.search",
            serde_json::json!({"query": "invoice"}),
        )
        .await
        .expect("live mail.search must round-trip through the MITM sidecar");
        assert!(value["results"].is_array(), "expected results array, got {value}");
        assert!(
            rows.lock().unwrap().iter().any(|r| r.action == "egress.allowed"
                && r.payload["tls_intercepted"] == serde_json::Value::Bool(true)),
            "expected an MITM-intercepted allow decision against live localmail"
        );

        let _ = worker.close();
        pool.close().await;
        let _ = std::fs::remove_dir_all(&scratch_root);
    });
}
