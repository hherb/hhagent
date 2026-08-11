//! shell-exec: a tool worker that runs an argv from a strict allowlist and
//! returns stdout/stderr/exit code over JSON-RPC stdio. **No shell interpretation.**
//!
//! The allowlist is read once at startup from environment variable
//! `KASTELLAN_SHELL_ALLOWLIST` as a JSON array of `[argv0, argv1, ...]` patterns.
//! Each pattern is exact-match on `argv[0]` and is the *only* allowed entry
//! point. The agent core is responsible for keeping that env var deny-by-default.
//!
//! Method exposed:
//!   - `shell.exec` — params: `{ "argv": ["program", "arg1", ...] }`
//!     result: `{ "exit_code": int, "stdout": str, "stderr": str }`
//!     err code [`POLICY_DENIED`] if `argv[0]` is not on the allowlist;
//!     err code [`OPERATION_FAILED`] if the child was killed by a signal.
//!
//! `exit_code` in a result is always an integer. A child terminated by a
//! signal has no exit code at all, and until #539 that `None` was serialized
//! as `"exit_code": null` inside a *successful* result — indistinguishable
//! from a command that legitimately printed nothing. It is now an error whose
//! message names the signal and its likely cause, because the sandbox kills
//! more ordinary commands than one would guess: anything resolving a user or
//! group name (`ls -l`, `id`, `whoami`, a bare `python3`) needs `socket(2)`
//! for the NSS lookup, and `Profile::WorkerStrict` denies it by design.
//!
//! [`POLICY_DENIED`]: kastellan_protocol::codes::POLICY_DENIED
//! [`OPERATION_FAILED`]: kastellan_protocol::codes::OPERATION_FAILED

pub mod handler;
