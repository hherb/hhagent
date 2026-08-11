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
//!     err code [`POLICY_DENIED`] if argv[0] is not on the allowlist.

pub mod handler;
