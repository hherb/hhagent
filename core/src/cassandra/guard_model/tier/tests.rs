//! Unit tests for the guard tier's pure half (wiring-spec D1/D4).
//!
//! No server, no Postgres. The arm logic and the threshold validation
//! are where this tier's security decisions actually live, so they are
//! pinned here exhaustively; the boot sequence that wires them together
//! needs a backend and is covered by `core/tests/guard_tier_e2e.rs`.

use super::*;
use crate::cassandra::injection_guard::BLOCK_THRESHOLD;

// ── consults_model: the catalogue short-circuit ─────────────────────

/// The catalogue's Block short-circuits the model, at and either side
/// of the threshold.
///
/// The boundary case is the one a `>` / `>=` mutation flips. A
/// catalogue score of exactly `BLOCK_THRESHOLD` Blocks, so the model
/// must NOT be consulted — asking it there would let a model that says
/// "clear" appear to disagree with a decision that has already been
/// made, and the tier is escalate-up only.
#[test]
fn the_model_is_consulted_below_the_threshold_and_never_at_or_above_it() {
    assert!(consults_model(0.0), "a clean document goes to the model");
    assert!(consults_model(BLOCK_THRESHOLD - 0.01));
    assert!(
        !consults_model(BLOCK_THRESHOLD),
        "exactly at the threshold the catalogue already Blocked"
    );
    assert!(!consults_model(BLOCK_THRESHOLD + 0.01));
    assert!(!consults_model(1.0));
}

/// `consults_model` must agree with the catalogue's own decision at
/// every score, not merely at the ones someone thought to test.
///
/// This is the guard against the two drifting: if `decision_for_score`
/// ever moves and this predicate keeps its own copy of the comparison,
/// the tier would consult the model on a document the catalogue had
/// already withheld.
#[test]
fn consults_model_agrees_with_the_catalogue_at_every_score() {
    for step in 0..=100 {
        let score = step as f32 / 100.0;
        let catalogue_allowed =
            matches!(decision_for_score(score), InjectionDecision::Allow);
        assert_eq!(
            consults_model(score),
            catalogue_allowed,
            "the tier and the catalogue disagree at score {score}"
        );
    }
}

// ── resolve: the four doors ─────────────────────────────────────────

#[test]
fn a_flagged_adjudication_blocks() {
    assert_eq!(
        resolve(GuardReading::Adjudicated(GuardAdjudication::Flagged)),
        GuardOutcome::Block
    );
}

#[test]
fn a_clear_adjudication_allows() {
    assert_eq!(
        resolve(GuardReading::Adjudicated(GuardAdjudication::Clear)),
        GuardOutcome::Allow
    );
}

/// `Unmeasured` allows, but it must NOT arrive as a plain `Allow`.
///
/// Both pass the document through; only one of them claims the model
/// cleared it. Collapsing the two makes a silently dead tier — the
/// endpoint up but returning no verdict pair — indistinguishable from a
/// working one in the audit log, which is the whole reason
/// `Unadjudicated` is an enum rather than a bool.
#[test]
fn an_unmeasured_adjudication_allows_but_is_never_reported_as_clear() {
    let out = resolve(GuardReading::Adjudicated(GuardAdjudication::Unmeasured));
    assert_eq!(out, GuardOutcome::AllowUnadjudicated { reason: Unadjudicated::Unmeasured });
    assert!(!out.blocks(), "Unmeasured passes the document through");
    assert_ne!(out, GuardOutcome::Allow, "it must not claim the model cleared it");
    assert_ne!(out.as_str(), GuardOutcome::Allow.as_str());
}

/// A failed call fails OPEN — the escalate-up-only property.
///
/// This is the door issue #604's HTTP 400 and issue #586's timeout both
/// arrive through. Fail-closed here would let anyone who can serve the
/// agent a web page deny it every document by padding one.
#[test]
fn a_failed_call_allows_and_is_recorded_as_a_router_error() {
    let out = resolve(GuardReading::Failed);
    assert_eq!(out, GuardOutcome::AllowUnadjudicated { reason: Unadjudicated::RouterError });
    assert!(!out.blocks(), "the tier is escalate-up only; a failure cannot withhold");
}

#[test]
fn an_unconfigured_tier_allows_and_says_so() {
    let out = resolve(GuardReading::NotConfigured);
    assert_eq!(out, GuardOutcome::AllowUnadjudicated { reason: Unadjudicated::NotConfigured });
    assert!(!out.blocks());
}

/// **Only `Flagged` may block.** Stated as a property over every
/// reading rather than as five separate assertions, because this is the
/// invariant the whole tier rests on: a mutation that made any other
/// door block would be a fail-closed reachable by an attacker.
#[test]
fn flagged_is_the_only_reading_that_can_withhold_a_document() {
    let readings = [
        GuardReading::NotConfigured,
        GuardReading::Failed,
        GuardReading::Adjudicated(GuardAdjudication::Clear),
        GuardReading::Adjudicated(GuardAdjudication::Unmeasured),
        GuardReading::Adjudicated(GuardAdjudication::Flagged),
    ];
    for reading in readings {
        let blocks = resolve(reading).blocks();
        let is_flagged =
            matches!(reading, GuardReading::Adjudicated(GuardAdjudication::Flagged));
        assert_eq!(blocks, is_flagged, "{reading:?} must block iff it is Flagged");
    }
}

/// Every state token is distinct and log-field shaped, so the three
/// fail-open doors are countable in the audit log.
#[test]
fn every_guard_state_token_is_distinct_and_log_shaped() {
    let outcomes = [
        GuardOutcome::Block,
        GuardOutcome::Allow,
        GuardOutcome::AllowUnadjudicated { reason: Unadjudicated::NotConfigured },
        GuardOutcome::AllowUnadjudicated { reason: Unadjudicated::Unmeasured },
        GuardOutcome::AllowUnadjudicated { reason: Unadjudicated::RouterError },
    ];
    let mut seen = std::collections::BTreeSet::new();
    for o in outcomes {
        let s = o.as_str();
        assert!(!s.is_empty());
        assert!(!s.chars().any(char::is_whitespace), "not a log token: {s:?}");
        assert!(seen.insert(s), "duplicate guard.state token {s:?} -- the doors stop being countable");
    }
    assert_eq!(seen.len(), 5);
}

// ── validate_tau: both ends are silent failures ─────────────────────

/// Measurement 3's fitted value is accepted, unchanged.
#[test]
fn the_fitted_tau_is_accepted_and_returned_verbatim() {
    let fitted = 0.795_526_56_f32;
    assert_eq!(validate_tau(fitted), Ok(fitted));
}

/// Zero and below are refused: `p >= 0.0` holds for every probability,
/// so the tier would withhold every document the catalogue allowed.
#[test]
fn a_non_positive_tau_is_refused_because_it_blocks_everything() {
    for tau in [0.0_f32, -0.0, -0.1, -1.0, f32::MIN] {
        assert_eq!(
            validate_tau(tau),
            Err(TauError::NotPositive(tau)),
            "tau={tau} would block every document"
        );
    }
}

/// Above 1.0 is refused: no probability can reach it, so the tier looks
/// configured and is off.
#[test]
fn a_tau_above_one_is_refused_because_it_never_flags() {
    for tau in [1.000_001_f32, 1.5, 2.0, f32::MAX] {
        assert_eq!(validate_tau(tau), Err(TauError::AboveOne(tau)), "tau={tau} never flags");
    }
}

/// Exactly 1.0 is accepted — extreme, but `p == 1.0` occurs and would
/// still flag. The boundary a `>=` mutation on the upper bound breaks.
#[test]
fn a_tau_of_exactly_one_is_accepted() {
    assert_eq!(validate_tau(1.0), Ok(1.0));
}

/// The smallest positive float is accepted — it is a terrible
/// threshold, but it is a *representable* one, and the refusal is for
/// values that cannot work at all rather than for values that are
/// unwise.
#[test]
fn the_smallest_positive_tau_is_accepted() {
    assert_eq!(validate_tau(f32::MIN_POSITIVE), Ok(f32::MIN_POSITIVE));
}

/// Non-finite is checked FIRST.
///
/// `NaN <= 0.0` and `NaN > 1.0` are both false, so a NaN falls straight
/// through the range arms into acceptance unless the finiteness test
/// comes before them — the same fail-open shape `decide` guards on the
/// `p` side. Asserted by variant, not just by is-error: a NaN reported
/// as `NotPositive` would send an operator to the wrong fix.
#[test]
fn a_non_finite_tau_is_refused_before_the_range_arms_are_reached() {
    for tau in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        match validate_tau(tau) {
            Err(TauError::NotFinite(_)) => {}
            other => panic!("tau={tau} must be NotFinite, got {other:?}"),
        }
    }
}

/// Every refusal names the env var and the fitted value, because the
/// operator reading it has to know what to set instead.
#[test]
fn every_tau_refusal_is_actionable() {
    let errs = [
        validate_tau(0.0).unwrap_err(),
        validate_tau(2.0).unwrap_err(),
        validate_tau(f32::NAN).unwrap_err(),
    ];
    for e in errs {
        let msg = e.to_string();
        assert!(msg.contains("KASTELLAN_LLM_GUARD_TAU"), "must name the key: {msg}");
        assert!(msg.contains("0.79552656"), "must name the fitted value: {msg}");
    }
}
