//! The `shell.exec` handler: allowlist check, then spawn, then report.
//!
//! Split out of `main.rs` so it can be tested at all — the crate was
//! bin-only, which is why the #539 defect (a signal-killed child reported as
//! a successful call with `exit_code: null`) was never pinned by a test.

use std::collections::HashSet;
use std::process::Command;

use kastellan_protocol::{codes, server::Handler, RpcError};
use kastellan_worker_prelude::child_exit::{classify, signal_death_message, ChildEnd};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ExecParams {
    pub argv: Vec<String>,
}

pub struct ShellExecHandler {
    allowed_argv0: HashSet<String>,
}

impl ShellExecHandler {
    pub fn from_env() -> anyhow::Result<Self> {
        let raw = std::env::var("KASTELLAN_SHELL_ALLOWLIST").unwrap_or_else(|_| "[]".to_string());
        let allowed: Vec<String> = serde_json::from_str(&raw).map_err(|e| {
            anyhow::anyhow!("KASTELLAN_SHELL_ALLOWLIST is not a valid JSON array of strings: {e}")
        })?;
        Ok(Self {
            allowed_argv0: allowed.into_iter().collect(),
        })
    }
}

impl Handler for ShellExecHandler {
    fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        if method != "shell.exec" {
            return Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method {method}"),
            ));
        }
        let p: ExecParams = serde_json::from_value(params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
        let program = p.argv.first().ok_or_else(|| {
            RpcError::new(codes::INVALID_PARAMS, "argv must be non-empty")
        })?;
        if !self.allowed_argv0.contains(program) {
            return Err(RpcError::new(
                codes::POLICY_DENIED,
                format!("argv[0] {program:?} not in allowlist"),
            ));
        }

        let output = Command::new(program)
            .args(&p.argv[1..])
            .output()
            .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("exec failed: {e}")))?;

        // A signal-terminated child has no exit code. Reporting that as a
        // successful call with `"exit_code": null` is indistinguishable from a
        // command that printed nothing — the silent failure #539 was filed for.
        match classify(output.status) {
            ChildEnd::Exited(code) => Ok(serde_json::json!({
                "exit_code": code,
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
            })),
            ChildEnd::Signalled(death) => Err(RpcError::new(
                codes::OPERATION_FAILED,
                signal_death_message(&death, program, output.stdout.len(), output.stderr.len()),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler(allow: &[&str]) -> ShellExecHandler {
        ShellExecHandler {
            allowed_argv0: allow.iter().map(|s| s.to_string()).collect(),
        }
    }

    // `/bin/sh` exists on both first-class platforms and `kill -9 $$` is
    // POSIX. We assert on exit BEHAVIOUR only, never on a child's output
    // text: a test that drives a shell is portability-sensitive to output
    // format (GNU vs BSD), which has bitten this tree before.
    #[test]
    fn a_signal_killed_child_is_an_error_not_an_empty_success() {
        let mut h = handler(&["/bin/sh"]);
        let params = serde_json::json!({"argv": ["/bin/sh", "-c", "kill -9 $$"]});
        let err = h
            .call("shell.exec", params)
            .expect_err("a signal-killed child must not be a successful result");
        assert_eq!(err.code, codes::OPERATION_FAILED);
        assert!(err.message.contains("SIGKILL"), "message: {}", err.message);
    }

    #[test]
    fn a_normal_exit_still_returns_stdout_and_the_code() {
        let mut h = handler(&["/bin/sh"]);
        let params = serde_json::json!({"argv": ["/bin/sh", "-c", "printf hi; exit 3"]});
        let v = h.call("shell.exec", params).expect("a normal exit is a result");
        assert_eq!(v["exit_code"], 3);
        assert_eq!(v["stdout"], "hi");
    }

    #[test]
    fn a_non_allowlisted_argv0_is_still_policy_denied() {
        let mut h = handler(&["/bin/sh"]);
        let params = serde_json::json!({"argv": ["/usr/bin/whoami"]});
        let err = h.call("shell.exec", params).expect_err("off-allowlist must be denied");
        assert_eq!(err.code, codes::POLICY_DENIED);
    }
}
