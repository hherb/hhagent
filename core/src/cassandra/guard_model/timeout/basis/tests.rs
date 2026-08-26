//! Unit tests for how a guard timeout reports its own provenance
//! (wiring-spec D9, issue #615).
//!
//! Pure throughout. Lifted out of `timeout/tests.rs` unchanged when
//! `timeout.rs` was split, so that each production file carries the
//! tests that name it. These reach the parent for
//! `validate_operator_timeout`, which is only the constructor they go
//! through — their subject is the `PinBand` and the `coverage_finding`
//! it earns.

use std::time::Duration;

use super::super::*;

// ── the operator override ───────────────────────────────────────────

/// A pinned timeout is honoured verbatim and reports itself as the
/// operator's, not as a measurement.
#[test]
fn an_operator_pinned_timeout_is_taken_verbatim() {
    let t = validate_operator_timeout(45_000).expect("a positive value is usable");
    assert_eq!(t.timeout, Duration::from_millis(45_000));
    assert_eq!(t.basis, TimeoutBasis::Operator { band: PinBand::InBand });
    assert_eq!(t.basis.kind(), "operator", "an in-band pin keeps the historic token");
    assert!(
        t.basis.coverage_finding().is_none(),
        "an operator's own IN-BAND number is not a finding about anything"
    );
}

// ── #615: an out-of-band pin is honoured, and reported ──────────────

/// The band boundaries, both **inclusive**.
///
/// A pin exactly at the floor or exactly at the ceiling is a value
/// `derive_guard_timeout` would itself produce, so it must not read as a
/// finding. Walking one step either side of both bounds is what pins the
/// comparison operators: `<` vs `<=` on either end changes exactly one
/// of these four rows, and each error is silent in production.
#[test]
fn classify_pin_puts_the_boundaries_inside_the_band() {
    assert_eq!(classify_pin(TIMEOUT_FLOOR_MS - 1), PinBand::BelowFloor);
    assert_eq!(classify_pin(TIMEOUT_FLOOR_MS), PinBand::InBand);
    assert_eq!(classify_pin(TIMEOUT_CEILING_MS), PinBand::InBand);
    assert_eq!(classify_pin(TIMEOUT_CEILING_MS + 1), PinBand::AboveCeiling);
    // The two extremes an operator can actually reach: 1 ms (accepted,
    // because the refusal is for values that cannot work) and #612's
    // recommended ~350 s for a Metal host.
    assert_eq!(classify_pin(1), PinBand::BelowFloor);
    assert_eq!(classify_pin(350_000), PinBand::AboveCeiling);
}

/// A pin below the floor is applied AND says it weakens the tier.
///
/// The direction that matters: a shorter timeout does not error, it
/// fails OPEN. This is the arm #615 was filed for — the value is still
/// honoured, so the assertion on `t.timeout` is as load-bearing as the
/// one on the finding.
#[test]
fn a_pin_below_the_floor_is_honoured_and_reported() {
    let t = validate_operator_timeout(TIMEOUT_FLOOR_MS - 1).expect("usable");
    assert_eq!(
        t.timeout,
        Duration::from_millis(TIMEOUT_FLOOR_MS - 1),
        "still not clamped -- reporting is not overriding"
    );
    assert_eq!(t.basis.kind(), "operator-below-floor");
    let finding = t.basis.coverage_finding().expect("below the floor is a finding");
    // The phrase crosses TWO of the literal's four `\`-continuation
    // joins (it spans three source lines). A continuation that loses its
    // trailing space welds two words together, and
    // `every_coverage_finding_reads_as_prose` cannot see that -- a double
    // space it can, a missing one it cannot. Pinning a sentence that
    // crosses joins is what covers the other half, for the joins it
    // crosses: the literal's first and last remain unpinned, which is the
    // honest scope rather than the flattering one.
    assert!(
        finding.contains(
            "an adjudication that runs out of budget does not error -- it fails OPEN \
             to catalogue-only screening"
        ),
        "the finding must name the failure direction, unwelded: {finding}"
    );
}

/// A pin above the ceiling is applied AND says what it costs.
///
/// Reachable by following this project's own advice: #612's mitigation
/// for a Metal host is ~350 s, roughly 3x `TIMEOUT_CEILING_MS`. The
/// finding is not a rebuke — it records a deliberate trade.
#[test]
fn a_pin_above_the_ceiling_is_honoured_and_reported() {
    let t = validate_operator_timeout(350_000).expect("usable");
    assert_eq!(t.timeout, Duration::from_millis(350_000), "still not clamped");
    assert_eq!(t.basis.kind(), "operator-above-ceiling");
    let finding = t.basis.coverage_finding().expect("above the ceiling is a finding");
    // Same reason as the below-floor case: a phrase crossing the
    // continuation joins, not a single word.
    assert!(
        finding.contains(
            "The pin is honoured: a single dispatch may now block for the whole \
             pinned budget."
        ),
        "the finding must name what the trade costs, unwelded: {finding}"
    );
}

/// The two findings are different sentences, not one shared one.
///
/// They describe opposite exposures — screening less versus stalling
/// longer — and an operator who reads the wrong one takes the wrong
/// action. `coverage_finding` returns the sentence rather than a `bool`
/// precisely so this can be asserted.
#[test]
fn the_two_pin_findings_are_distinct() {
    let below = TimeoutBasis::Operator { band: PinBand::BelowFloor }
        .coverage_finding()
        .expect("some");
    let above = TimeoutBasis::Operator { band: PinBand::AboveCeiling }
        .coverage_finding()
        .expect("some");
    assert_ne!(below, above);
    // And neither is the ceiling-CLAMP finding, which is about the host's
    // measured throughput rather than about a configured value.
    let clamped = TimeoutBasis::Probed {
        tok_per_s: 100.0,
        derived_ms: 1_000_000,
        clamped: Clamped::ToCeiling,
    }
    .coverage_finding()
    .expect("some");
    assert_ne!(below, clamped);
    assert_ne!(above, clamped);
}

/// No coverage finding carries a doubled space, and every basis that is
/// not a finding stays quiet.
///
/// A `\`-continued Rust string swallows the newline **and** the next
/// line's indentation, so a continuation with two trailing spaces
/// produces a double space in operator-facing text that goes into a
/// durable audit row. #614's review found exactly that in two panic
/// strings, by reading rather than by a failing test.
///
/// **What this does NOT catch: welding.** A continuation that *loses*
/// its trailing space joins two words, and no general assertion can
/// distinguish `adjudicationthat` from a long identifier.
///
/// That half is covered per-finding by asserting a phrase that crosses
/// the continuation joins -- but only for the **two** findings #615 added
/// (`a_pin_below_the_floor_is_honoured_and_reported` and
/// `a_pin_above_the_ceiling_is_honoured_and_reported`), and only for the
/// joins those phrases happen to cross. The three older findings in the
/// array below -- the ceiling clamp, `Saturated` and `Unprobed::Failed`
/// -- have no weld coverage at all. #619's review caught this doc, and
/// the ROADMAP entry fed from it, claiming "pinned per-finding" flatly.
/// Saying so here rather than letting the name imply full coverage.
#[test]
fn every_coverage_finding_reads_as_prose() {
    let bases = [
        TimeoutBasis::Operator { band: PinBand::BelowFloor },
        TimeoutBasis::Operator { band: PinBand::AboveCeiling },
        TimeoutBasis::Probed {
            tok_per_s: 100.0,
            derived_ms: 1_000_000,
            clamped: Clamped::ToCeiling,
        },
        TimeoutBasis::Saturated { budget_ms: PROBE_BUDGET_MS },
        TimeoutBasis::Unprobed { reason: UnprobedReason::Failed },
    ];
    for b in &bases {
        let f = b.coverage_finding().expect("these five are the findings");
        assert!(!f.contains("  "), "collapsed continuation (double space) in: {f}");
        assert!(!f.contains('\n'), "a finding is one line: {f}");
        assert!(f.ends_with('.'), "a finding is a sentence: {f}");
    }
    // And every basis that is NOT a finding stays silent, so the count
    // above is the whole set rather than the ones this test remembered.
    for b in [
        TimeoutBasis::Operator { band: PinBand::InBand },
        TimeoutBasis::Probed { tok_per_s: 5_000.0, derived_ms: 26_000, clamped: Clamped::No },
        TimeoutBasis::Probed { tok_per_s: 9e9, derived_ms: 1, clamped: Clamped::ToFloor },
        TimeoutBasis::Unprobed { reason: UnprobedReason::Nonsensical },
        TimeoutBasis::Unprobed { reason: UnprobedReason::TooFewUncachedTokens },
        TimeoutBasis::Unprobed { reason: UnprobedReason::NoTokenCount },
    ] {
        assert!(b.coverage_finding().is_none(), "routine must stay quiet: {b:?}");
    }
}

/// **Not clamped to the derivation band.**
///
/// The band constrains what this module may *infer*; an operator who
/// pinned a number has already decided, and silently overriding them
/// would make the env var advisory. Both sides of the band are checked
/// so a future "just clamp it for safety" edit fails here.
#[test]
fn an_operator_pinned_timeout_is_not_clamped_to_the_derivation_band() {
    let below = validate_operator_timeout(TIMEOUT_FLOOR_MS - 1).expect("usable");
    assert_eq!(below.timeout, Duration::from_millis(TIMEOUT_FLOOR_MS - 1));
    let above = validate_operator_timeout(TIMEOUT_CEILING_MS + 1).expect("usable");
    assert_eq!(above.timeout, Duration::from_millis(TIMEOUT_CEILING_MS + 1));
    // #615 added the reporting; it must not have quietly added the clamp
    // the paragraph above forbids. Asserting the band as well means a
    // future edit cannot satisfy this test by clamping and relabelling.
    assert_eq!(below.basis, TimeoutBasis::Operator { band: PinBand::BelowFloor });
    assert_eq!(above.basis, TimeoutBasis::Operator { band: PinBand::AboveCeiling });
}

/// Zero is refused — the one value that cannot work.
///
/// No request completes in zero milliseconds, so every adjudication
/// would time out and take the fail-open door: configured, logged as
/// configured, and off. Same silent failure `validate_tau` refuses at
/// both ends of the threshold range, reached through the timeout
/// instead. **1 ms is accepted**, because the refusal is for values
/// that cannot work rather than for values that are unwise — the same
/// line `validate_tau` draws at `f32::MIN_POSITIVE`.
#[test]
fn a_zero_operator_timeout_is_refused_but_one_millisecond_is_not() {
    assert_eq!(validate_operator_timeout(0), Err(TimeoutError::Zero));
    let msg = TimeoutError::Zero.to_string();
    assert!(msg.contains("KASTELLAN_LLM_GUARD_TIMEOUT_MS"), "must name the key: {msg}");
    assert!(msg.contains("OPEN"), "must state the consequence: {msg}");

    assert!(
        validate_operator_timeout(1).is_ok(),
        "the refusal is for the unusable, not for the unwise"
    );
}

/// Every `timeout_basis` token is distinct and log-field shaped.
///
/// These strings go straight into the `policy / guard_tier.boot` row and
/// the boot line. `Unprobed`'s three were covered; `Operator`,
/// `Probed` and `Saturated` were not, and a `Saturated` reported as
/// `"probed"` would spell a budget overrun the same way as a real
/// measurement — which it did until this PR.
#[test]
fn every_timeout_basis_token_is_distinct_and_log_shaped() {
    let bases = [
        TimeoutBasis::Operator { band: PinBand::InBand },
        TimeoutBasis::Operator { band: PinBand::BelowFloor },
        TimeoutBasis::Operator { band: PinBand::AboveCeiling },
        TimeoutBasis::Probed { tok_per_s: 5_000.0, derived_ms: 26_000, clamped: Clamped::No },
        TimeoutBasis::Saturated { budget_ms: PROBE_BUDGET_MS },
        TimeoutBasis::Unprobed { reason: UnprobedReason::Nonsensical },
        TimeoutBasis::Unprobed { reason: UnprobedReason::TooFewUncachedTokens },
        TimeoutBasis::Unprobed { reason: UnprobedReason::NoTokenCount },
        TimeoutBasis::Unprobed { reason: UnprobedReason::Failed },
    ];
    let mut seen = std::collections::BTreeSet::new();
    for b in &bases {
        let k = b.kind();
        assert!(!k.is_empty());
        assert!(!k.chars().any(char::is_whitespace), "not a log token: {k:?}");
        assert!(seen.insert(k), "duplicate timeout_basis token {k:?}");
    }
    assert_eq!(seen.len(), bases.len());
}
