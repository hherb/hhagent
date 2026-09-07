//! Unit tests for [`super::require`] — the REQUIRE-aware precondition
//! vocabulary (#679) and the source-level guard that keeps the call sites
//! using it.
//!
//! Two halves, and the second is the load-bearing one. The pure combinators
//! are easy to get right; what #667 actually shipped broken was the *wiring*,
//! and #680's review found the wiring could be replaced with `false` with the
//! whole suite still green. So the guard below reads the real
//! `core/tests/*.rs` sources and fails on any precondition that bypasses the
//! knob — including ones nobody has written yet.

use super::require::{
    bypassed_gates, dep_or_skip_to, first_unmet, skip_unless_ready_to, Probe, BANNED_HELPERS,
};
use crate::env::{env_lock, EnvVarGuard};
use crate::microvm::REQUIRE_ENV;

/// A probe that is met (no reason).
fn met() -> Option<String> {
    None
}

// ---------------------------------------------------------------------------
// first_unmet — pure
// ---------------------------------------------------------------------------

/// All met → nothing to report. The case an operator on a good host hits
/// every run, so it must not cost a `[SKIP]`.
#[test]
fn first_unmet_is_none_when_every_probe_is_met() {
    let probes: [Probe; 3] = [&met, &met, &met];
    assert_eq!(first_unmet(&probes), None);
}

/// The empty slice is "nothing to check", not "something is wrong". A caller
/// that has no extra preconditions must be able to pass `&[]` without
/// inventing a dummy probe.
#[test]
fn first_unmet_is_none_for_no_probes() {
    assert_eq!(first_unmet(&[]), None);
}

/// The FIRST unmet reason wins, in the order given. Ordering is the caller's
/// statement of which diagnosis is more useful: "no supervisor" explains a
/// missing Postgres cluster, not the other way round.
#[test]
fn first_unmet_returns_the_first_unmet_reason_in_order() {
    let a = || Some("supervisor unavailable: no bus".to_string());
    let b = || Some("no Postgres install found".to_string());
    let probes: [Probe; 3] = [&met, &a, &b];
    assert_eq!(
        first_unmet(&probes).as_deref(),
        Some("supervisor unavailable: no bus"),
        "the first unmet probe is the reported one"
    );
}

/// Short-circuiting is a behaviour, not an optimisation: these probes SPAWN
/// (`default_probe` shells out; `skip_if_origin_unreachable` opens a TCP
/// connection with a 5 s timeout). A probe after the first failure must not
/// run at all, or an offline host pays five seconds to be told something it
/// already knew.
#[test]
fn first_unmet_does_not_evaluate_probes_past_the_first_failure() {
    use std::cell::Cell;
    let ran_later = Cell::new(false);
    let fails = || Some("bwrap probe failed: no userns".to_string());
    let later = || {
        ran_later.set(true);
        None
    };
    let probes: [Probe; 2] = [&fails, &later];
    let _ = first_unmet(&probes);
    assert!(!ran_later.get(), "probes after the first failure must not be evaluated");
}

// ---------------------------------------------------------------------------
// skip_unless_ready — the Skip arm and the Fail arm
// ---------------------------------------------------------------------------

/// Unset knob: the unmet precondition renders as a real `[SKIP]` line and the
/// caller returns. This is the pre-#679 behaviour, unchanged — a plain
/// `cargo test` on a host with no supervisor must stay green.
#[test]
fn skip_unless_ready_emits_a_skip_line_and_returns_true_when_the_knob_is_unset() {
    let _lock = env_lock();
    let _guard = EnvVarGuard::unset(REQUIRE_ENV);

    let mut sink: Vec<u8> = Vec::new();
    let fails = || Some("supervisor unavailable: no bus".to_string());
    let probes: [Probe; 1] = [&fails];
    let skipped = skip_unless_ready_to(&probes, &mut sink);

    assert!(skipped, "an unmet precondition means the caller returns");
    let rendered = String::from_utf8(sink).expect("utf8");
    assert!(rendered.contains("[SKIP]"), "must emit the auditable line: {rendered:?}");
    assert!(rendered.contains("no bus"), "must carry the reason: {rendered:?}");
}

/// All met: no line at all. A `[SKIP]` line is evidence in this tree
/// (`grep -c '^\[SKIP\]'` audits a run), so a helper that printed on the
/// success path would inflate exactly the count it protects.
#[test]
fn skip_unless_ready_is_silent_and_returns_false_when_every_probe_is_met() {
    let _lock = env_lock();
    let _guard = EnvVarGuard::unset(REQUIRE_ENV);

    let mut sink: Vec<u8> = Vec::new();
    let probes: [Probe; 2] = [&met, &met];
    assert!(!skip_unless_ready_to(&probes, &mut sink));
    assert!(sink.is_empty(), "a met precondition prints nothing: {sink:?}");
}

/// The whole point of #679: with the knob truthy, the precondition the
/// operator did NOT ask about still stops the run. Before this, a host with
/// KVM, vsock, a built launcher and fresh images but no `enable-linger` gave
/// the operator a green run they had explicitly tried to rule out.
#[test]
#[should_panic(expected = "KASTELLAN_MICROVM_REQUIRE_E2E")]
fn skip_unless_ready_panics_naming_the_knob_when_a_real_run_was_demanded() {
    let _lock = env_lock();
    let _guard = EnvVarGuard::set(REQUIRE_ENV, "1");

    let mut sink: Vec<u8> = Vec::new();
    let fails = || Some("supervisor unavailable: no bus".to_string());
    let probes: [Probe; 1] = [&fails];
    let _ = skip_unless_ready_to(&probes, &mut sink);
}

/// ...and the panic must carry the *reason*, not just the knob, or the
/// operator is told their run was refused without being told what to fix.
#[test]
#[should_panic(expected = "no bus")]
fn skip_unless_ready_panic_carries_the_reason() {
    let _lock = env_lock();
    let _guard = EnvVarGuard::set(REQUIRE_ENV, "1");

    let mut sink: Vec<u8> = Vec::new();
    let fails = || Some("supervisor unavailable: no bus".to_string());
    let probes: [Probe; 1] = [&fails];
    let _ = skip_unless_ready_to(&probes, &mut sink);
}

// ---------------------------------------------------------------------------
// dep_or_skip — the value-returning sibling
// ---------------------------------------------------------------------------

/// The happy path hands the value straight through and prints nothing.
#[test]
fn dep_or_skip_passes_the_value_through_silently() {
    let _lock = env_lock();
    let _guard = EnvVarGuard::unset(REQUIRE_ENV);

    let mut sink: Vec<u8> = Vec::new();
    let got: Option<u32> = dep_or_skip_to(Ok(7), &mut sink);
    assert_eq!(got, Some(7));
    assert!(sink.is_empty(), "a met dependency prints nothing: {sink:?}");
}

/// Unset knob: `None` plus the auditable line, so the `let ... else return`
/// call sites keep working exactly as they did.
#[test]
fn dep_or_skip_reports_and_returns_none_when_the_knob_is_unset() {
    let _lock = env_lock();
    let _guard = EnvVarGuard::unset(REQUIRE_ENV);

    let mut sink: Vec<u8> = Vec::new();
    let got: Option<u32> = dep_or_skip_to(Err("no Postgres install found".to_string()), &mut sink);
    assert_eq!(got, None);
    let rendered = String::from_utf8(sink).expect("utf8");
    assert!(rendered.contains("[SKIP]"), "must emit the auditable line: {rendered:?}");
    assert!(rendered.contains("Postgres"), "must carry the reason: {rendered:?}");
}

/// A missing *dependency* is as fatal under REQUIRE as a failed probe. Both
/// are "this run would prove nothing"; that they arrive as `Result` rather
/// than `Option<String>` is a shape difference, not a severity difference.
#[test]
#[should_panic(expected = "KASTELLAN_MICROVM_REQUIRE_E2E")]
fn dep_or_skip_panics_when_a_real_run_was_demanded() {
    let _lock = env_lock();
    let _guard = EnvVarGuard::set(REQUIRE_ENV, "1");

    let mut sink: Vec<u8> = Vec::new();
    let _: Option<u32> = dep_or_skip_to(Err("no Postgres install found".to_string()), &mut sink);
}

// ---------------------------------------------------------------------------
// bypassed_gates — the pure source scanner
// ---------------------------------------------------------------------------

/// A bare call to a non-REQUIRE-aware helper is the defect #679 is about.
#[test]
fn bypassed_gates_flags_a_bare_helper_call() {
    let src = "fn t() {\n    if skip_if_no_supervisor() {\n        return;\n    }\n}\n";
    let found = bypassed_gates(src);
    assert_eq!(found.len(), 1, "one violation expected: {found:?}");
    assert_eq!(found[0].line, 2, "must report the line to fix");
    assert_eq!(found[0].what, "skip_if_no_supervisor");
}

/// The `||`-chained shape the issue was filed about: three helpers on one
/// line are three findings, because the fix is three substitutions.
#[test]
fn bypassed_gates_flags_every_helper_on_one_line() {
    let src = "if skip_if_no_microvm(R) || skip_if_no_supervisor() || skip_if_sandbox_unavailable() {\n";
    let found = bypassed_gates(src);
    let names: Vec<&str> = found.iter().map(|g| g.what.as_str()).collect();
    assert_eq!(names, vec!["skip_if_no_supervisor", "skip_if_sandbox_unavailable"]);
}

/// Every banned helper is actually reachable by the scanner. Without this a
/// typo in one entry of [`BANNED_HELPERS`] would silently stop checking that
/// one — a guard that guards less than it claims, which is #667's own shape.
#[test]
fn bypassed_gates_flags_every_banned_helper() {
    for helper in BANNED_HELPERS {
        let src = format!("    let _ = {helper}();\n");
        let found = bypassed_gates(&src);
        assert_eq!(found.len(), 1, "{helper} must be detected: {found:?}");
        assert_eq!(found[0].what, *helper);
    }
}

/// Prose is not a call. The module docs of a suite may well name a helper
/// while explaining why it does not use it; flagging that would make the
/// guard's own remedy impossible to document.
#[test]
fn bypassed_gates_ignores_a_helper_named_in_a_comment() {
    let src = "// skip_if_no_supervisor() is routed through the knob below\n\
               /// See skip_if_sandbox_unavailable for the bare form.\n";
    assert!(bypassed_gates(src).is_empty(), "a mention in prose is not a call");
}

/// A REQUIRE-aware call site must not trip the guard, or the fix cannot be
/// applied. `supervisor_unavailable_reason` is the sibling that IS routed.
#[test]
fn bypassed_gates_accepts_the_reason_siblings() {
    let src = "if skip_unless_ready(&[&supervisor_unavailable_reason, &sandbox_unavailable_reason]) {\n";
    assert!(bypassed_gates(src).is_empty(), "the fixed shape must pass: {:?}", bypassed_gates(src));
}

/// A hand-written `[SKIP]` is the other half of the class — it is how the
/// broker-binary checks were written, and it bypasses the knob just as
/// completely as a helper call does.
#[test]
fn bypassed_gates_flags_a_hand_written_skip_literal() {
    let src = "    eprintln!(\"\\n[SKIP] search-broker binary not built\\n\");\n";
    let found = bypassed_gates(src);
    assert_eq!(found.len(), 1, "one violation expected: {found:?}");
    assert_eq!(found[0].what, "hand-written [SKIP]");
}

/// ...but an *opt-in enablement flag* is legitimately not a host
/// precondition. A test whose own env gate is unset was never asked for, so
/// demanding a micro-VM run must not turn it into a failure. The exemption is
/// explicit, local, and carries its reason on the same line.
#[test]
fn bypassed_gates_honours_an_inline_exemption_marker() {
    let src = "    // REQUIRE-EXEMPT: opt-in enablement flag, not a host precondition\n\
               \x20   eprintln!(\"\\n[SKIP] {GATE} unset\\n\");\n";
    assert!(bypassed_gates(src).is_empty(), "an exempted literal must pass: {:?}", bypassed_gates(src));
}

/// The real placement: the marker sits above the `if` that guards the print,
/// so one line (the `if`) separates it from the literal. A window that
/// rejected this would reject the only exemption in the tree — measured, not
/// assumed: it did, at a window of 1.
#[test]
fn bypassed_gates_honours_a_marker_above_the_guarding_if() {
    let src = "    // REQUIRE-EXEMPT: opt-in enablement flag, not a host precondition.\n\
               \x20   if std::env::var(GATE).is_err() {\n\
               \x20       eprintln!(\"\\n[SKIP] {GATE} unset\\n\");\n";
    assert!(bypassed_gates(src).is_empty(), "the real shape must pass: {:?}", bypassed_gates(src));
}

/// ...but no further. Three lines up is a marker that has drifted away from
/// what it excuses, and this is the guard's only escape hatch — it must stay
/// visibly attached.
#[test]
fn a_marker_three_lines_up_does_not_exempt() {
    let src = "    // REQUIRE-EXEMPT: drifted\n\
               \x20   let a = 1;\n\
               \x20   let b = 2;\n\
               \x20   eprintln!(\"[SKIP] gate unset\");\n";
    let found = bypassed_gates(src);
    assert_eq!(found.len(), 1, "a drifted marker must not exempt: {found:?}");
    assert_eq!(found[0].line, 4);
}

/// The marker exempts only what it is next to. A second, unmarked literal
/// further down the same file is still a finding — otherwise one exemption
/// would silently disarm the whole file.
#[test]
fn an_exemption_marker_does_not_cover_a_later_literal() {
    let src = "    // REQUIRE-EXEMPT: opt-in enablement flag\n\
               \x20   eprintln!(\"[SKIP] gate unset\");\n\
               \x20   let x = 1;\n\
               \x20   let y = 2;\n\
               \x20   let z = 3;\n\
               \x20   eprintln!(\"[SKIP] egress-proxy not built\");\n";
    let found = bypassed_gates(src);
    assert_eq!(found.len(), 1, "only the unmarked literal is a finding: {found:?}");
    assert_eq!(found[0].line, 6);
}
