//! Sandbox helpers: `[SKIP]` probe, backend factory, canonical
//! shell-exec policy.
//!
//! Both `sandbox_unavailable_reason` and `backend()` are cfg-gated
//! per-OS so a single call site reads cleanly on Linux + macOS without
//! per-test `#[cfg]` ladders.

use std::path::Path;

use kastellan_sandbox::{SandboxBackend, SandboxPolicy};

/// Why the per-OS sandbox backend is unusable on this host, or `None` when it
/// is fine. The string is a *reason*, with no `[SKIP]` prefix and no newlines:
/// the caller decides whether an unmet precondition is a clean skip or a hard
/// failure, and renders it accordingly.
///
/// Linux: requires bwrap + unprivileged user-namespace permission
/// (AppArmor profile installed via
/// `scripts/linux/install-bwrap-apparmor-profile.sh`).
/// macOS: requires `/usr/bin/sandbox-exec` (present on all stock
/// installs from 10.5+).
#[cfg(target_os = "linux")]
pub fn sandbox_unavailable_reason() -> Option<String> {
    use kastellan_sandbox::linux_bwrap::LinuxBwrap;
    LinuxBwrap::probe().err().map(|e| format!("bwrap probe failed: {e}"))
}

#[cfg(target_os = "macos")]
pub fn sandbox_unavailable_reason() -> Option<String> {
    use kastellan_sandbox::macos_seatbelt::MacosSeatbelt;
    MacosSeatbelt::probe()
        .err()
        .map(|e| format!("sandbox-exec probe failed: {e}"))
}

/// Returns `true` if the per-OS sandbox backend's probe fails. Caller
/// should `return` immediately to short-circuit the test.
///
/// The skip-as-pass half of [`sandbox_unavailable_reason`]; cfg-free, because
/// the per-OS split lives entirely in the probe it wraps.
pub fn skip_if_sandbox_unavailable() -> bool {
    match sandbox_unavailable_reason() {
        Some(reason) => {
            eprint!("{}", crate::skip::skip_line(&reason));
            true
        }
        None => false,
    }
}

/// Boxed per-OS [`SandboxBackend`] for use in tests that spawn a
/// real sandboxed worker. The cfg-gating mirrors `default_backend()`
/// in `kastellan_sandbox` but stays here so tests don't import a
/// production helper that may grow per-feature gates.
#[cfg(target_os = "linux")]
pub fn backend() -> Box<dyn SandboxBackend> {
    Box::new(kastellan_sandbox::linux_bwrap::LinuxBwrap::new())
}

#[cfg(target_os = "macos")]
pub fn backend() -> Box<dyn SandboxBackend> {
    Box::new(kastellan_sandbox::macos_seatbelt::MacosSeatbelt::new())
}

/// Canonical sandbox policy for the shell-exec worker.
///
/// * `fs_read` = the worker binary itself (so it can be mapped at
///   spawn).
/// * `net = Deny` — shell-exec is never a network tool.
/// * `cpu_ms = 5_000`, `mem_mb = 256` — generous defaults for the
///   `echo` happy path; the tests that hit OOM or budget paths
///   override these.
/// * `profile = WorkerStrict` — Landlock + seccomp lockdown applied
///   from inside the worker before serve_stdio.
/// * `env` carries `KASTELLAN_SHELL_ALLOWLIST` as a JSON array of
///   strings (the worker's allowlist contract).
///
/// Scope: this helper is for *direct* worker-spawn tests (e.g.
/// `shell_exec_e2e`, `audit_dispatch_e2e`) that bypass the daemon and
/// drive the worker themselves. Daemon-backed tests (e.g.
/// `cli_ask_e2e`, `observation_capture`) do not use this helper —
/// they seed the `tool_allowlists` table via
/// [`crate::allowlist::seed_tool_allowlist`] and let the daemon's
/// `build_tool_registry` pack `KASTELLAN_SHELL_ALLOWLIST` from the DB
/// at spawn time.
pub fn policy_for_shell_exec(worker: &Path, allowlist: &[&str]) -> SandboxPolicy {
    let allow_json = serde_json::to_string(allowlist).expect("serialize allowlist");
    SandboxPolicy {
        fs_read: vec![worker.to_path_buf()],
        cpu_ms: 5_000,
        mem_mb: 256,
        env: vec![("KASTELLAN_SHELL_ALLOWLIST".to_string(), allow_json)],
        ..SandboxPolicy::default()
    }
}
