//! The `shell.exec` handler: allowlist check, then spawn, then report.
//!
//! Split out of `main.rs` so it can be tested at all — the crate was
//! bin-only, which is why the #539 defect (a signal-killed child reported as
//! a successful call with `exit_code: null`) was never pinned by a test.

use std::collections::HashSet;
use std::process::Command;

use kastellan_protocol::{codes, server::Handler, RpcError};
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

        Ok(serde_json::json!({
            "exit_code": output.status.code(),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        }))
    }
}
