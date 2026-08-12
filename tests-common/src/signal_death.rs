//! Assertions for the #539 signal-death dispatch error, shared by the three
//! `python_exec_*_e2e` test binaries.
//!
//! ## Why these live here rather than inline
//!
//! `python_exec_e2e.rs`, `python_exec_container_e2e.rs` and
//! `python_exec_firecracker_e2e.rs` are three separate test binaries with five
//! containment call sites between them. Each used to carry a hand-copied
//! `assert_is_signal_death` **and** a hand-copied `". 0 B out,"` literal. Two
//! consequences, both filed as #547:
//!
//! * reverting one of the five to the unanchored `"0 B out"` — which
//!   `"10 B out"` also satisfies — was undetectable, and the leak payloads
//!   those tests guard (`"CONNECTED\n"`, and a 943718400-byte allocation
//!   printed as `"943718400\n"`) are *exactly* 10 bytes;
//! * a legitimate re-tune of the worker-side prose failed five containment
//!   assertions at once, reading as a containment regression rather than a
//!   wording change.
//!
//! The zero-stdout question is now answered by
//! [`kastellan_worker_prelude::child_exit::reports_zero_stdout`] — the crate
//! that renders the string — so the predicate and its producer cannot drift.

use kastellan_core::tool_host::ToolHostError;
use kastellan_protocol::{client::ClientError, codes};
use kastellan_worker_prelude::child_exit::reports_zero_stdout;

/// Assert `err` is exactly the dispatch-level signal-death error introduced by
/// #539: an RPC error carrying `OPERATION_FAILED` whose message names the
/// killing signal.
///
/// Deliberately narrow. Every *other* `ToolHostError` shape means the worker
/// never ran, died before answering, or refused the call — none of which prove
/// containment, and all of which would otherwise be laundered into a passing
/// "contained" verdict by a bare `Err(_)` arm. A never-started worker is
/// `Sandbox(_)`; a worker that died mid-call is `Protocol(EarlyExit)`; a wrong
/// method is `METHOD_NOT_FOUND`; a failed spawn is `OPERATION_FAILED` without
/// `killed by`. All four fail here.
pub fn assert_is_signal_death(err: &ToolHostError) {
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

/// Assert `err` is a signal death **and** that the child printed nothing
/// before it died — without demanding a particular signal.
///
/// The zero-stdout half is what makes an `Err` arm as strong a containment
/// proof as the `Ok` arm's `!stdout.contains(payload)`: the child can win the
/// race, print its leak payload, and only then be torn down. A non-zero
/// captured stdout count means exactly that — the payload escaped and the kill
/// merely followed it.
///
/// Use this where the teardown signal is genuinely not determined (a denied
/// connect surfaces inconsistently across harness timing); use
/// [`assert_contained_by_signal`] where the mechanism pins one signal.
pub fn assert_contained_signal_death(err: &ToolHostError) {
    assert_is_signal_death(err);
    let msg = err.to_string();
    assert!(
        reports_zero_stdout(&msg),
        "the child printed before dying — not contained: {msg}"
    );
}

/// [`assert_contained_signal_death`], plus: the kill was by `expect_signal`.
///
/// Only for call sites where the containment mechanism determines the signal —
/// the cgroup OOM killer always sends SIGKILL, and a seccomp denial always
/// sends SIGSYS. Naming a signal the mechanism does not guarantee would make
/// the test flaky rather than stricter.
pub fn assert_contained_by_signal(err: &ToolHostError, expect_signal: &str) {
    assert_contained_signal_death(err);
    let msg = err.to_string();
    assert!(
        msg.contains(expect_signal),
        "expected a {expect_signal} kill: {msg}"
    );
}

/// Assert an `Ok` dispatch result is a real worker result object reporting a
/// non-zero exit.
///
/// Two failures this rules out that `assert_ne!(v["exit_code"], 0)` does not:
///
/// * **`exit_code: null` passes `assert_ne!(.., 0)`**, because
///   `Value::Null != Value::from(0)`. That is #539 itself — the containment
///   tests were green for years on exactly this comparison — so a worker
///   binary that reverts the fix, or a stale one predating it, would take the
///   `Ok` arm and pass while reporting the very bug under repair.
/// * **A result with no `exit_code` at all.** `dispatch_with_sink` substitutes
///   an injection placeholder (`{injection_blocked, note, score,
///   reason_codes}`) when the CASSANDRA screen blocks a Strict-profile
///   worker's output, and python-exec is on the Strict list. Reading
///   `v["stdout"]` out of that shape yields `Null` → `""`, so a bare
///   `!stdout.contains(payload)` passes vacuously.
pub fn assert_nonzero_exit(v: &serde_json::Value) {
    let code = v["exit_code"].as_i64().unwrap_or_else(|| {
        panic!(
            "worker returned no integer `exit_code` — dispatch broken, result shape \
             changed (injection placeholder?), or a signal death reported as a null \
             exit (#539); not contained: {v}"
        )
    });
    assert_ne!(code, 0, "expected a failure exit, got a clean 0: {v}");
}

/// Read a result object's `stdout`, panicking if the result has no `stdout`
/// string at all.
///
/// `v["stdout"].as_str().unwrap_or_default()` silently yields `""` for a
/// malformed or substituted result, which makes every `!contains(payload)`
/// containment assertion pass vacuously. This is the guard the container and
/// microvm net-denial tests used to carry as
/// `assert!(out.get("exit_code").is_some())` before it was dropped.
pub fn stdout_of(v: &serde_json::Value) -> &str {
    v["stdout"].as_str().unwrap_or_else(|| {
        panic!(
            "worker returned no `stdout` string — dispatch broken or result shape \
             changed (injection placeholder?), not contained: {v}"
        )
    })
}
