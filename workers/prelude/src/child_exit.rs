//! How a child process ended, and how to say so in one sentence the planner
//! can act on.
//!
//! ## Why this is not `ExitStatus::code()`
//!
//! On Unix `ExitStatus::code()` returns `None` whenever the child did not
//! exit normally — in practice a signal death, since no `wait` in this
//! codebase passes `WUNTRACED` (see [`classify`] for the one other shape
//! `wait(2)` can report in general). Both workers that spawn children used to
//! serialize that `None` straight into a successful result as
//! `"exit_code": null`, which is indistinguishable from a command that
//! legitimately printed nothing — so a seccomp kill, an OOM kill and a
//! CPU-budget kill all read as success (#539). [`ChildEnd`] makes the two
//! cases separate variants, so a caller cannot serialize one as the other by
//! accident.
//!
//! ## Why it lives in the prelude
//!
//! Two reasons, and the second is the load-bearing one.
//!
//! `shell-exec` and `python-exec` both link this crate, so the wording lives
//! in one place rather than being hand-synced between them. This crate also
//! links `kastellan-protocol`, which lets the *ordering test* below assert
//! against the shared [`kastellan_protocol::STEP_ERR_DETAIL_MAX`] instead of
//! a hand-synced copy. Note that [`signal_death_message`] itself does **not**
//! consult that constant — it formats unconditionally, and the budget is
//! enforced by `the_advice_survives_the_clamp_for_the_longest_command_it_can_quote`.
//! A per-worker copy of the constant would let a core-side clamp change
//! silently truncate the advice with every test on both sides still green,
//! which is the trap #536 documented.
//!
//! More fundamentally: **the signals this module glosses are produced by the
//! prelude's own containment layers.** SIGSYS comes from `seccomp_lock`,
//! SIGXCPU from `rlimit::apply_from_env`, SIGKILL from the memory cap the
//! sandbox sets — and the spawned child inherits all of them. [`SignalDeath::cause`]
//! is the human-readable inverse of what this crate enforces, so it is native
//! here rather than merely conveniently placed.
//!
//! ## Cross-platform
//!
//! `#[cfg(unix)]`, which covers **both** first-class platforms, so both gate
//! hosts compile and run this. Signal numbers come from `libc` because they
//! differ between the two: `SIGSYS` is 31 on Linux and 12 on macOS, and
//! `SIGBUS` is 7 on Linux and 10 on macOS.

use std::process::ExitStatus;

/// The substring [`signal_death_message`] emits when the child produced no
/// stdout at all. Anchored on the `". "` that always precedes the byte counts:
/// an unanchored `"0 B out"` also matches `"10 B out"`, and the leak payloads
/// the containment e2e tests guard against (`"CONNECTED\n"`, and a
/// 943718400-byte allocation printed as `"943718400\n"`) are *exactly* 10
/// bytes — so the unanchored form was defeated by the precise race it exists
/// to catch. Prefer [`reports_zero_stdout`] over matching this by hand.
const ZERO_STDOUT_SEGMENT: &str = ". 0 B out,";

/// Does this signal-death message report that the child wrote **nothing** to
/// stdout?
///
/// The containment e2e tests (`python_exec_e2e`, `python_exec_container_e2e`,
/// `python_exec_firecracker_e2e`) use this to prove a signal-killed child was
/// stopped *before* it could print a leak payload — a child that connected
/// and printed before the kill landed is not contained. Exported so those
/// five call sites share one definition with the renderer instead of
/// hand-duplicating the substring, which is what #547 was filed about.
pub fn reports_zero_stdout(msg: &str) -> bool {
    msg.contains(ZERO_STDOUT_SEGMENT)
}

/// Which worker is reporting, which decides what repair advice is honest.
///
/// The same signal means different things to the two callers, and advice the
/// caller cannot act on is worse than none: `inner_loop` feeds a failed step's
/// error straight back to the planner, which then acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// The caller chose a command line (shell-exec). It can respell the argv,
    /// so naming the offending *argument* is actionable.
    Argv,
    /// The caller submitted source to a fixed interpreter (python-exec). It
    /// cannot change the interpreter flags — `python_args()` already pins
    /// `-I -S -B` — so telling it to "try `python3 -S`" is advice it has
    /// already taken.
    Interpreter,
}

/// Byte counts as captured by the caller.
///
/// Named fields because the two counts are the same type and adjacent: with
/// positional `usize`s the ordering was pinned only inside this crate and a
/// transposition at either call site passed the whole suite — while three e2e
/// files use the stdout count as a *containment* assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Captured {
    pub stdout_len: usize,
    pub stderr_len: usize,
}

/// How a child process ended.
///
/// [`classify`] is the only intended producer. Building a variant by hand is
/// possible and sometimes right (tests do it), but writing
/// `ChildEnd::Exited(status.code().unwrap_or(0))` anywhere would reintroduce
/// #539 in a worse form — a signal death reported as an unambiguous exit 0.
///
/// `Exited`'s payload is deliberately `i32` rather than `u8`, matching
/// `ExitStatus::code()`: the only producer is `WEXITSTATUS`, so the value is
/// always `0..=255`, and narrowing would buy a cast at the JSON boundary for
/// no reachable bug.
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

/// What [`SignalDeath::cause`] says for a signal the table does not enumerate.
const UNKNOWN_CAUSE: &str = "terminated by a signal";

impl SignalDeath {
    /// Build one from a raw signal number.
    ///
    /// Any `i32` is accepted, including `0` and negatives: every method here
    /// is total, and an unrecognized value renders as `signal N` rather than
    /// being given a fabricated name or cause. Validation would create
    /// pressure to lie, because [`classify`] must stay total — see its
    /// `unwrap_or(0)` fallback.
    pub fn from_raw(signal: i32) -> Self {
        Self { signal }
    }

    /// The raw signal number, or `0` for the stopped-child status
    /// [`classify`] cannot attribute to any signal.
    pub fn signal(&self) -> i32 {
        self.signal
    }

    /// The single table behind both [`Self::name`] and [`Self::cause`], so a
    /// signal cannot be known to one and unknown to the other. Divergence
    /// between two hand-maintained matches would produce
    /// "killed by SIGHUP (terminated by a signal)" — degraded rather than
    /// wrong, but free to avoid.
    ///
    /// The `SIGSYS` gloss names `socket(2)` deliberately: measured on the DGX,
    /// every user- or group-name lookup in the jail dies this way (glibc NSS
    /// opens a socket to reach nscd/systemd-userdb, `WorkerStrict` denies
    /// `socket(2)`, and the seccomp action is `KILL_PROCESS` rather than
    /// `ERRNO`, so there is no fallback to `/etc/passwd`). That covers
    /// `ls -l`, `id`, `whoami` and a bare `python3` alike. See the spec's §1.2
    /// table. It is hedged as "commonly" because SIGSYS is the kill action for
    /// *every* non-allowlisted syscall, not only this class.
    fn known(&self, caller: Caller) -> Option<(&'static str, &'static str)> {
        const CRASH: &str = "the command crashed";
        Some(match self.signal {
            s if s == libc::SIGSYS => (
                "SIGSYS",
                match caller {
                    Caller::Argv => {
                        "seccomp denied a syscall — a user/group-name lookup \
                         needs socket(2); try `ls` without `-l`, or `python3 -S`"
                    }
                    Caller::Interpreter => {
                        "seccomp denied a syscall — the interpreter already runs \
                         `-I -S -B`; avoid imports that resolve names or open sockets"
                    }
                },
            ),
            s if s == libc::SIGKILL => (
                "SIGKILL",
                "the memory cap (cgroup OOM on Linux) or an external kill",
            ),
            s if s == libc::SIGXCPU => ("SIGXCPU", "the CPU budget was exhausted"),
            s if s == libc::SIGSEGV => ("SIGSEGV", CRASH),
            s if s == libc::SIGBUS => ("SIGBUS", CRASH),
            s if s == libc::SIGILL => ("SIGILL", CRASH),
            s if s == libc::SIGABRT => ("SIGABRT", CRASH),
            s if s == libc::SIGFPE => ("SIGFPE", CRASH),
            s if s == libc::SIGPIPE => ("SIGPIPE", "wrote to a closed pipe"),
            s if s == libc::SIGTERM => ("SIGTERM", "terminated on request"),
            s if s == libc::SIGINT => ("SIGINT", "terminated on request"),
            _ => return None,
        })
    }

    /// The conventional name, or `None` for a signal this table does not
    /// enumerate — in which case callers say "signal N" rather than inventing
    /// a name for it. Independent of [`Caller`]; only the `SIGSYS` *cause*
    /// varies by caller.
    pub fn name(&self) -> Option<&'static str> {
        self.known(Caller::Argv).map(|(n, _)| n)
    }

    /// What that signal most plausibly means for this caller's child, phrased
    /// for a reader that is either the planner or an operator.
    pub fn cause(&self, caller: Caller) -> &'static str {
        self.known(caller).map_or(UNKNOWN_CAUSE, |(_, c)| c)
    }
}

/// Classify a finished child's [`ExitStatus`].
pub fn classify(status: ExitStatus) -> ChildEnd {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => ChildEnd::Exited(code),
        // `signal()` is `Some` whenever `code()` is `None` for a child reaped
        // by a plain `wait(2)`. `unwrap_or(0)` keeps this total rather than
        // panicking on a status word that is neither WIFEXITED nor
        // WIFSIGNALED — a *stopped* child, which `wait(2)` can report in
        // general (with `WUNTRACED`), though no `wait` this codebase calls
        // requests it. So the fallback is defensive totality rather than a
        // reachable path; 0 falls through to the unnamed arm ("signal 0")
        // rather than being mistaken for a real signal.
        None => ChildEnd::Signalled(SignalDeath::from_raw(status.signal().unwrap_or(0))),
    }
}

/// One sentence for a signal-killed child.
///
/// **Ordering is load-bearing.** The planner sees only the RPC error's `code`
/// and a `message` clamped to [`kastellan_protocol::STEP_ERR_DETAIL_MAX`]
/// (applied core-side in `scheduler::inner_loop::summary::render_step_outcome`),
/// so the diagnosis comes first, the advice second, and the caller-supplied
/// `ran` — the only segment with no bound at all — comes last. Anything placed
/// after it can be clipped away entirely.
///
/// Byte counts are reported instead of the captured bytes themselves: an RPC
/// error carries no result, and `RpcError`'s `data` field reaches neither the
/// planner (`scheduler::tool_dispatch::result_mapping::map_dispatch_result`
/// keeps only `code` + `message`) nor the audit row
/// (`tool_host::post_process` records `e.to_string()`, and `RpcError`'s
/// `Display` omits `data`). **The captured bytes are therefore discarded** —
/// see the crate docs of both callers for why that is the deliberate trade and
/// what it costs.
///
/// `ran` is a short, caller-vocabulary label for what was executed. It is not
/// machine-parsed, may be clipped, and is **not secret-scrubbed on this path**
/// (`tool_host::secret_scrub` walks the *result* value only, and nothing
/// scrubs `RpcError.message`) — so never pass agent-supplied arguments here.
/// Both callers pass a value the operator configured: shell-exec its
/// already-allowlisted `argv[0]`, python-exec its env-supplied interpreter
/// path.
///
/// `captured` counts are as measured by the caller and mean subtly different
/// things: shell-exec passes the raw, uncapped `output.stdout.len()`, while
/// python-exec passes the post-truncation length of the lossy-decoded string,
/// already capped at its own 256 KiB ceiling. A count can therefore be a cap
/// rather than a measurement, and lossy decoding inflates (each invalid byte
/// becomes a 3-byte U+FFFD). The *zero* case is exact in both, which is what
/// [`reports_zero_stdout`] depends on.
pub fn signal_death_message(
    death: &SignalDeath,
    caller: Caller,
    ran: &str,
    captured: Captured,
) -> String {
    let label = match death.name() {
        Some(n) => n.to_string(),
        None => format!("signal {}", death.signal()),
    };
    format!(
        "killed by {label} ({cause}). {out} B out, {err} B err. ran: {ran}",
        cause = death.cause(caller),
        out = captured.stdout_len,
        err = captured.stderr_len,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    const NONE_CAPTURED: Captured = Captured {
        stdout_len: 0,
        stderr_len: 0,
    };

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
    // this codebase calls requests it. `from_raw(0x7f)` is that encoding: the
    // low byte 0x7f makes `WIFEXITED` false (`0x7f & 0x7f != 0`) and
    // `WIFSIGNALED` false (`(i8)(0x7f + 1) >> 1` is negative, not > 0), on
    // both glibc and Darwin — so `code()` and `signal()` are BOTH `None`,
    // which is exactly the shape the fallback is for.
    // Without this test, changing `unwrap_or(0)` to `unwrap_or(9)` or to
    // `.unwrap()` passes the rest of the suite untouched.
    #[test]
    fn a_stopped_status_falls_through_to_signal_zero_not_a_fabricated_signal() {
        let end = classify(ExitStatus::from_raw(0x7f));
        match end {
            ChildEnd::Signalled(d) => {
                assert_eq!(d.signal(), 0);
                assert_eq!(d.name(), None);
                let msg =
                    signal_death_message(&d, Caller::Argv, "/usr/bin/thing", NONE_CAPTURED);
                assert!(msg.contains("signal 0"), "msg: {msg}");
            }
            other => panic!("expected Signalled, got {other:?}"),
        }
    }

    // The one test that can catch a hard-coded literal table: these are the
    // ONLY two constants in `known()` that differ between our two gate hosts
    // (SIGSYS 31/12, SIGBUS 7/10). Every other signal in the table agrees
    // across Linux and macOS, so asserting on them cannot distinguish
    // `libc::X` from a literal — SIGBUS was omitted from this test's first
    // cut, leaving 2 of its 3 assertions inert.
    #[test]
    fn signal_names_come_from_libc_not_literals() {
        assert_eq!(SignalDeath::from_raw(libc::SIGSYS).name(), Some("SIGSYS"));
        assert_eq!(SignalDeath::from_raw(libc::SIGBUS).name(), Some("SIGBUS"));
    }

    // `name()` and `cause()` read one table, so they cannot disagree about
    // which signals are known. This pins that property rather than the
    // structure that currently provides it.
    #[test]
    fn name_and_cause_know_exactly_the_same_signals() {
        for sig in 0..=64 {
            let d = SignalDeath::from_raw(sig);
            for caller in [Caller::Argv, Caller::Interpreter] {
                assert_eq!(
                    d.name().is_some(),
                    d.cause(caller) != UNKNOWN_CAUSE,
                    "signal {sig}: name() and cause() disagree for {caller:?}"
                );
            }
        }
    }

    // Without this, swapping the SIGKILL and SIGXCPU arms — the two signals
    // the containment tests actually turn on — survives the whole suite, and
    // an OOM would be reported to the planner as "the CPU budget was
    // exhausted".
    #[test]
    fn each_signal_gets_its_own_cause() {
        let cases = [
            (libc::SIGKILL, "SIGKILL", "memory cap"),
            (libc::SIGXCPU, "SIGXCPU", "CPU budget"),
            (libc::SIGSEGV, "SIGSEGV", "crashed"),
            (libc::SIGBUS, "SIGBUS", "crashed"),
            (libc::SIGPIPE, "SIGPIPE", "closed pipe"),
            (libc::SIGTERM, "SIGTERM", "on request"),
        ];
        for (sig, name, cause_fragment) in cases {
            let d = SignalDeath::from_raw(sig);
            assert_eq!(d.name(), Some(name), "signal {sig}");
            assert!(
                d.cause(Caller::Argv).contains(cause_fragment),
                "signal {sig}: cause {:?} lacks {cause_fragment:?}",
                d.cause(Caller::Argv)
            );
        }
    }

    #[test]
    fn an_unknown_signal_is_named_by_number_and_invents_no_cause() {
        let d = SignalDeath::from_raw(64);
        assert_eq!(d.name(), None);
        assert_eq!(d.cause(Caller::Argv), UNKNOWN_CAUSE);
        let msg = signal_death_message(&d, Caller::Argv, "/usr/bin/thing", NONE_CAPTURED);
        assert!(msg.contains("signal 64"), "msg: {msg}");
    }

    // Every other test here passes zero counts, so nothing catches the two
    // being transposed or one being dropped. Two distinct, non-zero values
    // pin each number to its own stream label.
    #[test]
    fn stream_byte_counts_are_reported_against_their_own_stream() {
        let d = SignalDeath::from_raw(libc::SIGKILL);
        let msg = signal_death_message(
            &d,
            Caller::Argv,
            "/usr/bin/thing",
            Captured {
                stdout_len: 7,
                stderr_len: 42,
            },
        );
        assert!(msg.contains("7 B out"), "msg: {msg}");
        assert!(msg.contains("42 B err"), "msg: {msg}");
    }

    // The core containment e2e tests prove a signal-killed child printed
    // NOTHING via `reports_zero_stdout`. Pin both the segment and — the part
    // that was wrong first time — its ANCHORING: an unanchored
    // `contains("0 B out")` also matches `"10 B out"`, and both leak payloads
    // those tests guard against ("CONNECTED\n", and a 943718400-byte
    // allocation printed as "943718400\n") are exactly 10 bytes, so the
    // unanchored form was defeated by the precise race it exists to catch.
    #[test]
    fn the_zero_stdout_predicate_is_anchored() {
        let d = SignalDeath::from_raw(libc::SIGSYS);
        let zero = signal_death_message(&d, Caller::Argv, "/bin/x", NONE_CAPTURED);
        assert!(reports_zero_stdout(&zero), "zero-output message: {zero}");
        let ten = signal_death_message(
            &d,
            Caller::Argv,
            "/bin/x",
            Captured {
                stdout_len: 10,
                stderr_len: 0,
            },
        );
        assert!(
            !reports_zero_stdout(&ten),
            "a 10-byte capture must not satisfy the zero-output check: {ten}"
        );
    }

    #[test]
    fn a_seccomp_kill_says_so_and_offers_a_repair() {
        let d = SignalDeath::from_raw(libc::SIGSYS);
        let msg = signal_death_message(&d, Caller::Argv, "/usr/bin/python3", NONE_CAPTURED);
        assert!(msg.contains("SIGSYS"), "msg: {msg}");
        // Structure, not prose: the message must offer *something* actionable
        // for the class measured in the spec (§1.2).
        assert!(msg.contains("socket(2)"), "msg: {msg}");
    }

    // A SIGSYS reaching python-exec cannot be the `site`/`getpwuid` case —
    // `python_args()` pins `-I -S -B` unconditionally, so `-S` is ALREADY
    // applied. Telling that caller to "try `python3 -S`" is advice it has
    // taken, and `ls -l` is not a lever it has at all.
    #[test]
    fn the_interpreter_caller_is_not_told_to_retry_with_flags_it_already_uses() {
        let d = SignalDeath::from_raw(libc::SIGSYS);
        let msg = signal_death_message(
            &d,
            Caller::Interpreter,
            "/usr/bin/python3",
            NONE_CAPTURED,
        );
        assert!(!msg.contains("try `python3 -S`"), "msg: {msg}");
        assert!(!msg.contains("`-l`"), "msg: {msg}");
        assert!(msg.contains("seccomp denied a syscall"), "msg: {msg}");
    }

    // The load-bearing one. `ran` is caller-supplied and unbounded; every word
    // of diagnosis and advice must sit BEFORE it, and inside the clamp, for
    // every signal in the table and both callers.
    #[test]
    fn the_advice_survives_the_clamp_for_the_longest_command_it_can_quote() {
        // Probe with counts far past any real capture. python-exec caps its
        // streams at 256 KiB, but shell-exec's `Command::output()` is
        // UNCAPPED — so the earlier "256 KiB is the largest either count can
        // be" premise was false for one of the two callers. 10 digits covers
        // anything `MAX_RECORD_BYTES` could carry, and makes the test
        // independent of either cap.
        const WIDEST_COUNT: usize = 9_999_999_999;
        let longest = "/".repeat(kastellan_protocol::STEP_ERR_DETAIL_MAX * 4);
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
            for caller in [Caller::Argv, Caller::Interpreter] {
                let d = SignalDeath::from_raw(sig);
                let msg = signal_death_message(
                    &d,
                    caller,
                    &longest,
                    Captured {
                        stdout_len: WIDEST_COUNT,
                        stderr_len: WIDEST_COUNT,
                    },
                );
                // Every fixed segment precedes the unbounded one. This single
                // assertion is what makes a REORDER fail: the previous
                // `what_at <= MAX` form still passed with `ran:` moved ahead
                // of the byte counts, because the prefix was short enough
                // either way.
                assert!(
                    msg.ends_with(&longest),
                    "signal {sig} ({caller:?}): something follows the command: {msg}"
                );
                let what_at = msg.find(&longest).expect("the command must be quoted");
                // Everything except the caller's own string fits in the budget.
                assert!(
                    what_at <= kastellan_protocol::STEP_ERR_DETAIL_MAX,
                    "signal {sig} ({caller:?}): advice ends at {what_at}, past the clamp: {msg}"
                );
                // And the diagnosis genuinely precedes it.
                let name_at = msg.find("killed by").expect("diagnosis must be present");
                assert!(
                    name_at < what_at,
                    "signal {sig} ({caller:?}): diagnosis after the command: {msg}"
                );
            }
        }
    }

    // The interpreter path is python-exec's whole M10 guarantee ("an operator
    // reading the error sees WHICH interpreter died"), and it is quoted in the
    // `ran:` segment — the one that gets clipped. At 186 chars the earlier
    // prefix pushed `/usr/bin/python3` to 202 chars, past the 200-char clamp,
    // so the guarantee held only in the audit row. Pin that a realistic
    // interpreter path now survives end to end.
    #[test]
    fn a_realistic_interpreter_path_survives_the_clamp() {
        for path in [
            "/usr/bin/python3",
            "/opt/homebrew/bin/python3.13",
            "/usr/local/bin/python3.12",
        ] {
            let d = SignalDeath::from_raw(libc::SIGSYS);
            let msg =
                signal_death_message(&d, Caller::Interpreter, path, NONE_CAPTURED);
            let clamped: String = msg
                .chars()
                .take(kastellan_protocol::STEP_ERR_DETAIL_MAX)
                .collect();
            assert!(
                clamped.contains(path),
                "the interpreter path must survive the clamp the planner applies: {clamped}"
            );
        }
    }
}
