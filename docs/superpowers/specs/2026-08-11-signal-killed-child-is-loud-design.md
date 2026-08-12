# A signal-killed child must be loud — design

**Issue:** [#539](https://github.com/hherb/kastellan/issues/539)
**Date:** 2026-08-11
**Status:** design approved, not yet implemented

---

## 1. The measurement, and how it widens the issue

#539 was filed from one live observation: `/usr/bin/python3` is on the
`shell-exec` allowlist, dies in ~100 ms producing nothing, and `shell.exec`
reports that as a **successful** call —

```json
{"req": {"argv": ["/usr/bin/python3", "-c", "…"]},
 "result": {"stderr": "", "stdout": "", "exit_code": null}}
```

The issue proposed two things: make a null exit code a failure, and decide what
`/usr/bin/python3` is doing on the allowlist at all. Measuring first changed the
second half of that, and widened the first.

### 1.1 The kill is seccomp, and the kernel already says so

The DGX audit log carries one record per death (`journalctl -k`, `type=1326`,
readable unprivileged via the `adm` group — [[dgx-seccomp-syscall-enumeration]]):

```
type=1326 … subj=bwrap pid=2830611 comm="python3" exe="/usr/bin/python3.12"
           sig=31 arch=c00000b7 syscall=198
```

`sig=31` is SIGSYS — a seccomp `KillProcess`, not Landlock and not the OOM
killer. `arch=c00000b7` is aarch64, whose generic syscall table puts **198 =
`socket`**. `WorkerStrict` kills `socket(2)` deliberately; that is the profile
doing exactly its job.

### 1.2 The trap is not python3 — it is every user- or group-name lookup

Reproduced on the DGX with the **shipped** `kastellan-worker-lockdown-exec`
shim, which applies the same lockdown from the same env the daemon derives.
`env -i` reproduces the jail's condition, because `linux_bwrap::build_argv`
passes `--clearenv` and re-adds only `policy.env`:

| command | ambient env | `env -i` (the jail) |
| --- | --- | --- |
| `python3 -c "print(1)"` | rc 0 | **rc 159 (SIGSYS)** |
| `python3 -c "import getpass; print(getpass.getuser())"` | rc 0 | **rc 159** |
| `ls /usr/bin` | rc 0 | rc 0 (25 433 B) |
| `ls -l /usr/bin` | — | **rc 159, 0 B out** |
| `id`, `whoami` | — | **rc 159, 0 B out** |
| `cat /usr/lib/os-release` | — | rc 0 (400 B) |
| `python3` with `HOME=/tmp`, or `python3 -S`, or `-S -E` | — | rc 0 |
| `python3 -I` | — | **rc 159** (`-I` does not disable `site`) |

Three conclusions, none of which the issue could have known:

1. **python3 is not categorically broken.** It dies because `site` expands `~`
   through `getpwuid`, so `-S` or any `HOME` avoids it. `-I` does *not* — it
   ignores env vars without disabling `site`, which is the trap inside the trap.
2. **The class is far broader than python3.** `ls -l`, `id` and `whoami` die the
   same way, with the same empty-success report. glibc NSS opens a socket for
   every user- or group-name resolution; the jail has no `/etc/passwd` and no
   `nscd`, and `socket(2)` is denied. `ls -l` is about the most ordinary command
   there is.
3. **Curating the allowlist therefore cannot fix this.** Removing one row leaves
   the class intact, and the honest members of the class (`ls`, `cat`) differ
   from the fatal ones only by an *argument*.

### 1.3 What is actually broken, in one line

[`workers/shell-exec/src/main.rs:68`](../../../workers/shell-exec/src/main.rs)
serializes `output.status.code()`. On Unix that is `None` **exactly** when the
child was terminated by a signal, and the worker returns it inside an `Ok`
result. Nothing downstream can recover the distinction: the audit row is
indistinguishable from a command that legitimately printed nothing, and
`map_dispatch_result` sees `Ok(value)` so `inner_loop` records a successful
step.

The live cost is not hypothetical. Task 147 concluded *"the tool returned no
output for these commands. Consequently, I cannot determine…"* — a
correct-sounding answer derived from an unreported failure. Task 148 (2026-08-11,
on the #533 build) re-tried four times, eventually adding a `CWD_START` marker
to test whether stdout capture itself was broken, and died on the LLM timeout.

**The same line exists in a second worker.**
[`workers/python-exec/src/exec/mod.rs:399`](../../../workers/python-exec/src/exec/mod.rs)
is `exit_code: status.code()`, and the doc comment 200 lines above it already
names the three causes — "SIGKILL from the cgroup OOM-killer, SIGXCPU past the
rlimit, SIGSYS from seccomp" — then serializes null as a success anyway. A
previous author saw the case and documented it without reporting it.

### 1.4 Why #533 makes this urgent rather than merely wrong

Before #533 the planner guessed at allowlist membership and was refused loudly.
After #533 it reads the advertised set and picks a permitted binary — which on
this host means it reliably chooses `/usr/bin/python3`, the one that silently
does nothing. **A loud refusal was strictly better than a silent no-op**, so
shipping #533 without this makes the agent measurably worse at the exact task
#533 improved.

---

## 2. Scope

**In scope:**

- One pure classifier + message builder in `kastellan-worker-prelude`, used by
  both workers.
- `shell-exec`: signal death → `RpcError(OPERATION_FAILED)`; a `[lib]` target so
  the handler is testable at all.
- `python-exec`: same, plus a type change that makes the silent state
  unrepresentable.
- Tests at both tiers, mutation-checked.
- Updating #539 with the measured table from §1.2, which is a bigger finding
  than the issue's title.

**Out of scope, deliberately:**

- **Widening `WorkerStrict` to permit `socket(2)`.** That filter is the
  containment. The whole point of `Net::Deny` + `WorkerStrict` is that a
  compromised shell-exec cannot open a socket; permitting one so `ls -l` can
  print owner names inverts the threat model for a cosmetic gain.
- **Giving the jail a `HOME`, or a stub `/etc/passwd`.** Measured to fix python3
  (§1.2) and measured *not* to fix `ls -l`, `id` or `whoami`, which reach NSS
  directly. A partial fix that widens the jail's env surface, decided on its own
  evidence rather than smuggled in here.
- **Removing the `/usr/bin/python3` allowlist row.** An operator call, and the
  measurement shows the row is recoverable (`python3 -S`) rather than dead.
- **Core-side changes.** `map_dispatch_result` already maps
  `OPERATION_FAILED` to a named `StepOutcome::Err`, and `inner_loop` already
  feeds a failed step's detail back on the next iteration. Nothing there needs
  to learn about signals.

---

## 3. Design

### 3.1 One pure classifier, shared by both workers

New module `workers/prelude/src/child_exit.rs`:

```rust
/// How a child process ended. `Exited` carries the wait-status code;
/// `Signalled` is the case `ExitStatus::code()` renders as `None`.
pub enum ChildEnd { Exited(i32), Signalled(SignalDeath) }

pub struct SignalDeath { signal: i32 }

impl SignalDeath {
    pub fn signal(&self) -> i32;
    pub fn name(&self) -> &'static str;   // "SIGSYS", "SIGKILL", …
    pub fn cause(&self) -> &'static str;  // the operator/planner-facing gloss
}

pub fn classify(status: ExitStatus) -> ChildEnd;
pub fn signal_death_message(
    death: &SignalDeath, what: &str,
    stdout_len: usize, stderr_len: usize,
) -> String;
```

Both functions are pure — no I/O, no clock, no global state — so both are
testable without spawning anything.

**Why prelude.** It is the one crate both workers already link, and it already
depends on `kastellan-protocol`, so the message builder can derive its length
budget from the shared `STEP_ERR_DETAIL_MAX` rather than a hand-synced copy.
That is the #536 lesson applied before it can bite: a private copy in each
worker would let a core-side clamp change truncate the advice with every test on
both sides still green.

**Why `#[cfg(unix)]` and not `cfg(target_os = "linux")`.** `ExitStatusExt` is a
Unix extension, and Linux + macOS are both first-class here — so a `cfg(unix)`
module compiles, and its tests run, on **both** gate hosts. That is the
`atomic_write` argument (#511) rather than the shape that has repeatedly left
half a guarantee invisible to one host.

**Signal numbers come from `libc`, never from literals.** **Two** numbers in
this table differ between the two first-class platforms (verified against the
`libc` crate's per-target constants: `unix/linux_like/linux/gnu/*` vs
`unix/bsd/mod.rs`, and against the macOS SDK's `sys/signal.h`):

| signal | Linux | macOS |
| --- | --- | --- |
| `SIGSYS` | 31 | **12** |
| `SIGBUS` | 7 | **10** |

Every other signal the table names agrees across both (`SIGXCPU` 24, `SIGSEGV`
11, `SIGKILL` 9, …), which matters for the *test* and not only the prose:
asserting on an agreeing constant cannot distinguish `libc::X` from a
hard-coded literal, so `signal_names_come_from_libc_not_literals` must probe
`SIGSYS` **and** `SIGBUS` — exactly the two above — or it is inert on the
majority of its assertions. Its first cut asserted `SIGSYS`, `SIGXCPU` and
`SIGSEGV`, i.e. one discriminating probe and two that agree with themselves on
both hosts, while omitting the one other divergent constant in the table.

`libc` is already an unconditional dependency of prelude (it is how `rlimit`
works on both platforms), so the table costs nothing regardless. A literal `31`
would name the wrong signal on the Mac and the test would agree with it.

### 3.2 The message is the repair mechanism, so it is budgeted

The planner never sees `RpcError.data` — `map_dispatch_result` keeps only
`code` and `message` — and `summary.rs` clamps the detail to
`STEP_ERR_DETAIL_MAX` (200 chars). So the message is written **diagnosis first,
advice second, variable text last**, exactly as #536 settled: every word of
advice then sits at a fixed offset and the fit stops depending on input length.

Illustrative shape for the dominant case (~176 chars before the command, which
is the part that may be clipped):

```
killed by SIGSYS (sandbox syscall filter): a user/group-name lookup needs
socket(2), which is denied — try `ls` without `-l`, or `python3 -S`.
no output. cmd: `/usr/bin/python3`
```

The `cause()` gloss is a small pure table:

| signal | gloss |
| --- | --- |
| `SIGSYS` | blocked syscall (the sandbox's syscall filter), with the NSS hint |
| `SIGKILL` | the memory cap (cgroup OOM) or an external kill |
| `SIGXCPU` | the CPU budget was exhausted |
| `SIGSEGV`, `SIGBUS`, `SIGILL`, `SIGABRT`, `SIGFPE` | the command crashed |
| `SIGPIPE` | wrote to a closed pipe |
| `SIGTERM`, `SIGINT` | terminated |
| anything else | named by number, with no invented cause |

Captured output is **summarized, not returned** — byte counts only, and the
command is therefore the message's *single* variable-length segment. An earlier
draft appended a tail of stderr, which would have given the message two variable
segments and broken the fixed-offset property the whole ordering rule exists to
provide.

**The output is genuinely lost on both consumer paths.** `RpcError` has a
`data` field, but it reaches neither: `map_dispatch_result` keeps only `code`
and `message`, and the audit row records `"err": e.to_string()`
(`tool_host/post_process.rs`), which renders no payload. Stuffing the captured
bytes into `data` would look thorough and write to nothing — recorded here so a
later reviewer does not "improve" the design in that direction without changing
one of the two consumers first.

**Correction to an earlier draft of this section,** which claimed there is
"nowhere to hide it" at all: there *is* a third channel, just not a
planner-facing one. `spawn_worker` installs `worker_stderr::spawn_drain` on
every worker, so anything a worker writes to its **own** stderr is surfaced at
`debug` in the daemon log. A bounded tail of the discarded child output could
be logged there without touching the message, the planner contract, or the
fixed-offset property. Deliberately not done in this change — it is an operator
-facing diagnostic, not a repair mechanism, and it deserves its own decision
about byte caps and secret scrubbing (`RpcError.message` is not scrubbed today,
and neither would that log line be). Stated so the option is known rather than
foreclosed by a sentence that was wrong.

That loss is the honest cost of choosing "always an error", and it is small in
practice: the realistic signal deaths (seccomp `KillProcess`, cgroup OOM,
`SIGXCPU`) kill the child *without* letting it print, so the case that loses
anything is a command that printed progressively and was then killed. The
alternative — success when output is non-empty — restores a path where a
failure reads as success, which is the shape #539 exists to remove.

### 3.3 The silent state becomes unrepresentable, not merely discouraged

`shell-exec`'s handler matches on `ChildEnd` and has no arm that can put a null
into a result. `python-exec`'s `ExecOutcome.exit_code` changes `Option<i32>` →
`i32`, and `run_code` returns the signal case through a different arm, so the
struct can no longer hold the state that was being serialized. The truncation
flags stay on the success path untouched.

### 3.4 `shell-exec` gains a `[lib]` target

The crate is bin-only today: the handler lives in `main.rs`, so **no test
anywhere in the tree can call it**. It gets a `[lib]` with the handler in
`src/handler.rs` and a thin `main.rs`, copying python-exec's existing shape.

Per the `boot_supervisor` precedent, **the move is its own commit** with no
behaviour change, so the movement diff is reviewable on its own and the test
count is verifiable before and after.

### 3.5 What does not change

The `POLICY_DENIED` allowlist path; the success path's JSON shape (an `Ok`
result had a non-null `exit_code` before and still does); python-exec's
documented contract that a Python **exception** is a nonzero `exit_code` plus a
traceback, not an RPC error. Only signal deaths move.

---

## 4. Tests (written first)

**prelude — hermetic, no spawn, both hosts:**

- `a_normal_exit_classifies_as_exited` / `a_signal_death_classifies_as_signalled`
  — built with `ExitStatus::from_raw`, so no child is needed.
- `signal_names_come_from_libc_not_literals` — asserts `SIGSYS`/`SIGXCPU` map to
  their names on the host running the test, which is the only way the
  Linux/macOS numbering difference can be caught.
- `an_unknown_signal_is_named_by_number_and_invents_no_cause`.
- `the_advice_survives_the_clamp_for_the_longest_command_it_can_quote` — derives
  its worst case from `STEP_ERR_DETAIL_MAX` itself, and asserts the *index* of
  the advice is below the index of the variable text, so it holds regardless of
  prose length. (A budget-only assertion passes with the arms in either order —
  #536 paid for that lesson twice.)

**shell-exec — real child, both hosts:**

- `a_signal_killed_child_is_an_error_not_an_empty_success` — allowlists
  `/bin/sh`, runs `kill -9 $$`, asserts `OPERATION_FAILED` and that the message
  names the signal. Exit **behaviour** only; never output text, because a test
  that drives a shell is portability-sensitive to coreutils output format
  ([[shell-out-tests-coreutils-format-skew]]).
- `a_normal_exit_still_returns_stdout_and_the_code` — the unchanged path.
- `a_non_allowlisted_argv0_is_still_policy_denied` — the guard that the new arm
  did not disturb the old one.

**python-exec:**

- `a_signal_killed_interpreter_is_an_error` — a script that SIGKILLs itself.
- The existing exception-is-not-an-error test must still pass unchanged; if it
  needs editing, the contract moved further than intended.

**Mutation checks (run, not assumed).** Restore `status.code()` in each worker
and confirm **exactly** the new tests fail — the #533 lesson that a test can be
named for a property it never actually evaluates.

---

## 5. Verification

**Gates, both hosts.** No `cfg(target_os)` code in this diff, but there is
`cfg(unix)` code, which means both hosts genuinely run it. Predict the test
delta before running and reconcile any mismatch rather than accepting it.

**Live, on the DGX, after merge and deploy.** The A/B is cheap because the old
behaviour is on record:

1. Re-ask task 148's question (working directory + user), which steered the
   planner to `/usr/bin/python3`. Before: `exit_code: null`, empty output, four
   iterations of self-doubt, LLM timeout. After: a `OPERATION_FAILED` step whose
   detail names SIGSYS and the lookup cause — and the audit row must show the
   step as **failed**, which is the assertion that matters.
2. Ask for a detailed listing (`ls -l`), the more ordinary member of the class,
   and confirm the same.

**Stated honestly:** the hermetic tests carry the weight. A post-deploy run
shows the bell rings in production; it cannot show the class is covered.

---

## 6. Notes for the implementer

- `ExitStatus::from_raw` takes a **wait(2) status word**, not a signal number.
  For a signal-killed child the low 7 bits are the signal, so
  `from_raw(libc::SIGKILL)` yields `code() == None`, `signal() == Some(9)`.
  Do not pass `128 + signal`.
- `/bin/sh` exists on both first-class platforms; `kill -9 $$` is POSIX. Do not
  reach for `timeout`, `setsid` or GNU-only flags.
- The message builder takes a `what` label rather than an `argv0`, because
  python-exec has no argv0 to quote — one function, two call sites, no
  per-worker prose to drift.
- Quote the variable text **last** in every arm, including the `SIGKILL` and
  unknown-signal arms. The temptation is to lead with the command because it
  reads better; that is precisely the ordering #536 had to fix twice.
