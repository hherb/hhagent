//! REQUIRE-aware preconditions for micro-VM suites (#679).
//!
//! # The defect this closes
//!
//! #667 gave the micro-VM preflight a knob,
//! [`REQUIRE_ENV`](super::REQUIRE_ENV): set it, and every unmet micro-VM
//! precondition panics naming itself instead of printing `[SKIP]` and
//! reporting green. It covers what `skip_if_no_microvm` itself checks — the
//! Firecracker probe, the launcher, and image freshness.
//!
//! It did not cover the preconditions asked **beside** it:
//!
//! ```ignore
//! if skip_if_no_microvm(VM_ROOTFS) || skip_if_no_supervisor() || skip_if_sandbox_unavailable() {
//!     return;
//! }
//! let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
//! ```
//!
//! `||` short-circuits, so on exactly the host the operator cares about —
//! KVM, vsock, a built launcher, fresh images — control reaches
//! `skip_if_no_supervisor()`, which knows nothing about the knob. Missing
//! `loginctl enable-linger` then produces the same silent skip-as-pass the
//! knob exists to abolish, one helper to the right.
//!
//! ⚠️ **The issue counted one syntactic shape; the property is broader.**
//! Grepping for the `||` chain found 7 call sites. Asking instead "which
//! preconditions inside a micro-VM-gated test bypass the knob" found **11
//! tests across 6 kinds** — the `||` chain, the same three helpers written as
//! sequential `if`s, `pg_bin_dir_or_skip`, `skip_if_origin_unreachable`,
//! `egress_proxy_bin_or_skip`, and four hand-written `eprintln!("[SKIP] …")`
//! broker-binary checks. Hence [`bypassed_gates`], which pins the *property*
//! over the real sources instead of the shape.
//!
//! # The vocabulary
//!
//! Two combinators, matching the two shapes a precondition comes in:
//!
//! * [`skip_unless_ready`] for a `bool` precondition — takes [`Probe`]s, i.e.
//!   the `*_or_reason` siblings that return a reason without rendering a
//!   verdict (#653's pattern, reused rather than re-invented).
//! * [`dep_or_skip`] for one that yields a value (`Result<T, String>`).
//!
//! Both route the reason through [`super::report_unmet_microvm`], so a
//! `[SKIP]` and a REQUIRE panic cannot disagree about what was unmet.

use std::io::Write;

/// A precondition probe: `None` when met, `Some(reason)` when not.
///
/// The `*_or_reason` half of the tree's `*_or_reason` / `skip_if_*` split.
/// Named because a bare `&[&dyn Fn() -> Option<String>]` at a call site does
/// not coerce from a mixed array of closures without an annotation, and the
/// annotation should say what the thing *is*.
pub type Probe<'a> = &'a dyn Fn() -> Option<String>;

/// The first unmet precondition among `probes`, in order, or `None` when all
/// are met.
///
/// Pure over its injected probes, so the ordering and the short-circuit are
/// unit-testable with no host in the loop — the same seam
/// [`super::preflight`] uses, and for the same reason: #680's review found
/// that the impure half of the last such check could be replaced wholesale
/// with nothing failing.
///
/// **Short-circuiting is behaviour, not an optimisation.** These probes spawn
/// processes and open sockets (`default_probe` shells out; an origin probe
/// waits up to 5 s for a TCP connect), so a probe after the first failure
/// must not run: an offline host would otherwise pay seconds to be told
/// something the first probe already established.
pub fn first_unmet(probes: &[Probe]) -> Option<String> {
    probes.iter().find_map(|probe| probe())
}

/// The two host preconditions **every** micro-VM daemon e2e needs, in
/// diagnosis order.
///
/// All 11 sites #679 covers ask for exactly this pair, so naming it once
/// keeps the call sites to one line and stops the pair drifting apart the way
/// the byte-copied `[SKIP]` helpers this module was created to end did.
///
/// The order is a claim about usefulness, not an accident: an absent user
/// supervisor explains a Postgres cluster that will not come up, so reporting
/// it first sends the operator to `loginctl enable-linger` rather than to a
/// database that was never the problem.
pub fn host_probes() -> [Probe<'static>; 2] {
    [&crate::skip::supervisor_unavailable_reason, &crate::sandbox::sandbox_unavailable_reason]
}

/// `[SKIP]` + `true` when any precondition is unmet, or panic when
/// [`super::REQUIRE_ENV`] demanded a real run.
///
/// The REQUIRE-aware replacement for OR-ing `skip_if_*` helpers beside
/// `skip_if_no_microvm`. Returns `true` so the call site stays the one-liner
/// it was:
///
/// ```ignore
/// if skip_if_no_microvm(VM_ROOTFS)
///     || skip_unless_ready(&[&supervisor_unavailable_reason, &sandbox_unavailable_reason])
/// {
///     return;
/// }
/// ```
///
/// # Panics
///
/// Under [`crate::gliner_e2e::UnmetAction::Fail`], naming both the knob and
/// the reason — see [`super::report_unmet_microvm`].
pub fn skip_unless_ready(probes: &[Probe]) -> bool {
    skip_unless_ready_to(probes, &mut std::io::stderr())
}

/// [`skip_unless_ready`] with the `[SKIP]` line written to `out`.
///
/// Exists so a unit test can prove the Skip arm **emits** the line without
/// emitting a real `[SKIP]` into the run it is protecting — asserting on
/// [`crate::skip::skip_line`] alone would leave the write deletable with the
/// suite still green, and `grep -c '^\[SKIP\]'` is how a run is audited here.
///
/// # Panics
///
/// As [`skip_unless_ready`].
pub fn skip_unless_ready_to(probes: &[Probe], out: &mut dyn Write) -> bool {
    match first_unmet(probes) {
        Some(reason) => super::report_unmet_microvm_to(&reason, out),
        None => false,
    }
}

/// Hand a required dependency through, or `[SKIP]` + `None` — or panic when
/// [`super::REQUIRE_ENV`] demanded a real run.
///
/// The value-returning sibling of [`skip_unless_ready`], for the
/// `let ... else { return; }` call sites:
///
/// ```ignore
/// let Some(bin_dir) = dep_or_skip(pg_bin_dir_or_reason()) else { return; };
/// ```
///
/// A missing dependency is as fatal under REQUIRE as a failed probe: both say
/// the run would prove nothing. That one arrives as a `Result` and the other
/// as an `Option<String>` is a shape difference, not a severity difference.
///
/// # Panics
///
/// As [`skip_unless_ready`].
pub fn dep_or_skip<T>(dep: Result<T, String>) -> Option<T> {
    dep_or_skip_to(dep, &mut std::io::stderr())
}

/// [`dep_or_skip`] with the `[SKIP]` line written to `out`, for the same
/// reason [`skip_unless_ready_to`] exists.
///
/// # Panics
///
/// As [`skip_unless_ready`].
pub fn dep_or_skip_to<T>(dep: Result<T, String>, out: &mut dyn Write) -> Option<T> {
    match dep {
        Ok(value) => Some(value),
        Err(reason) => {
            super::report_unmet_microvm_to(&reason, out);
            None
        }
    }
}

// ---------------------------------------------------------------------------
// The source-level guard
// ---------------------------------------------------------------------------

/// Skip helpers that are **not** REQUIRE-aware, and so must not be called
/// from a micro-VM suite.
///
/// Each is shared with ~70 non-micro-VM suites, which is why none of them can
/// be made require-aware in place: the knob is a micro-VM concept and these
/// helpers have no business knowing about it. The resolution is #653's — the
/// helper keeps its `*_or_reason` sibling, and the *call site* decides the
/// verdict. This list is what the call sites may no longer say.
///
/// Kept in one place because [`bypassed_gates`] and its "every entry is
/// actually detected" test both read it: a typo in an entry would otherwise
/// silently stop checking that one helper, which is a guard that guards less
/// than it claims — #667's own shape.
pub const BANNED_HELPERS: &[&str] = &[
    "skip_if_no_supervisor",
    "skip_if_sandbox_unavailable",
    "skip_if_origin_unreachable",
    "pg_bin_dir_or_skip",
    "egress_proxy_bin_or_skip",
];

/// The marker that exempts a hand-written `[SKIP]` from [`bypassed_gates`].
///
/// For an **opt-in enablement flag** — a test whose own env gate is unset was
/// never asked for, so demanding a micro-VM run must not turn it into a
/// failure. That is categorically different from an unmet *host* precondition,
/// which is what the knob is about. The exemption is inline and carries its
/// reason so the distinction is made where it applies, not in a list some
/// other file owns.
pub const EXEMPT_MARKER: &str = "REQUIRE-EXEMPT";

/// How many lines above a `[SKIP]` literal an [`EXEMPT_MARKER`] may sit.
///
/// **Two**, and the number is measured rather than chosen: the idiomatic
/// placement is above the `if` that guards the print, not inside the block —
///
/// ```ignore
/// // REQUIRE-EXEMPT: opt-in enablement flag, not a host precondition.
/// if std::env::var(GATE).is_err() {
///     eprintln!("\n[SKIP] {GATE} unset …");
/// ```
///
/// — which puts exactly one line (the `if`) between the marker and what it
/// excuses. A window of 1 rejected the only real exemption in the tree.
///
/// It stays deliberately small. A marker that has drifted away from its
/// literal is a marker nobody re-reads, and this one is the single escape
/// hatch in the guard: it must stay visibly attached to the thing it excuses.
const EXEMPT_WINDOW: usize = 2;

/// A precondition that reports a green run when [`super::REQUIRE_ENV`]
/// demanded a real one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BypassedGate {
    /// 1-based line within the scanned source.
    pub line: usize,
    /// What was found: a [`BANNED_HELPERS`] name, or `"hand-written [SKIP]"`.
    pub what: String,
}

/// Pure: every precondition in `src` that bypasses [`super::REQUIRE_ENV`].
///
/// Two rules, both textual, because the property is not visible to the type
/// system and not visible to any run — the false green only appears on a host
/// where the micro-VM preconditions are met and a neighbouring one is not,
/// which is by definition not the host anybody gates on.
///
/// 1. **No [`BANNED_HELPERS`] call.** Comment-only lines are exempt: a suite's
///    docs may well name a helper while explaining why it does not use it, and
///    flagging that would make this guard's own remedy undocumentable.
/// 2. **No hand-written `[SKIP]` in a print macro**, unless an
///    [`EXEMPT_MARKER`] sits on that line or the one above.
///
/// Deliberately conservative about what counts as a call: it matches the bare
/// identifier anywhere on a non-comment line, imports included. An import of a
/// banned helper into a micro-VM suite is itself the finding — nothing else
/// would want it there.
pub fn bypassed_gates(src: &str) -> Vec<BypassedGate> {
    let lines: Vec<&str> = src.lines().collect();
    let mut found = Vec::new();

    for (idx, raw) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = raw.trim_start();
        let is_comment = trimmed.starts_with("//");

        if !is_comment {
            for helper in BANNED_HELPERS {
                if contains_identifier(raw, helper) {
                    found.push(BypassedGate { line: line_no, what: (*helper).to_string() });
                }
            }
        }

        if is_print_macro(raw) && raw.contains("[SKIP]") && !exempt_near(&lines, idx) {
            found.push(BypassedGate { line: line_no, what: "hand-written [SKIP]".to_string() });
        }
    }
    found
}

/// Pure: does `line` contain `name` as a whole identifier?
///
/// Substring matching alone would report `skip_if_no_supervisor` inside a
/// hypothetical `skip_if_no_supervisor_reason`, and — the case that actually
/// matters — would flag `pg_bin_dir_or_skip` inside nothing at all while
/// missing that `egress_proxy_bin_or_skip` is a **prefix** of no other name
/// only by luck. Bounding on Rust identifier characters makes the rule stable
/// as names are added.
fn contains_identifier(line: &str, name: &str) -> bool {
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(rel) = line[from..].find(name) {
        let start = from + rel;
        let end = start + name.len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Pure: Rust identifier body character (ASCII is enough — these are all
/// snake_case helper names).
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Pure: is this line a stderr/stdout print macro?
///
/// The `[SKIP]` rule targets a line that *renders* a skip, not one that
/// mentions the string — an assertion such as
/// `assert!(!rendered.contains("[SKIP]"))` must not be a finding.
fn is_print_macro(line: &str) -> bool {
    ["eprintln!", "eprint!", "println!", "print!", "write!", "writeln!"]
        .iter()
        .any(|m| line.contains(m))
}

/// Pure: does an [`EXEMPT_MARKER`] sit on line `idx` or within
/// [`EXEMPT_WINDOW`] lines above it?
fn exempt_near(lines: &[&str], idx: usize) -> bool {
    let first = idx.saturating_sub(EXEMPT_WINDOW);
    lines[first..=idx].iter().any(|l| l.contains(EXEMPT_MARKER))
}
