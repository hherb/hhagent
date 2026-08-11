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
                "seccomp (Linux) blocked a syscall — a user/group-name lookup needs \
                 socket(2), which the sandbox denies; try `ls` without `-l`, or \
                 `python3 -S`"
            }
            s if s == libc::SIGKILL => {
                "the memory cap (cgroup OOM on Linux) or an external kill"
            }
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
        // that is neither WIFEXITED nor WIFSIGNALED — a *stopped* child. `wait(2)`
        // can report that shape in general (with `WUNTRACED`), but every `wait`
        // this codebase calls omits it, so in practice the fallback is defensive
        // totality rather than a reachable path; 0 falls through to the unnamed
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
///
/// `stdout_len`/`stderr_len` are as captured by the caller, not necessarily
/// the child's true output size: shell-exec passes the raw, uncapped
/// `output.stdout.len()` / `output.stderr.len()`, while python-exec passes the
/// post-truncation length already capped at its own 256 KiB ceiling — the
/// truncation flags themselves are discarded before reaching this function.
/// A count here can therefore be a cap rather than a measurement, depending
/// on the caller.
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

    // `classify`'s `unwrap_or(0)` fallback exists for a status word that is
    // neither `WIFEXITED` nor `WIFSIGNALED` — a *stopped* child, the shape
    // `wait(2)` can report in general (with `WUNTRACED`), though no `wait`
    // this codebase calls requests it. `from_raw(0x7f)` is that encoding
    // (low byte 0x7f, stop signal 0): verified by hand on this host that
    // `ExitStatus::from_raw(0x7f)` gives `code() == None` *and*
    // `signal() == None`, which is exactly the shape the fallback is for.
    // Without this test, changing `unwrap_or(0)` to `unwrap_or(9)` or to
    // `.unwrap()` passes the rest of the suite untouched.
    #[test]
    fn a_stopped_status_falls_through_to_signal_zero_not_a_fabricated_signal() {
        let end = classify(ExitStatus::from_raw(0x7f));
        match end {
            ChildEnd::Signalled(d) => {
                assert_eq!(d.signal(), 0);
                assert_eq!(d.name(), None);
                let msg = signal_death_message(&d, "/usr/bin/thing", 0, 0);
                assert!(msg.contains("signal 0"), "msg: {msg}");
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
        assert_eq!(d.cause(), "terminated by a signal");
        let msg = signal_death_message(&d, "/usr/bin/thing", 0, 0);
        assert!(msg.contains("signal 64"), "msg: {msg}");
    }

    // Every other test above passes `stdout_len = stderr_len = 0`, so nothing
    // catches the two counts being transposed or one of them being dropped.
    // Two distinct, non-zero values pin each number to its own stream label.
    #[test]
    fn stream_byte_counts_are_reported_against_their_own_stream() {
        let d = SignalDeath::from_signal(libc::SIGKILL);
        let msg = signal_death_message(&d, "/usr/bin/thing", 7, 42);
        assert!(msg.contains("7 B out"), "msg: {msg}");
        assert!(msg.contains("42 B err"), "msg: {msg}");
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
        // The byte-count segment is itself variable-width, so probe with the
        // realistic maximum rather than 0 — python-exec's own captured-stream
        // cap, which is the largest either count can be in practice. Mirrored
        // as a literal (not a dependency on the python-exec crate, which would
        // be a new edge just for a test constant): see
        // `workers/python-exec/src/exec/mod.rs::MAX_CAPTURE_BYTES` (256 KiB).
        const MAX_REALISTIC_CAPTURE_LEN: usize = 256 * 1024;
        let longest = "/".repeat(kastellan_protocol::STEP_ERR_DETAIL_MAX * 4);
        // Every signal `name()`/`cause()` name individually, plus 64 for the
        // unknown-signal fallback arm — the whole table, so the assertion below
        // actually holds for "every signal in the table" (the doc comment's
        // claim) rather than a hand-picked subset of it.
        for sig in [
            libc::SIGSYS,
            libc::SIGKILL,
            libc::SIGXCPU,
            libc::SIGSEGV,
            libc::SIGBUS,
            libc::SIGILL,
            libc::SIGABRT,
            libc::SIGFPE,
            libc::SIGPIPE,
            libc::SIGTERM,
            libc::SIGINT,
            64,
        ] {
            let d = SignalDeath::from_signal(sig);
            let msg = signal_death_message(
                &d,
                &longest,
                MAX_REALISTIC_CAPTURE_LEN,
                MAX_REALISTIC_CAPTURE_LEN,
            );
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
