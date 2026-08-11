# A signal-killed child must be loud — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A child process terminated by a signal must surface as an RPC error naming the signal and its likely cause, in both workers that spawn children — instead of a successful result carrying `exit_code: null` and empty output.

**Architecture:** One pure classifier + one pure message builder in `kastellan-worker-prelude` (the crate both workers already link, and which already depends on `kastellan-protocol`, so the message's length budget comes from the shared `STEP_ERR_DETAIL_MAX` rather than a copy). `shell-exec` gains a `[lib]` target so its handler is testable at all; `python-exec` changes `ExecOutcome.exit_code: Option<i32>` to a `ChildEnd`, which makes the silent state **unrepresentable** rather than merely discouraged.

**Tech Stack:** Rust 2021 workspace, `libc` (already an unconditional dependency of the prelude), `serde_json`, `kastellan-protocol` JSON-RPC.

**Spec:** [`docs/superpowers/specs/2026-08-11-signal-killed-child-is-loud-design.md`](../specs/2026-08-11-signal-killed-child-is-loud-design.md) — read §1.2 (the measured table) before starting; it is why this is not a python3 fix.

## Global Constraints

- **Run every cargo command in the FOREGROUND.** Never background a `cargo test`/`cargo clippy` and poll it.
- **On the Mac, use a scratch target dir:** `export CARGO_TARGET_DIR=$HOME/.cache/kastellan-sdd-target` — the IDE's rust-analyzer holds `target/debug/.cargo-lock` and CLI cargo otherwise blocks. Source cargo first: `source "$HOME/.cargo/env"`.
- **`git add <exact paths>` only. Never `git add -A`** — untracked files in this tree must stay out of commits.
- **Clippy is enforced:** `cargo clippy --workspace --all-targets -- -D warnings` must stay at exit 0.
- **Signal numbers come from `libc` constants, never integer literals.** `SIGSYS` is 31 on Linux and **12** on macOS; `SIGXCPU` is 24 and 30. A literal is wrong on one of our two first-class platforms.
- **`workers/prelude/src/lib.rs` has `#![deny(missing_debug_implementations)]`** — every public type you add needs `#[derive(Debug)]`.
- **The new module is `#[cfg(unix)]`, not `cfg(target_os = "linux")`.** Both first-class platforms are Unix, so both gate hosts compile *and run* the tests.
- **Message ordering rule (load-bearing):** diagnosis first, advice second, caller-supplied variable text **last**, in every arm. The planner sees only `code` + a `STEP_ERR_DETAIL_MAX`-clamped `message`; anything after the variable segment can be clipped away.
- The exact prose of the causes is expected to be **re-tuned after live use**. Tests pin the *structure* (order, budget, code, signal naming) — do not write a test that asserts a whole cause sentence verbatim, or every future wording tweak becomes a test edit.

---

## Task 0: Branch

Before Task 1, from an up-to-date `main` (currently `45e453dc`, #540 merged):

```sh
git checkout main && git pull --ff-only
git checkout -b fix/539-signal-death-is-loud
```

Every task below commits to that branch. The spec and this plan are already on
`main` (`49e0ff1c` and its follow-up), so the branch carries code only.

---

## File Structure

| File | Responsibility |
| --- | --- |
| `workers/prelude/src/child_exit.rs` | **new.** Pure: `ChildEnd`, `SignalDeath`, `classify`, `signal_death_message`, plus their tests. No I/O. |
| `workers/prelude/src/lib.rs` | one `#[cfg(unix)] pub mod child_exit;` line. |
| `workers/shell-exec/src/handler.rs` | **new.** `ShellExecHandler` moved here verbatim (Task 2), then given the signal arm (Task 3) and its tests. |
| `workers/shell-exec/src/lib.rs` | **new.** Crate doc + `pub mod handler;`. |
| `workers/shell-exec/src/main.rs` | shrinks to a thin binary entry point. |
| `workers/shell-exec/Cargo.toml` | gains a `[lib]` section. |
| `workers/python-exec/src/exec/mod.rs` | `ExecOutcome.exit_code: Option<i32>` → `end: ChildEnd`; one construction site at `:398`. |
| `workers/python-exec/src/handler.rs` | maps `ExecOutcome` → `Result<Value, RpcError>` through a new pure function. |

---

### Task 1: The pure classifier and message builder

**Files:**
- Create: `workers/prelude/src/child_exit.rs`
- Modify: `workers/prelude/src/lib.rs` (module declaration, next to `pub mod rlimit;`)
- Test: inline `#[cfg(test)] mod tests` in `workers/prelude/src/child_exit.rs`

**Interfaces:**
- Consumes: `kastellan_protocol::STEP_ERR_DETAIL_MAX` (already a dependency), `libc` (already an unconditional dependency).
- Produces, for Tasks 3 and 4:
  - `pub enum ChildEnd { Exited(i32), Signalled(SignalDeath) }`
  - `pub struct SignalDeath` with `pub fn from_signal(signal: i32) -> Self`, `pub fn signal(&self) -> i32`, `pub fn name(&self) -> Option<&'static str>`, `pub fn cause(&self) -> &'static str`
  - `pub fn classify(status: std::process::ExitStatus) -> ChildEnd`
  - `pub fn signal_death_message(death: &SignalDeath, what: &str, stdout_len: usize, stderr_len: usize) -> String`

- [ ] **Step 1: Write the failing tests**

Create `workers/prelude/src/child_exit.rs` containing **only** the test module for now (the code under test comes in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    // `ExitStatus::from_raw` takes a wait(2) status WORD, not an exit code and
    // not a signal number: the low 7 bits are the terminating signal, and a
    // normal exit puts its code in the high byte. So `from_raw(2 << 8)` is
    // "exited with 2" and `from_raw(SIGKILL)` is "killed by SIGKILL".
    #[test]
    fn a_normal_exit_classifies_as_exited() {
        assert_eq!(classify(ExitStatus::from_raw(0)), ChildEnd::Exited(0));
        assert_eq!(classify(ExitStatus::from_raw(2 << 8)), ChildEnd::Exited(2));
    }

    #[test]
    fn a_signal_death_classifies_as_signalled() {
        let end = classify(ExitStatus::from_raw(libc::SIGKILL));
        match end {
            ChildEnd::Signalled(d) => {
                assert_eq!(d.signal(), libc::SIGKILL);
                assert_eq!(d.name(), Some("SIGKILL"));
            }
            other => panic!("expected Signalled, got {other:?}"),
        }
    }

    // The one test that can catch a hard-coded 31: SIGSYS is 31 on Linux and
    // 12 on macOS, so a literal table is wrong on exactly one gate host and
    // agrees with itself on the other.
    #[test]
    fn signal_names_come_from_libc_not_literals() {
        assert_eq!(SignalDeath::from_signal(libc::SIGSYS).name(), Some("SIGSYS"));
        assert_eq!(SignalDeath::from_signal(libc::SIGXCPU).name(), Some("SIGXCPU"));
        assert_eq!(SignalDeath::from_signal(libc::SIGSEGV).name(), Some("SIGSEGV"));
    }

    #[test]
    fn an_unknown_signal_is_named_by_number_and_invents_no_cause() {
        let d = SignalDeath::from_signal(64);
        assert_eq!(d.name(), None);
        let msg = signal_death_message(&d, "/usr/bin/thing", 0, 0);
        assert!(msg.contains("signal 64"), "msg: {msg}");
    }

    #[test]
    fn a_seccomp_kill_says_so_and_offers_a_repair() {
        let d = SignalDeath::from_signal(libc::SIGSYS);
        let msg = signal_death_message(&d, "/usr/bin/python3", 0, 0);
        assert!(msg.contains("SIGSYS"), "msg: {msg}");
        // Structure, not prose: the message must offer *something* actionable
        // for the class measured in the spec (§1.2).
        assert!(msg.contains("socket(2)"), "msg: {msg}");
    }

    // The load-bearing one. `what` is caller-supplied and unbounded; every
    // word of diagnosis and advice must sit BEFORE it, and inside the clamp,
    // for every signal in the table. Asserting an index ordering (rather than
    // only a total length) is what makes this fail if the arms are reordered.
    #[test]
    fn the_advice_survives_the_clamp_for_the_longest_command_it_can_quote() {
        let longest = "/".repeat(kastellan_protocol::STEP_ERR_DETAIL_MAX * 4);
        for sig in [libc::SIGSYS, libc::SIGKILL, libc::SIGXCPU, libc::SIGSEGV, 64] {
            let d = SignalDeath::from_signal(sig);
            let msg = signal_death_message(&d, &longest, 0, 0);
            let what_at = msg.find(&longest).expect("the command must be quoted");
            // Everything except the caller's own string fits in the budget.
            assert!(
                what_at <= kastellan_protocol::STEP_ERR_DETAIL_MAX,
                "signal {sig}: advice ends at {what_at}, past the clamp: {msg}"
            );
            // And the diagnosis genuinely precedes it.
            let name_at = msg.find("killed by").expect("diagnosis must be present");
            assert!(name_at < what_at, "signal {sig}: diagnosis after the command: {msg}");
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-prelude child_exit
```

Expected: **compile error** — `classify`, `ChildEnd`, `SignalDeath`, `signal_death_message` do not exist. (A compile failure is the correct RED here; there is no implementation to run yet.)

- [ ] **Step 3: Write the implementation**

Prepend to `workers/prelude/src/child_exit.rs`, above the test module:

```rust
//! How a child process ended, and how to say so in one sentence the planner
//! can act on.
//!
//! ## Why this is not `ExitStatus::code()`
//!
//! On Unix `ExitStatus::code()` returns `None` **exactly** when the child was
//! terminated by a signal. Both workers that spawn children used to serialize
//! that `None` straight into a successful result as `"exit_code": null`, which
//! is indistinguishable from a command that legitimately printed nothing —
//! so a seccomp kill, an OOM kill and a CPU-budget kill all read as success
//! (#539). [`ChildEnd`] makes the two cases separate variants, so a caller
//! cannot serialize one as the other by accident.
//!
//! ## Why it lives in the prelude
//!
//! `shell-exec` and `python-exec` both link this crate, and this crate links
//! `kastellan-protocol` — so [`signal_death_message`] can size itself against
//! the shared [`kastellan_protocol::STEP_ERR_DETAIL_MAX`] instead of a
//! hand-synced copy. A per-worker copy would let a core-side clamp change
//! silently truncate the advice with every test on both sides still green,
//! which is the trap #536 documented.
//!
//! ## Cross-platform
//!
//! `#[cfg(unix)]`, which covers **both** first-class platforms, so both gate
//! hosts compile and run this. Signal numbers come from `libc` because they
//! differ: `SIGSYS` is 31 on Linux and 12 on macOS.

use std::process::ExitStatus;

/// How a child process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildEnd {
    /// Exited normally with this status code.
    Exited(i32),
    /// Terminated by a signal — the case `ExitStatus::code()` renders `None`.
    Signalled(SignalDeath),
}

/// A child terminated by `signal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalDeath {
    signal: i32,
}

impl SignalDeath {
    /// Build one directly. Public so tests (and any future caller that already
    /// knows the number) need not fabricate an [`ExitStatus`].
    pub fn from_signal(signal: i32) -> Self {
        Self { signal }
    }

    /// The raw signal number as the kernel reported it.
    pub fn signal(&self) -> i32 {
        self.signal
    }

    /// The conventional name, or `None` for a signal this table does not
    /// enumerate — in which case callers say "signal N" rather than inventing
    /// a name for it.
    pub fn name(&self) -> Option<&'static str> {
        Some(match self.signal {
            s if s == libc::SIGSYS => "SIGSYS",
            s if s == libc::SIGKILL => "SIGKILL",
            s if s == libc::SIGXCPU => "SIGXCPU",
            s if s == libc::SIGSEGV => "SIGSEGV",
            s if s == libc::SIGBUS => "SIGBUS",
            s if s == libc::SIGILL => "SIGILL",
            s if s == libc::SIGABRT => "SIGABRT",
            s if s == libc::SIGFPE => "SIGFPE",
            s if s == libc::SIGPIPE => "SIGPIPE",
            s if s == libc::SIGTERM => "SIGTERM",
            s if s == libc::SIGINT => "SIGINT",
            _ => return None,
        })
    }

    /// What that signal most plausibly means for a worker's child, phrased for
    /// a reader that is either the planner or an operator.
    ///
    /// The `SIGSYS` gloss names `socket(2)` deliberately: measured on the DGX,
    /// every user- or group-name lookup in the jail dies this way (glibc NSS
    /// opens a socket, and `WorkerStrict` denies it by design), which covers
    /// `ls -l`, `id`, `whoami` and a bare `python3` alike. See the spec's §1.2
    /// table.
    pub fn cause(&self) -> &'static str {
        match self.signal {
            s if s == libc::SIGSYS => {
                "blocked syscall — a user/group-name lookup needs socket(2), \
                 which the sandbox denies; try `ls` without `-l`, or `python3 -S`"
            }
            s if s == libc::SIGKILL => "the memory cap (cgroup OOM) or an external kill",
            s if s == libc::SIGXCPU => "the CPU budget was exhausted",
            s if s == libc::SIGSEGV
                || s == libc::SIGBUS
                || s == libc::SIGILL
                || s == libc::SIGABRT
                || s == libc::SIGFPE =>
            {
                "the command crashed"
            }
            s if s == libc::SIGPIPE => "wrote to a closed pipe",
            s if s == libc::SIGTERM || s == libc::SIGINT => "terminated on request",
            _ => "terminated by a signal",
        }
    }
}

/// Classify a finished child's [`ExitStatus`].
pub fn classify(status: ExitStatus) -> ChildEnd {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => ChildEnd::Exited(code),
        // `signal()` is `Some` whenever `code()` is `None` for a *waited* child;
        // `unwrap_or(0)` keeps this total rather than panicking on a status word
        // no `wait(2)` we make can produce, and 0 falls through to the unnamed
        // arm ("signal 0") rather than being mistaken for a real signal.
        None => ChildEnd::Signalled(SignalDeath::from_signal(status.signal().unwrap_or(0))),
    }
}

/// One sentence for a signal-killed child.
///
/// **Ordering is load-bearing.** The planner sees only the RPC error's `code`
/// and a `message` clamped to [`kastellan_protocol::STEP_ERR_DETAIL_MAX`], so
/// the diagnosis comes first, the advice second, and the caller-supplied
/// `what` — the only unbounded segment — comes last. Anything placed after it
/// can be clipped away entirely. Byte counts are reported instead of the
/// captured bytes themselves: an RPC error carries no result, and `RpcError`'s
/// `data` field reaches neither the planner (`map_dispatch_result` keeps only
/// `code` + `message`) nor the audit row (which records `e.to_string()`).
pub fn signal_death_message(
    death: &SignalDeath,
    what: &str,
    stdout_len: usize,
    stderr_len: usize,
) -> String {
    let label = match death.name() {
        Some(n) => n.to_string(),
        None => format!("signal {}", death.signal()),
    };
    format!(
        "killed by {label} ({cause}). {stdout_len} B out, {stderr_len} B err. ran: {what}",
        cause = death.cause(),
    )
}
```

Then add the module declaration to `workers/prelude/src/lib.rs`, immediately above `pub mod rlimit;`:

```rust
#[cfg(unix)]
pub mod child_exit;
```

- [ ] **Step 4: Run the tests to verify they pass**

```sh
cargo test -p kastellan-worker-prelude child_exit
```

Expected: 6 passed, 0 failed. If `the_advice_survives_the_clamp…` fails, the fixed prefix is too long for the budget — shorten the `SIGSYS` cause, do **not** raise the budget or move `what` earlier.

- [ ] **Step 5: Clippy, then commit**

```sh
cargo clippy -p kastellan-worker-prelude --all-targets -- -D warnings
git add workers/prelude/src/child_exit.rs workers/prelude/src/lib.rs
git commit -m "feat(prelude): classify a child's exit and name a signal death (#539)"
```

---

### Task 2: Give `shell-exec` a lib target — pure movement, no behaviour change

**Files:**
- Create: `workers/shell-exec/src/lib.rs`, `workers/shell-exec/src/handler.rs`
- Modify: `workers/shell-exec/src/main.rs`, `workers/shell-exec/Cargo.toml`

**Interfaces:**
- Consumes: nothing new.
- Produces, for Task 3: `kastellan_worker_shell_exec::handler::ShellExecHandler`, with `pub fn from_env() -> anyhow::Result<Self>` unchanged.

**Why this is its own task and its own commit:** the crate is bin-only today, so **no test anywhere in the tree can call its handler** — which is why the #539 defect was never pinned. Moving the code with zero behaviour change, in a separate commit, keeps the movement diff reviewable on its own (the `boot_supervisor/tests.rs` precedent) and makes Task 3's diff show only the new arm.

- [ ] **Step 1: Add the `[lib]` section to `workers/shell-exec/Cargo.toml`**

Insert immediately above the existing `[[bin]]` block:

```toml
[lib]
name = "kastellan_worker_shell_exec"
path = "src/lib.rs"
```

- [ ] **Step 2: Create `workers/shell-exec/src/lib.rs`**

Move the existing crate-level doc comment from `main.rs` here verbatim (the `//! shell-exec: a tool worker that runs an argv…` block, all 12 lines), then:

```rust
pub mod handler;
```

- [ ] **Step 3: Create `workers/shell-exec/src/handler.rs`**

Move `ExecParams`, `ShellExecHandler`, its `from_env`, and its `impl Handler` **verbatim** from `main.rs` — no edits beyond making the two types `pub`:

```rust
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

// … from_env() and impl Handler moved verbatim from main.rs …
```

- [ ] **Step 4: Shrink `workers/shell-exec/src/main.rs` to the binary entry point**

The whole file becomes (mirroring `workers/python-exec/src/main.rs`):

```rust
//! Binary entry point: env-resolved allowlist, then the prelude's lockdown +
//! serve loop (Landlock + seccomp + rlimit before any I/O).

use kastellan_worker_prelude::serve_stdio;
use kastellan_worker_shell_exec::handler::ShellExecHandler;

fn main() -> anyhow::Result<()> {
    let mut handler = ShellExecHandler::from_env()?;
    serve_stdio(&mut handler)?;
    Ok(())
}
```

- [ ] **Step 5: Verify the move changed nothing**

```sh
cargo build -p kastellan-worker-shell-exec
cargo clippy -p kastellan-worker-shell-exec --all-targets -- -D warnings
cargo test -p kastellan-worker-shell-exec
```

Expected: builds clean, clippy exit 0, `0 passed` (the crate has no tests yet — that is the point of this task).

- [ ] **Step 6: Commit**

```bash
git add workers/shell-exec/Cargo.toml workers/shell-exec/src/lib.rs \
        workers/shell-exec/src/handler.rs workers/shell-exec/src/main.rs
git commit -m "refactor(shell-exec): split the handler into a lib target (#539)

Pure movement, no behaviour change. The crate was bin-only, so no test
could reach ShellExecHandler — which is why #539 went unpinned."
```

---

### Task 3: `shell-exec` reports a signal death as an error

**Files:**
- Modify: `workers/shell-exec/src/handler.rs`
- Test: inline `#[cfg(test)] mod tests` in `workers/shell-exec/src/handler.rs`

**Interfaces:**
- Consumes: `kastellan_worker_prelude::child_exit::{classify, signal_death_message, ChildEnd}` (Task 1).
- Produces: nothing further tasks depend on.

- [ ] **Step 1: Write the failing tests**

Append to `workers/shell-exec/src/handler.rs`:

```rust
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
```

- [ ] **Step 2: Run the tests to verify the right one fails**

```sh
cargo test -p kastellan-worker-shell-exec
```

Expected: `a_signal_killed_child_is_an_error_not_an_empty_success` **FAILS** with "a signal-killed child must not be a successful result" (the handler currently returns `Ok`). The other two pass — they pin behaviour this task must not disturb.

- [ ] **Step 3: Write the implementation**

In `workers/shell-exec/src/handler.rs`, add the import:

```rust
use kastellan_worker_prelude::child_exit::{classify, signal_death_message, ChildEnd};
```

and replace the `Ok(serde_json::json!({ … "exit_code": output.status.code(), … }))` tail of `call` with:

```rust
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
```

Note `program` is already bound above as `&String` from `p.argv.first()`; if the borrow checker objects because `p.argv[1..]` was borrowed, clone it into a `String` before the `Command` call.

- [ ] **Step 4: Run the tests to verify they pass**

```sh
cargo test -p kastellan-worker-shell-exec
```

Expected: 3 passed, 0 failed.

- [ ] **Step 5: Mutation check — run it, do not assume it**

Temporarily change the `ChildEnd::Exited(code)` arm back to serializing `output.status.code()` for **both** arms (i.e. delete the `Signalled` arm's error and return `Ok` with a null code). Re-run:

```sh
cargo test -p kastellan-worker-shell-exec
```

Expected: **exactly** `a_signal_killed_child_is_an_error_not_an_empty_success` fails. If any other test fails, or if it passes, the test is not pinning what it claims. Revert the mutation before committing.

- [ ] **Step 6: Clippy, then commit**

```sh
cargo clippy -p kastellan-worker-shell-exec --all-targets -- -D warnings
git add workers/shell-exec/src/handler.rs
git commit -m "fix(shell-exec): a signal-killed child is an error, not an empty success (closes #539)"
```

---

### Task 4: `python-exec` cannot represent the silent state

**Files:**
- Modify: `workers/python-exec/src/exec/mod.rs` (the `ExecOutcome` struct ~`:199`, the single construction site ~`:398`)
- Modify: `workers/python-exec/src/handler.rs`
- Test: inline `#[cfg(test)] mod tests` in `workers/python-exec/src/handler.rs`

**Interfaces:**
- Consumes: `kastellan_worker_prelude::child_exit::{classify, signal_death_message, ChildEnd}` (Task 1).
- Produces: `pub fn outcome_to_rpc(outcome: &ExecOutcome) -> Result<serde_json::Value, RpcError>` in `handler.rs`.

**Why a pure mapping function here rather than a spawning test:** `run_code` needs a real interpreter, and a test that skips when one is absent reports green without having checked anything — the skip-as-pass shape this tree distrusts. Splitting the mapping out makes the decision testable with a hand-built `ExecOutcome`, and the *classification* half is already pinned in Task 1. The old code additionally becomes **impossible to restore**: `exit_code` no longer exists as an `Option`, so the mutation is a compile error rather than a silent regression.

- [ ] **Step 1: Write the failing tests**

Append to the existing `#[cfg(test)] mod tests` in `workers/python-exec/src/handler.rs`:

```rust
    use crate::exec::ExecOutcome;
    use kastellan_worker_prelude::child_exit::{ChildEnd, SignalDeath};

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
        let err = outcome_to_rpc(&outcome(ChildEnd::Signalled(SignalDeath::from_signal(
            libc::SIGKILL,
        ))))
        .expect_err("a signal-killed interpreter must not be a successful result");
        assert_eq!(err.code, codes::OPERATION_FAILED);
        assert!(err.message.contains("SIGKILL"), "message: {}", err.message);
    }

    // The documented contract that must NOT move: a Python exception is a
    // nonzero exit code plus a traceback, not an RPC error.
    #[test]
    fn a_nonzero_exit_is_still_a_result_not_an_error() {
        let mut o = outcome(ChildEnd::Exited(1));
        o.stderr = "Traceback (most recent call last): …".to_string();
        let v = outcome_to_rpc(&o).expect("a Python exception is a result");
        assert_eq!(v["exit_code"], 1);
        assert!(v["stderr"].as_str().unwrap().contains("Traceback"));
    }
```

`libc` is not yet a dependency of `kastellan-worker-python-exec`, and the crate has **no `[dev-dependencies]` section at all** — this creates the first one. Append to `workers/python-exec/Cargo.toml`, after the `[dependencies]` block:

```toml
[dev-dependencies]
# Test-only: signal numbers differ across our two first-class platforms
# (SIGKILL agrees, SIGSYS does not), so even a test names them via libc.
libc = "0.2"
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
cargo test -p kastellan-worker-python-exec
```

Expected: **compile error** — `ExecOutcome` has no field `end`, and `outcome_to_rpc` does not exist.

- [ ] **Step 3: Write the implementation**

In `workers/python-exec/src/exec/mod.rs`, replace the `exit_code` field (and its doc comment naming the three signal causes) with:

```rust
    /// How the interpreter ended. Was `Option<i32>` until #539: a `None` there
    /// was serialized as `"exit_code": null` inside a *successful* result, so
    /// a seccomp kill, a cgroup OOM kill and a CPU-budget kill were all
    /// indistinguishable from a script that printed nothing. The enum makes
    /// that state unrepresentable rather than merely discouraged.
    pub end: ChildEnd,
```

with `use kastellan_worker_prelude::child_exit::{classify, ChildEnd};` at the top of the module, and change the single construction site (~`:398`) from `exit_code: status.code(),` to `end: classify(status),`.

In `workers/python-exec/src/handler.rs`, add the pure mapping function above the `impl Handler` block:

```rust
/// Render a finished run as either a JSON-RPC result or an error.
///
/// Pure — no I/O — so the decision is testable without an interpreter. A
/// Python **exception** stays a result (nonzero `exit_code` + traceback,
/// which is what the planner iterates on); only a **signal death** becomes an
/// error, because it produces no exit code at all and previously surfaced as
/// a successful call with `"exit_code": null` (#539).
pub fn outcome_to_rpc(outcome: &ExecOutcome) -> Result<serde_json::Value, RpcError> {
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
            signal_death_message(&death, "python", outcome.stdout.len(), outcome.stderr.len()),
        )),
    }
}
```

and replace the handler's trailing `Ok(serde_json::json!({ … }))` with:

```rust
        outcome_to_rpc(&outcome)
```

- [ ] **Step 4: Run the tests to verify they pass**

```sh
cargo test -p kastellan-worker-python-exec
```

Expected: all previously-passing tests still pass, plus the 2 new ones.

- [ ] **Step 5: Clippy, then commit**

```sh
cargo clippy -p kastellan-worker-python-exec --all-targets -- -D warnings
git add workers/python-exec/Cargo.toml workers/python-exec/src/exec/mod.rs \
        workers/python-exec/src/handler.rs
git commit -m "fix(python-exec): a signal death cannot be reported as a null exit code (#539)"
```

---

### Task 5: Whole-workspace gate on both hosts

**Files:** none — this task verifies the previous four.

- [ ] **Step 1: Predict the test delta before running**

Write down the expected count: baseline **3135** (the DGX figure at the #533 tip, HANDOVER) **+ 11** = **3146** — 6 in `child_exit`, 3 in shell-exec's handler, 2 in python-exec's handler. Reconcile any mismatch rather than accepting it; a surprise delta has twice now been a test running on a host for the first time, which is information worth having.

- [ ] **Step 2: Mac gate**

```sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR=$HOME/.cache/kastellan-sdd-target
cargo test -p kastellan-worker-prelude -p kastellan-worker-shell-exec -p kastellan-worker-python-exec
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: 0 failures, clippy exit 0. The Mac leg matters here specifically because `SIGSYS`/`SIGXCPU` have **different numbers** on this host — `signal_names_come_from_libc_not_literals` is the test that only fails here if the table was hard-coded.

- [ ] **Step 3: DGX gate (authoritative)**

Write the log to `$HOME`, never `/tmp` — `/tmp` is scrubbed mid-run on both hosts and has eaten a finished gate's log before:

```sh
ssh dgx 'cd ~/src/kastellan && git fetch origin && git checkout fix/539-signal-death-is-loud && git pull --ff-only'
ssh dgx 'cd ~/src/kastellan && (cargo test --workspace 2>&1; echo "TEST_EXIT=$?"; cargo clippy --workspace --all-targets -- -D warnings 2>&1; echo "CLIPPY_EXIT=$?"; echo DONE) > ~/gate-logs/539.log 2>&1'
ssh dgx 'grep -E "^(test result|TEST_EXIT|CLIPPY_EXIT|DONE)" ~/gate-logs/539.log | tail -20'
```

Expected: `TEST_EXIT=0`, `CLIPPY_EXIT=0`, and the `[SKIP]` count unchanged at 4 (all `KASTELLAN_GLINER_RELEX_ENABLE`) — check with `--nocapture` if in doubt, because a green run with new `[SKIP]` lines is a false green.

- [ ] **Step 4: Push and open the PR**

```sh
git push -u origin fix/539-signal-death-is-loud
gh pr create --title "fix(workers): a signal-killed child is an error, not an empty success (closes #539)" --body-file <path-to-body>
```

The body must carry, in this order: (1) the measured §1.2 table — it is the
finding, and it shows the issue's title understates the defect; (2) what
changed in each of the two workers, naming the type change as the reason the
old code cannot come back; (3) both hosts' gate numbers with the predicted
delta reconciled; (4) what is deliberately **not** in the PR — the seccomp
profile, the jail's env, and the `/usr/bin/python3` allowlist row — each with
its one-line reason from the spec's §2.

---

## Follow-through after merge (controller, not a task)

- **Deploy and live-verify** with `scripts/upgrade_from_git.sh` on the DGX, then re-ask task 148's question (working directory + user) and one `ls -l` question. The assertion that matters is that the audit row shows the step **failed** with `OPERATION_FAILED`, not that the wording reads well — the wording is expected to be re-tuned from daily use.
- **Update #539** with the §1.2 table before closing: `ls -l`, `id` and `whoami` die exactly as python3 does, so the issue's title understates it, and `python3 -S` works — which is why the allowlist row was left alone.
- **HANDOVER.md + ROADMAP.md** at session end.

---

## Self-review notes

- **Spec coverage:** §3.1 → Task 1; §3.2 → Task 1 (message + budget test); §3.3 → Tasks 3 and 4; §3.4 → Task 2; §4 → the test steps of Tasks 1, 3, 4; §5 → Task 5 + Follow-through. §2's out-of-scope items appear in no task, which is correct.
- **Type consistency:** `ChildEnd`, `SignalDeath::from_signal`, `classify`, `signal_death_message(death, what, stdout_len, stderr_len)` and `outcome_to_rpc` are spelled identically in every task that references them.
- **Known risk left to the implementer:** Task 3's `program` borrow may need a `.clone()`; the step says so rather than pretending the borrow checker will be silent.
