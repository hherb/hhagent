//! JSON-RPC dispatch for the one method this worker serves: `python.exec`.

use std::path::PathBuf;

use kastellan_protocol::{codes, server::Handler, RpcError};
use kastellan_worker_prelude::child_exit::{
    signal_death_message, Caller, Captured, ChildEnd,
};
use serde::Deserialize;

use crate::exec::{self, run_code, serialize_params, ExecOutcome, MAX_CODE_BYTES};

/// Env var carrying the absolute interpreter path. Set by the host
/// manifest (`core/src/workers/python_exec.rs`) via `policy.env`; the
/// same name doubles as the operator's daemon-side discovery override.
pub const PYTHON_BIN_ENV: &str = "KASTELLAN_PYTHON_EXEC_PYTHON";

#[derive(Deserialize)]
struct ExecParams {
    code: String,
    #[serde(default)]
    params: Option<serde_json::Value>,
}

pub struct PythonExecHandler {
    python: PathBuf,
}

impl PythonExecHandler {
    /// Fail-closed startup: no interpreter path, no worker. (The host
    /// manifest always injects it; a bare manual spawn must not guess.)
    pub fn from_env() -> anyhow::Result<Self> {
        let raw = std::env::var(PYTHON_BIN_ENV)
            .map_err(|_| anyhow::anyhow!("{PYTHON_BIN_ENV} must be set (absolute interpreter path)"))?;
        if raw.trim().is_empty() {
            anyhow::bail!("{PYTHON_BIN_ENV} is set but empty");
        }
        let python = PathBuf::from(raw);
        if !python.is_absolute() {
            anyhow::bail!("{PYTHON_BIN_ENV} must be an absolute path, got {python:?}");
        }
        Ok(Self { python })
    }

    /// Test constructor: bypass the env (unit/integration tests inject
    /// the interpreter directly).
    pub fn with_python(python: PathBuf) -> Self {
        Self { python }
    }
}

/// Render a finished run as either a JSON-RPC result or an error.
///
/// Pure — no I/O — so the decision is testable without an interpreter. A
/// Python **exception** stays a result (nonzero `exit_code` + traceback,
/// which is what the planner iterates on); only a **signal death** becomes an
/// error, because it produces no exit code at all and previously surfaced as
/// a successful call with `"exit_code": null` (#539).
///
/// `what` is the caller-supplied label for the signal-death message's `ran:`
/// segment (see [`signal_death_message`]) — the interpreter path, so an
/// operator reading the error sees *which* interpreter died rather than the
/// bare word "python".
///
/// The signal path DISCARDS whatever the script printed before the kill (only
/// its byte count survives) and, because `inner_loop` breaks the plan on any
/// `Err`, ends the plan there. For a long-running script OOM-killed on its
/// last line that is a real loss of partial output — the deliberate trade for
/// not reporting the failure as a success, since an `RpcError` carries no
/// result and its `data` field reaches neither planner nor audit row.
pub fn outcome_to_rpc(outcome: &ExecOutcome, what: &str) -> Result<serde_json::Value, RpcError> {
    match outcome.end {
        ChildEnd::Exited(code) => Ok(serde_json::json!({
            "exit_code": code,
            "stdout": outcome.stdout,
            "stderr": outcome.stderr,
            "stdout_truncated": outcome.stdout_truncated,
            "stderr_truncated": outcome.stderr_truncated,
        })),
        ChildEnd::Signalled(death) => Err(RpcError::new(
            codes::OPERATION_FAILED,
            signal_death_message(
                &death,
                // The caller submitted source, not an argv: it cannot change
                // the interpreter flags, which `python_args()` pins to
                // `-I -S -B`. Advice naming `-S` or `ls -l` would be advice it
                // cannot act on.
                Caller::Interpreter,
                what,
                Captured {
                    stdout_len: outcome.stdout.len(),
                    stderr_len: outcome.stderr.len(),
                },
            ),
        )),
    }
}

impl Handler for PythonExecHandler {
    fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        if method != "python.exec" {
            return Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method {method}"),
            ));
        }
        let p: ExecParams = serde_json::from_value(params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
        if p.code.len() > MAX_CODE_BYTES {
            return Err(RpcError::new(
                codes::INVALID_PARAMS,
                format!("code is {} bytes; cap is {MAX_CODE_BYTES}", p.code.len()),
            ));
        }

        let params_json = serialize_params(&p.params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, e.to_string()))?;
        let file_max = exec::params_file_max(|k| std::env::var(k).ok());
        let channel = exec::decide_param_channel(params_json.len(), exec::INLINE_PARAMS_MAX, file_max)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, e.to_string()))?;

        let outcome = run_code(&self.python, &p.code, &params_json, channel)
            .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("spawn failed: {e}")))?;

        outcome_to_rpc(&outcome, &self.python.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_worker_prelude::child_exit::SignalDeath;

    fn handler() -> PythonExecHandler {
        // The interpreter is never reached by these tests (they fail
        // validation first), so a dummy path is fine.
        PythonExecHandler::with_python(PathBuf::from("/nonexistent/python3"))
    }

    /// Run `body` with [`exec::WORKER_SCRATCH_ENV`] pointed at a private dir.
    ///
    /// **Required by any test that falls through validation into `run_code`.**
    /// `run_code` resolves the scratch dir from the *real* environment and
    /// wipes it before it spawns — so with the variable unset it resolves to
    /// [`exec::SCRATCH_DIR`], `/tmp`, and the test deletes the developer's
    /// `/tmp`. It also raced the sibling `exec::tests` wipe fixtures, which
    /// live under `/tmp`, and turned that suite intermittently red in CI
    /// (issue #574: `left: 1, right: 3`).
    ///
    /// The production default is *correct* and deliberately unchanged: inside
    /// the sandbox `/tmp` **is** the worker's own scratch tmpfs, so wiping it
    /// is the intended pristine-scratch reset. It is only wrong for a unit
    /// test, which runs outside the jail against the host's real `/tmp`.
    ///
    /// `KASTELLAN_WORKER_SCRATCH` is process-global while Rust runs tests in
    /// parallel threads, so the mutex — not the ordering — is what makes this
    /// deterministic. Cleanup runs from `Drop` so a failing assertion cannot
    /// leak the variable into whatever runs next. Same shape as the mail
    /// worker's `with_out_dir`, for the same reason.
    fn with_scratch_dir<T>(tag: &str, body: impl FnOnce() -> T) -> T {
        static SCRATCH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                std::env::remove_var(exec::WORKER_SCRATCH_ENV);
                std::fs::remove_dir_all(&self.0).ok();
            }
        }

        let _guard = SCRATCH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir =
            std::env::temp_dir().join(format!("pyexec-scratch-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var(exec::WORKER_SCRATCH_ENV, &dir);
        let _restore = Restore(dir.clone());
        body()
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let err = handler()
            .call("python.evaluate", serde_json::json!({"code": "1"}))
            .unwrap_err();
        assert_eq!(err.code, codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn missing_code_is_invalid_params() {
        let err = handler().call("python.exec", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn non_string_code_is_invalid_params() {
        let err = handler()
            .call("python.exec", serde_json::json!({"code": 42}))
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn over_cap_code_is_invalid_params() {
        let big = "#".repeat(MAX_CODE_BYTES + 1);
        let err = handler()
            .call("python.exec", serde_json::json!({"code": big}))
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
        assert!(err.message.contains("cap"));
    }

    #[test]
    fn unspawnable_interpreter_is_operation_failed() {
        // Reaches `run_code`, so it must not be pointed at the real `/tmp`.
        with_scratch_dir("unspawnable", || {
            let err = handler()
                .call("python.exec", serde_json::json!({"code": "print(1)"}))
                .unwrap_err();
            assert_eq!(err.code, codes::OPERATION_FAILED);
        });
    }

    #[test]
    fn non_object_params_is_invalid_params() {
        let err = handler()
            .call("python.exec", serde_json::json!({"code": "print(1)", "params": [1, 2]}))
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
        assert!(err.message.contains("object"), "got: {}", err.message);
    }

    #[test]
    fn over_file_cap_params_is_invalid_params() {
        // A param larger than the default 1 MiB file ceiling is rejected
        // fail-closed (INVALID_PARAMS) — proves the file channel still has a
        // hard ceiling. (Env unset → default ceiling.)
        let big = "x".repeat(crate::exec::PARAMS_FILE_MAX_DEFAULT + 1024);
        let err = handler()
            .call(
                "python.exec",
                serde_json::json!({"code": "print(1)", "params": {"k": big}}),
            )
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
        assert!(err.message.contains("cap"), "got: {}", err.message);
    }

    #[test]
    fn absent_params_is_accepted_and_reaches_spawn() {
        // No `params` key: validation passes, so we fall through to the spawn,
        // which fails on the dummy interpreter → OPERATION_FAILED (not
        // INVALID_PARAMS). Proves absent params is the `{}` default, not a reject.
        //
        // "Reaches spawn" is exactly what makes the scratch guard mandatory
        // here: everything past validation runs `run_code`'s wipe first.
        with_scratch_dir("absent-params", || {
            let err = handler()
                .call("python.exec", serde_json::json!({"code": "print(1)"}))
                .unwrap_err();
            assert_eq!(err.code, codes::OPERATION_FAILED);
        });
    }

    fn outcome(end: ChildEnd) -> ExecOutcome {
        ExecOutcome {
            end,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    #[test]
    fn a_signal_killed_interpreter_is_an_error() {
        let err = outcome_to_rpc(
            &outcome(ChildEnd::Signalled(SignalDeath::from_raw(libc::SIGKILL))),
            "/usr/bin/python3",
        )
        .expect_err("a signal-killed interpreter must not be a successful result");
        assert_eq!(err.code, codes::OPERATION_FAILED);
        assert!(err.message.contains("SIGKILL"), "message: {}", err.message);
    }

    // #539/M10: the message must name the interpreter path, not the bare word
    // "python" — an operator reading the error should see *which* interpreter
    // died.
    #[test]
    fn a_signal_killed_interpreter_names_the_interpreter_path() {
        let err = outcome_to_rpc(
            &outcome(ChildEnd::Signalled(SignalDeath::from_raw(libc::SIGKILL))),
            "/usr/bin/python3",
        )
        .expect_err("a signal-killed interpreter must not be a successful result");
        assert!(
            err.message.contains("/usr/bin/python3"),
            "message must name the interpreter path: {}",
            err.message
        );
    }

    // `Captured`'s two fields are the same type and adjacent, so the ONLY
    // thing pinning this call site's argument order is a test that uses
    // distinct, non-zero counts. Every other signal-death test here (and all
    // five containment e2e assertions) exercises the zero case, where a
    // transposition is invisible — and those e2e tests read the stdout count
    // as a CONTAINMENT proof, so a swap would let a child that printed a leak
    // payload to stdout report "0 B out".
    #[test]
    fn the_byte_counts_are_passed_in_the_right_order() {
        let mut o = outcome(ChildEnd::Signalled(SignalDeath::from_raw(libc::SIGKILL)));
        o.stdout = "x".repeat(7);
        o.stderr = "y".repeat(42);
        let err = outcome_to_rpc(&o, "/usr/bin/python3")
            .expect_err("a signal-killed interpreter must not be a successful result");
        assert!(err.message.contains("7 B out"), "message: {}", err.message);
        assert!(err.message.contains("42 B err"), "message: {}", err.message);
    }

    // A SIGSYS here cannot be the `site`/`getpwuid` case that motivated #539:
    // `python_args()` pins `-I -S -B`, so `-S` is already applied. The message
    // must not tell this caller to retry with a flag it already uses.
    #[test]
    fn a_seccomp_kill_does_not_advise_flags_the_interpreter_already_uses() {
        let err = outcome_to_rpc(
            &outcome(ChildEnd::Signalled(SignalDeath::from_raw(libc::SIGSYS))),
            "/usr/bin/python3",
        )
        .expect_err("a signal-killed interpreter must not be a successful result");
        assert!(
            !err.message.contains("try `python3 -S`"),
            "message: {}",
            err.message
        );
    }

    // The documented contract that must NOT move: a Python exception is a
    // nonzero exit code plus a traceback, not an RPC error.
    #[test]
    fn a_nonzero_exit_is_still_a_result_not_an_error() {
        let mut o = outcome(ChildEnd::Exited(1));
        o.stderr = "Traceback (most recent call last): …".to_string();
        let v = outcome_to_rpc(&o, "/usr/bin/python3").expect("a Python exception is a result");
        assert_eq!(v["exit_code"], 1);
        assert!(v["stderr"].as_str().unwrap().contains("Traceback"));
    }
}
