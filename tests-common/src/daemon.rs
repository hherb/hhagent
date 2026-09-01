//! Shared real-daemon bring-up for the CLI e2e tests.
//!
//! Six integration tests drive a *real* `kastellan` daemon under the
//! supervisor against a per-test Postgres cluster, most of them then
//! exercising it through the `kastellan-cli` operator subprocess. They
//! previously each carried a byte-duplicated `MockLlm` + `bring_up_daemon`
//! pair that drifted apart over time; this module is the single source of
//! truth (issue #15 spirit).
//!
//! Three of the six were still hand-rolled copies until issue #634 —
//! `cli_ask_e2e`, `observation_capture` and `guard_boot_row_e2e`, ~70
//! identical lines each. The drift that argument predicts had already
//! happened: #635's stderr-on-failure fix landed in one private copy while
//! this shared helper, carrying three other e2es, kept the defect. What
//! made each of them a copy is now a [`DaemonSpec`] setter.
//!
//! What is *not* here: anything that depends on `kastellan-core` types
//! (skill factories, the per-OS python interpreter cascade). `tests-common`
//! is deliberately core-free — those stay private to the individual test file.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use kastellan_supervisor::{default_supervisor, ServiceStatus};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::guards::{PathGuard, ServiceGuard};
use crate::{core_binary, unique_temp_root, wait_for_log_match, wait_for_status};

mod spec;

pub use spec::{
    DaemonSpec, LlmEndpoint, DEFAULT_LLM_MODEL, DEFAULT_LLM_TIMEOUT_MS, DEFAULT_READY_TIMEOUT,
};

// ---------------------------------------------------------------------------
// Inert LLM mock — the `l3_run` paths NEVER call the LLM (the daemon executes
// the approved skill directly, no planner / CASSANDRA). It exists only so the
// daemon's router config points at a live socket and the daemon boots cleanly;
// every request gets a 503. If an l3_run path ever did dial the LLM, that 503
// would surface loudly as a task failure rather than hang.
// ---------------------------------------------------------------------------

/// A live-but-inert local-LLM endpoint. Holds the listener task; aborts it on
/// drop so no socket leaks between tests.
pub struct MockLlm {
    /// `http://127.0.0.1:<ephemeral-port>` — feed this to the daemon's
    /// `KASTELLAN_LLM_LOCAL_URL` (the caller appends `/v1`).
    pub base_url: String,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for MockLlm {
    fn drop(&mut self) {
        if let Some(h) = self.join.take() {
            h.abort();
        }
    }
}

/// Bind an ephemeral loopback port and serve `503 Service Unavailable` to every
/// connection. Returns once the listener is bound and accepting.
pub async fn spawn_inert_mock() -> MockLlm {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");

    let join = tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            // Drain whatever the client sent (best-effort) then 503.
            let mut tmp = [0u8; 1024];
            let _ = sock.read(&mut tmp).await;
            let body = "{}";
            let resp = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });

    MockLlm {
        base_url,
        join: Some(join),
    }
}

// ---------------------------------------------------------------------------
// Daemon bring-up.
// ---------------------------------------------------------------------------

/// The log file paths of a booted daemon — used by callers to dump the daemon's
/// stdout/stderr into assertion-failure messages.
pub struct DaemonHandle {
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
}

/// The RAII guards a booted daemon owns: the service (stopped + uninstalled on
/// drop), the core log dir, and the state dir.
pub type DaemonGuards = (ServiceGuard, PathGuard, PathGuard);

/// A daemon's stderr, formatted for a panic message.
///
/// Distinguishes the three states a reader needs kept apart — content,
/// genuinely empty, and unreadable. Collapsing the last two into `""`
/// (which `unwrap_or_default` does) turns "the log is gone" into "the
/// daemon said nothing", and the second reading sends you looking for a
/// code defect that is not there.
pub fn stderr_tail(stderr_path: &Path) -> String {
    match std::fs::read_to_string(stderr_path) {
        Ok(s) if s.trim().is_empty() => {
            format!("\n--- daemon stderr ({}) --- <empty>\n", stderr_path.display())
        }
        Ok(s) => format!("\n--- daemon stderr ({}) ---\n{s}\n", stderr_path.display()),
        Err(e) => format!(
            "\n--- daemon stderr ({}) --- <unreadable: {e}>\n",
            stderr_path.display()
        ),
    }
}

/// Install + start a real `kastellan` daemon under the supervisor and wait for
/// it to log `"scheduler spawned"`.
///
/// Everything configurable lives on [`DaemonSpec`]; see its docs for the
/// defaults and for why the parameters became a struct (issue #634).
///
/// Panics (rather than skips) on failure: callers are expected to have already
/// short-circuited on missing host prerequisites.
pub fn bring_up_daemon(daemon: &DaemonSpec) -> (DaemonHandle, DaemonGuards) {
    let core_log_dir = unique_temp_root(&daemon.log_dir_infix());
    std::fs::create_dir_all(&core_log_dir).expect("create core log dir");
    let core_log_guard = PathGuard {
        path: core_log_dir.clone(),
    };

    let state_dir = unique_temp_root(&daemon.state_dir_infix());
    let state_guard = PathGuard {
        path: state_dir.clone(),
    };

    let spec = daemon.service_spec(&core_binary(), &core_log_dir, &state_dir);
    // Read back rather than re-derived: `service_spec` owns the naming, and
    // a second `join(format!("{name}.out"))` here is exactly the kind of
    // duplicated derivation that drifts.
    let stdout_path = spec.stdout_log.clone().expect("service_spec sets stdout_log");
    let stderr_path = spec.stderr_log.clone().expect("service_spec sets stderr_log");

    let sup = default_supervisor();
    let service_guard = ServiceGuard {
        sup: default_supervisor(),
        name: spec.name.clone(),
    };
    sup.install(&spec).expect("install core");
    sup.start(&spec.name).expect("start core");

    // Both bring-up waits report the daemon's STDERR on failure, and
    // that is not decoration. Tracing goes to stdout
    // (`tracing_subscriber::fmt()`'s default writer), but a bring-up
    // abort propagates out of `main() -> Result<()>` and is printed by
    // `Termination` to **stderr** — so stdout holds the boot lines and
    // stderr holds the one line that names the failure. `wait_for_log_match`'s
    // own timeout text quotes stdout, which is the half that cannot say
    // why. Read errors are surfaced rather than defaulted to "": an
    // unreadable log and an empty one look identical otherwise, and
    // `/tmp` is scrubbed mid-run on both dev hosts.
    if let Err(e) = wait_for_status(
        sup.as_ref(),
        &spec.name,
        |s| s == ServiceStatus::Active,
        Duration::from_secs(10),
    ) {
        panic!("core active: {e}{}", stderr_tail(&stderr_path));
    }

    // The readiness budget is the caller's — see
    // [`DEFAULT_READY_TIMEOUT`]'s ⚠️ for why 10 s is not universal and
    // what a configured guard tier does to it.
    let ready = daemon.ready_timeout_value();
    if let Err(e) = wait_for_log_match(
        &stdout_path,
        |s| s.contains("scheduler spawned"),
        ready,
    ) {
        panic!(
            "daemon should log 'scheduler spawned' within {}s: {e}{}",
            ready.as_secs(),
            stderr_tail(&stderr_path)
        );
    }

    (
        DaemonHandle {
            stdout_path,
            stderr_path,
        },
        (service_guard, core_log_guard, state_guard),
    )
}

// ---------------------------------------------------------------------------
// Durable boot-row readback.
// ---------------------------------------------------------------------------

/// The stored `policy / guard_tier.boot` payload, as Postgres holds it.
///
/// Every booted daemon writes exactly one of these rows, so two e2es
/// carried a character-for-character copy of this query until #634
/// folded them together.
///
/// **Read back from the real table rather than from a sink double.**
/// `db::audit::insert` applies `truncate_payload` on the way in, so a
/// double asserts the payload that was *passed* rather than the one that
/// was *stored* — a distinction that has cost this tree a guard score
/// once already. It is live for the configured arm in a way it is not
/// for the two-key unconfigured one: the configured payload is the
/// larger of the two and the only one carrying a prose finding.
///
/// `fetch_all` plus an explicit length assertion rather than
/// `fetch_one`, because `fetch_one` returns the FIRST row and errors
/// only on zero — it does not reject duplicates, so uniqueness has to be
/// asserted here to be asserted at all. `ORDER BY id` keeps the failure
/// message deterministic rather than heap-ordered.
pub async fn guard_tier_boot_payload(pool: &sqlx::PgPool) -> serde_json::Value {
    let rows: Vec<(serde_json::Value,)> = sqlx::query_as(
        "SELECT payload FROM audit_log \
         WHERE actor = 'policy' AND action = 'guard_tier.boot' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("select guard_tier.boot rows");
    assert_eq!(
        rows.len(),
        1,
        "expected exactly one guard_tier.boot row (one per daemon start); got {}",
        rows.len()
    );
    rows.into_iter().next().expect("length asserted above").0
}

// ---------------------------------------------------------------------------
// CLI-output assertions.
// ---------------------------------------------------------------------------

/// Assert the operator CLI subprocess exited 0 and return its decoded
/// `(stdout, stderr)` for further content checks. On failure the panic message
/// dumps BOTH the CLI streams and the daemon's log files — the only way to
/// diagnose a daemon-side error from a CI log. `what` names the invocation.
pub fn assert_cli_success(output: &Output, daemon: &DaemonHandle, what: &str) -> (String, String) {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "{what} must exit 0; got {:?}\n\
         --- CLI stdout ---\n{}\n--- CLI stderr ---\n{}\n\
         --- daemon stdout ({}) ---\n{}\n--- daemon stderr ({}) ---\n{}\n",
        output.status,
        stdout,
        stderr,
        daemon.stdout_path.display(),
        std::fs::read_to_string(&daemon.stdout_path).unwrap_or_default(),
        daemon.stderr_path.display(),
        std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
    );
    (stdout, stderr)
}

/// Assert the operator CLI subprocess exited NON-zero (the fail-closed contract)
/// and return its decoded `(stdout, stderr)`. `what` names the invocation.
pub fn assert_cli_failure(output: &Output, what: &str) -> (String, String) {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !output.status.success(),
        "{what} must exit non-zero; got {:?}\nstdout={stdout}\nstderr={stderr}",
        output.status,
    );
    (stdout, stderr)
}
