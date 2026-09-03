//! The `shell.exec` handler: allowlist check, then spawn, then report.
//!
//! Split out of `main.rs` so it can be tested at all — the crate was
//! bin-only, which is why the #539 defect (a signal-killed child reported as
//! a successful call with `exit_code: null`) was never pinned by a test.

use std::collections::HashSet;
use std::process::Command;

use kastellan_protocol::{codes, server::Handler, RpcError};
use kastellan_worker_prelude::child_exit::{
    classify, signal_death_message, Caller, Captured, ChildEnd,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct ExecParams {
    argv: Vec<String>,
}

pub struct ShellExecHandler {
    allowed_argv0: HashSet<String>,
}

/// How many chars of the joined allowlist a denial carries. Sized to fit the
/// planner's step-detail clamp (`kastellan_protocol::STEP_ERR_DETAIL_MAX`,
/// 200 chars) after the fixed prefix, so the advice survives the render.
const ALLOWLIST_ECHO_MAX: usize = 150;

/// Truncate `s` to at most `max` chars with a trailing `…` marker.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
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
            // Do NOT echo argv[0] back (security audit 2026-09-02, finding
            // H1): the core substitutes `secret://` refs anywhere in params
            // before the call, so an injected planner could pass a ref as
            // argv[0] and read the plaintext out of this very message. The
            // core now scrubs error text too, but the worker should not hand
            // out what it was given in the first place. What the planner can
            // use instead is the operator's allowlist, which is not secret —
            // that is the repair advice this error exists to carry.
            let mut allowed: Vec<&str> = self.allowed_argv0.iter().map(String::as_str).collect();
            allowed.sort_unstable();
            return Err(RpcError::new(
                codes::POLICY_DENIED,
                format!(
                    "argv[0] not in allowlist; allowed argv[0] values: {}",
                    clip(&allowed.join(", "), ALLOWLIST_ECHO_MAX)
                ),
            ));
        }

        let output = Command::new(program)
            .args(&p.argv[1..])
            .output()
            .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("exec failed: {e}")))?;

        // A signal-terminated child has no exit code. Reporting that as a
        // successful call with `"exit_code": null` is indistinguishable from a
        // command that printed nothing — the silent failure #539 was filed for.
        //
        // Note what the error path costs: whatever the child managed to print
        // before the kill is DISCARDED (only its byte count survives), and
        // `inner_loop` aborts the rest of the plan on any `Err`. That is the
        // deliberate trade — an `RpcError` carries no result, and its `data`
        // field reaches neither the planner nor the audit row — but it means a
        // command killed on its last line loses output it had already produced.
        match classify(output.status) {
            ChildEnd::Exited(code) => Ok(serde_json::json!({
                "exit_code": code,
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
            })),
            ChildEnd::Signalled(death) => Err(RpcError::new(
                codes::OPERATION_FAILED,
                signal_death_message(
                    &death,
                    // shell-exec's caller chose the argv, so it can respell it.
                    Caller::Argv,
                    program,
                    Captured {
                        stdout_len: output.stdout.len(),
                        stderr_len: output.stderr.len(),
                    },
                ),
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

    #[test]
    fn a_denial_never_echoes_argv0_but_names_the_allowlist() {
        // argv[0] may be a substituted `secret://` ref's plaintext (audit
        // 2026-09-02, H1): the denial must not carry it, and must instead
        // carry the (non-secret) allowlist as repair advice.
        let mut h = handler(&["/bin/sh", "/usr/bin/env"]);
        let params = serde_json::json!({"argv": ["hunter2-the-secret-value"]});
        let err = h.call("shell.exec", params).expect_err("off-allowlist must be denied");
        assert!(!err.message.contains("hunter2"), "message echoed argv[0]: {}", err.message);
        assert!(err.message.contains("/bin/sh, /usr/bin/env"), "message: {}", err.message);
    }

    #[test]
    fn clip_bounds_the_allowlist_echo() {
        assert_eq!(clip("short", 10), "short");
        let long = "x".repeat(200);
        let clipped = clip(&long, 150);
        assert_eq!(clipped.chars().count(), 151);
        assert!(clipped.ends_with('…'));
    }
}
