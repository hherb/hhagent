//! End-to-end test: the agent core runs python-exec inside the macOS
//! `MacosContainer` micro-VM (Phase 4 container mode) and round-trips
//! `python.exec` through `tool_host::dispatch_with_sink`.
//!
//! Pins what host mode can't on macOS: the `mem_mb: 512` cap is actually
//! ENFORCED by the VM (a >512 MiB allocation is SIGKILLed), and `Net::Deny`
//! + `--network none` contains a socket attempt inside the guest kernel.
//!
//! Uses `dispatch_with_sink` with a no-op audit sink so the test needs no
//! Postgres cluster — the container itself is the only external dependency.
//!
//! `[SKIP]`s cleanly when the `container` CLI / its system service / the
//! `kastellan/python-exec:dev` image are missing. Build the image first:
//!     scripts/workers/python-exec/build-image.sh

#![cfg(target_os = "macos")]

use std::sync::Arc;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, spawn_worker, ToolHostError, WorkerSpec};
use kastellan_core::workers::python_exec::{container_mode_entry, DEFAULT_IMAGE};
use kastellan_protocol::{client::ClientError, codes};
use kastellan_sandbox::{macos_container::MacosContainer, SandboxBackendKind, SandboxBackends};
use kastellan_tests_common::NoopAuditSink;

/// Skip the test (via early-return) when Apple `container` isn't usable
/// on this host or the python-exec image is absent. Returns `true` when
/// the caller should skip.
fn skip_if_no_container_image() -> bool {
    if let Err(e) = MacosContainer::probe() {
        eprintln!("\n[SKIP] container probe failed: {e}\n");
        return true;
    }
    let listed = std::process::Command::new("container")
        .args(["image", "list"])
        .output();
    let has_image = matches!(
        listed,
        Ok(o) if String::from_utf8_lossy(&o.stdout).contains("python-exec")
    );
    if !has_image {
        eprintln!(
            "\n[SKIP] {DEFAULT_IMAGE} image not present; run \
             scripts/workers/python-exec/build-image.sh\n"
        );
        return true;
    }
    false
}

/// Resolve the container backend for the python-exec image.
///
/// Layering note: this resolves the backend directly (like
/// `lifecycle_container_routing_e2e.rs`) rather than threading the entry's
/// `sandbox_backend`/`container_image` through the daemon's spec→backend
/// wiring. That field-mapping is covered separately — the manifest unit tests
/// (`resolve_uses_container_backend_when_flag_set`) assert `container_mode_entry`
/// produces those fields, and `lifecycle_container_routing_e2e.rs` proves the
/// lifecycle manager honors `sandbox_backend == Some(Container)`. This e2e's job
/// is the *runtime* proof: real worker + real VM + the strict policy's flags.
fn container_backend() -> Arc<dyn kastellan_sandbox::SandboxBackend> {
    SandboxBackends::default_for_current_os()
        .resolve(Some(SandboxBackendKind::Container), Some(DEFAULT_IMAGE))
}

/// Spawn the worker in the VM, dispatch one `python.exec` with the given
/// JSON-RPC params object, return the raw result.
///
/// Uses `dispatch_with_sink` + `NoopAuditSink` so no PG cluster is needed.
/// `container_mode_entry` sets `ephemeral_scratch: false` (scratch is the
/// in-VM `/tmp` tmpfs), so no `with_scratch` call.
///
/// Fallible: `dispatch_in_container` below is the happy-path convenience
/// that unwraps this. The containment tests that must accept EITHER a
/// non-zero `exit_code` (`Ok`) OR the #539 signal-death error (`Err`) call
/// this directly instead.
async fn try_dispatch_in_container(
    payload: serde_json::Value,
) -> Result<serde_json::Value, ToolHostError> {
    let entry = container_mode_entry(
        std::path::PathBuf::from(
            kastellan_core::workers::python_exec::CONTAINER_WORKER_BIN,
        ),
        DEFAULT_IMAGE.to_string(),
        None,
        kastellan_core::worker_lifecycle::Lifecycle::SingleUse,
    );
    let backend = container_backend();
    let program = entry.binary.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let mut worker = spawn_worker(&*backend, &spec).expect("spawn worker in container");
    let result = dispatch_with_sink(
        &NoopAuditSink,
        &Vault::new(),
        &mut worker,
        "python-exec",
        "python.exec",
        payload,
    )
    .await;
    let _ = worker.close();
    result
}

/// Convenience: the happy-path dispatch, unwrapped.
async fn dispatch_in_container(payload: serde_json::Value) -> serde_json::Value {
    try_dispatch_in_container(payload)
        .await
        .expect("dispatch python.exec")
}

/// Convenience: dispatch code-only (no `params`).
async fn run_in_container(code: &str) -> serde_json::Value {
    dispatch_in_container(serde_json::json!({ "code": code })).await
}

/// Assert `err` is exactly the dispatch-level signal-death error introduced
/// by #539: an RPC error carrying `OPERATION_FAILED` whose message names the
/// killing signal (the fixed `killed by <SIG> (...)` shape built by
/// `kastellan_worker_prelude::child_exit::signal_death_message`). Local to
/// this file rather than shared — `python_exec_e2e.rs`,
/// `python_exec_container_e2e.rs` and `python_exec_firecracker_e2e.rs` are
/// three separate test binaries, each with its own call site. Sharing it
/// would not dodge a new dependency edge the way an earlier version of this
/// comment claimed: all three files already depend on `kastellan-tests-common`,
/// which already depends on `kastellan-core`, so referencing `ToolHostError`
/// costs nothing — only `kastellan-protocol` would be genuinely new, and that
/// is one line in a dev-only manifest. The real trade is smaller: this helper
/// (doc comment + body) is ~30 lines, so keeping it inline triples ~30 lines
/// of duplication across the tree to avoid a fourth crate whose only reason
/// to exist would be this one function — a defensible call either way, but
/// the dependency-edge framing was not the actual reason for it.
fn assert_is_signal_death(err: &ToolHostError) {
    match err {
        ToolHostError::Protocol(ClientError::Rpc(rpc)) => {
            assert_eq!(
                rpc.code,
                codes::OPERATION_FAILED,
                "expected the #539 signal-death error, got: {rpc:?}"
            );
            assert!(
                rpc.message.contains("killed by"),
                "signal-death message must name the killing signal: {}",
                rpc.message
            );
        }
        other => panic!(
            "expected Protocol(Rpc(_)) carrying the #539 signal-death error, got: {other:?}"
        ),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn python_exec_round_trips_through_container() {
    if skip_if_no_container_image() {
        return;
    }
    let out = run_in_container("print('hello-from-microvm')").await;
    let stdout = out["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("hello-from-microvm"),
        "expected sentinel in stdout, got: {out}"
    );
    assert_eq!(out["exit_code"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn container_enforces_mem_cap() {
    if skip_if_no_container_image() {
        return;
    }
    // Allocate ~900 MiB — above the 512 MiB cap. The VM enforces the cap, so the
    // allocation fails; under macOS Seatbelt host mode it would SUCCEED (Seatbelt
    // has no memory primitive — the parity gap this micro-VM mode closes).
    let code = "x = bytearray(900 * 1024 * 1024); print(len(x))";
    let result = try_dispatch_in_container(serde_json::json!({ "code": code })).await;
    // Two legitimate containment shapes now that a signal death is a dispatch
    // ERROR rather than a null `exit_code` (#539):
    //   (a) Ok(result) with a non-zero exit_code — observed: exit_code 1 with
    //       a Python MemoryError traceback.
    //   (b) Err(_) carrying the #539 signal-death error — the cgroup OOM
    //       killer SIGKILLs the child before it can raise.
    // Reject a clean Ok(exit_code: 0) — that would mean the 512 MiB cap was
    // NOT enforced (the Seatbelt host-mode gap this micro-VM mode closes).
    match result {
        Ok(out) => {
            assert_ne!(out["exit_code"], 0, "expected an OOM failure exit: {out}");
            let stdout = out["stdout"].as_str().unwrap_or_default();
            assert!(
                !stdout.contains(&(900 * 1024 * 1024).to_string()),
                "the allocation print must not appear — it should be killed first: {out}"
            );
        }
        Err(err) => {
            assert_is_signal_death(&err);
            assert!(
                err.to_string().contains("SIGKILL"),
                "the cgroup OOM killer sends SIGKILL: {err}"
            );
            // The teardown race the shapes above already document: the child can
            // SUCCEED at the forbidden allocation (and print its length) before it
            // is signalled. `signal_death_message` reports the captured byte count,
            // so demand it is zero — a non-zero count here would mean the child got
            // to print before dying, i.e. NOT contained.
            assert!(
                err.to_string().contains("0 B out"),
                "the child printed before dying — not contained: {err}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn container_contains_socket_attempt() {
    if skip_if_no_container_image() {
        return;
    }
    // Net::Deny + --network none: a connect to a public IP cannot succeed in the VM.
    let code = "\
import socket, sys
try:
    s = socket.create_connection(('1.1.1.1', 443), timeout=2)
    print('CONNECTED')
except Exception as e:
    print('blocked', file=sys.stderr)
";
    let result = try_dispatch_in_container(serde_json::json!({ "code": code })).await;
    // Containment guard: a SUCCESSFUL connection prints "CONNECTED" to stdout, so
    // its ABSENCE is the invariant proving egress was denied. A denied connect
    // surfaces inconsistently across harness timing, but now that a signal death
    // is a dispatch ERROR rather than a null `exit_code` (#539) there are exactly
    // two legitimate "no egress" shapes:
    //   (a) Ok(result) — the caught-ENETUNREACH path: the script runs past the
    //       try/except and exits 0 with "blocked" on stderr.
    //   (b) Err(_) carrying the #539 signal-death error — the child is torn down
    //       mid-attempt instead of reaching its `except` clause.
    // Both are legitimate "no egress" outcomes; only a real connection would ever
    // print "CONNECTED". Non-vacuity rests on
    // `python_exec_round_trips_through_container`: it proves this same harness
    // faithfully returns the child's stdout, so a connection that truly succeeded
    // could not hide.
    // NOTE: this test's non-vacuity DEPENDS on the round-trip test above staying
    // live (not `#[ignore]`d / removed) — if it ever is, a worker that never ran
    // would also print no "CONNECTED" and this guard would weaken. Keep them paired.
    match result {
        Ok(out) => {
            let stdout = out["stdout"].as_str().unwrap_or_default();
            assert!(!stdout.contains("CONNECTED"), "network must be denied (no CONNECTED): {out}");
        }
        Err(err) => {
            assert_is_signal_death(&err);
            // Same teardown race as `container_enforces_mem_cap`: the child can
            // print "CONNECTED" before being torn down. A non-zero captured byte
            // count here would hide exactly that — the connection succeeding,
            // then a kill racing the print. Zero out bytes is what makes this
            // Err arm as strong a containment proof as the Ok arm's `!contains`.
            assert!(
                err.to_string().contains("0 B out"),
                "the child printed before dying — not contained: {err}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn container_large_param_round_trips_via_file_channel() {
    if skip_if_no_container_image() {
        return;
    }
    // A >64 KiB params payload exceeds the inline env threshold, so the worker
    // takes the FILE channel: it writes `<scratch>/params.json` and points the
    // child at it via KASTELLAN_PYTHON_PARAMS_FILE. In container mode scratch
    // is the in-VM `/tmp` tmpfs (`--tmpfs /tmp`, writable even under `--read-only`)
    // and the worker runs as `nobody`. This proves that write path actually works
    // in the VM — the one fail-CLOSED path host mode covers but container mode did
    // not (`write_params_file(...)?` aborts the whole exec on any IO error, so a
    // tmpfs that `nobody` couldn't write would surface as a non-zero exit here).
    //
    // 100_000 bytes ≫ the 64 KiB inline threshold, ≪ the 1 MiB default file
    // ceiling → the File channel. The agent reads the file when the env var is
    // set, else falls back to the inline var (which would be the "{}" default →
    // KeyError → non-zero exit if the file channel silently failed).
    let blob = "A".repeat(100_000);
    let code = concat!(
        "import json, os\n",
        "p = os.environ.get('KASTELLAN_PYTHON_PARAMS_FILE')\n",
        "if p:\n",
        "    with open(p) as f:\n",
        "        params = json.load(f)\n",
        "else:\n",
        "    params = json.loads(os.environ.get('KASTELLAN_PYTHON_PARAMS', '{}'))\n",
        "b = params['blob']\n",
        "print(len(b), b[:4], b[-4:])\n",
    );
    let out = dispatch_in_container(
        serde_json::json!({ "code": code, "params": { "blob": blob } }),
    )
    .await;
    assert_eq!(
        out["exit_code"].as_i64(),
        Some(0),
        "file-channel write to the in-VM tmpfs must succeed as nobody; stderr: {}",
        out["stderr"]
    );
    assert_eq!(
        out["stdout"].as_str().unwrap_or_default().trim_end(),
        "100000 AAAA AAAA",
        "agent must read the full 100 KiB payload via the in-VM file channel: {out}"
    );
}
