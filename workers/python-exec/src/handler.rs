//! JSON-RPC dispatch for the one method this worker serves: `python.exec`.

use std::path::PathBuf;

use kastellan_protocol::{codes, server::Handler, RpcError};
use kastellan_worker_prelude::child_exit::{signal_death_message, ChildEnd};
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
            signal_death_message(&death, what, outcome.stdout.len(), outcome.stderr.len()),
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
        let err = handler()
            .call("python.exec", serde_json::json!({"code": "print(1)"}))
            .unwrap_err();
        assert_eq!(err.code, codes::OPERATION_FAILED);
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
        let err = handler()
            .call("python.exec", serde_json::json!({"code": "print(1)"}))
            .unwrap_err();
        assert_eq!(err.code, codes::OPERATION_FAILED);
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
            &outcome(ChildEnd::Signalled(SignalDeath::from_signal(libc::SIGKILL))),
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
            &outcome(ChildEnd::Signalled(SignalDeath::from_signal(libc::SIGKILL))),
            "/usr/bin/python3",
        )
        .expect_err("a signal-killed interpreter must not be a successful result");
        assert!(
            err.message.contains("/usr/bin/python3"),
            "message must name the interpreter path: {}",
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
