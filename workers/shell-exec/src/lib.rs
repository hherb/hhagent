//! shell-exec: a tool worker that runs an argv from a strict allowlist and
//! returns stdout/stderr/exit code over JSON-RPC stdio. **No shell interpretation.**
//!
//! The allowlist is read once at startup from environment variable
//! `KASTELLAN_SHELL_ALLOWLIST` as a flat JSON array of strings, each one an
//! exact-match pattern for `argv[0]` alone (not the whole argv). Any string
//! in the array is an allowed entry point. The agent core is responsible for
//! keeping that env var deny-by-default.
//!
//! Method exposed:
//!   - `shell.exec` — params: `{ "argv": ["program", "arg1", ...] }`
//!     result: `{ "exit_code": int, "stdout": str, "stderr": str }`
//!     err code [`INVALID_PARAMS`] if `params` doesn't deserialize, or
//!     `argv` is empty;
//!     err code [`POLICY_DENIED`] if `argv[0]` is not on the allowlist;
//!     err code [`OPERATION_FAILED`] if spawning the child fails, or the
//!     child was killed by a signal;
//!     err code [`METHOD_NOT_FOUND`] for any method other than `shell.exec`.
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
//! [`INVALID_PARAMS`]: kastellan_protocol::codes::INVALID_PARAMS
//! [`POLICY_DENIED`]: kastellan_protocol::codes::POLICY_DENIED
//! [`OPERATION_FAILED`]: kastellan_protocol::codes::OPERATION_FAILED
//! [`METHOD_NOT_FOUND`]: kastellan_protocol::codes::METHOD_NOT_FOUND

pub mod handler;
