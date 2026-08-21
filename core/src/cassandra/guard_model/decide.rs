//! The pure verdict mapping: a probability (or its absence) and a
//! threshold become an adjudication.

/// Mistral's documented default threshold. **Not a fitted value** — see
/// D9 in the slice-1 spec. Measurement 3's calibration set does not
/// exist yet, so any threshold in this codebase today is provisional
/// and must not be promoted to a production default.
pub const DEFAULT_TAU: f32 = 0.5;

/// What the guard model concluded.
///
/// Three-valued on purpose. `Unmeasured` is not a score and is not a
/// pass: [`kastellan_llm_router::logprob_score::binary_token_probability`]
/// returns `None` unless BOTH verdict spellings appear among the
/// alternatives, and collapsing that into a number is the fail-open
/// defect the Rust port exists to make unrepresentable — a sentinel
/// floor renormalises to exactly 0.5 with neither spelling present,
/// which reads as "below tau", i.e. safe.
///
/// Deciding that `Unmeasured` should be allowed is a security decision
/// and belongs at the wiring site, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardAdjudication {
    /// The model judged the document unsafe at or above `tau`.
    Flagged,
    /// The model judged the document safe, below `tau`.
    Clear,
    /// No usable probability. NOT a pass.
    ///
    /// Covers both doors: no verdict pair came back (`None`), and a
    /// score came back that is not a number (non-finite). See
    /// [`decide`].
    Unmeasured,
}

// NOTE: there is deliberately no `escalates() -> bool` helper here, and
// adding one back would undo this module's whole point. Such a method
// performs the three-into-two collapse *inside* the adjudicator and
// then reads, at a call site, as the one predicate a wiring site needs
// — which it is not. D4 requires `Unmeasured` to be Allowed **and
// audited**, and a caller consuming only a `bool` structurally cannot
// audit a distinction it has already erased. Wiring sites must `match`
// all three variants, so the compiler forces the Unmeasured branch to
// be written and therefore reviewed.

/// Map a probability to an adjudication.
///
/// `p >= tau` flags. Inclusive on purpose: an exactly-at-threshold
/// score is the ambiguous case, and the tier escalates up.
///
/// **A non-finite `p` is `Unmeasured`, not `Clear`.** `NaN >= tau` is
/// `false`, so without the explicit guard a `NaN` would fall through
/// the flag arm into `Clear` — the same fail-open this enum exists to
/// make unrepresentable, reached through the `Some` door instead of the
/// `None` door. It is not reachable from the wire today (`serde_json`
/// rejects an out-of-range float with `NumberOutOfRange` rather than
/// decoding it to an infinity, so `binary_token_probability` cannot
/// produce a `NaN`), but "unreachable" is a property of a dependency's
/// parser, and this is a security control.
///
/// Pure.
pub fn decide(p: Option<f32>, tau: f32) -> GuardAdjudication {
    match p {
        None => GuardAdjudication::Unmeasured,
        Some(p) if !p.is_finite() => GuardAdjudication::Unmeasured,
        Some(p) if p >= tau => GuardAdjudication::Flagged,
        Some(_) => GuardAdjudication::Clear,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_is_unmeasured_and_never_clear() {
        assert_eq!(decide(None, DEFAULT_TAU), GuardAdjudication::Unmeasured);
        assert_eq!(decide(None, 0.0), GuardAdjudication::Unmeasured);
        assert_eq!(decide(None, 1.0), GuardAdjudication::Unmeasured);
    }

    /// Table-driven, including the boundary. `p == tau` must FLAG:
    /// the comparison is `>=`, so an exactly-at-threshold score
    /// escalates rather than passing. A mutation to `>` must fail here.
    #[test]
    fn probability_is_compared_to_tau_inclusively() {
        let cases: &[(f32, f32, GuardAdjudication)] = &[
            (0.00, 0.50, GuardAdjudication::Clear),
            (0.49, 0.50, GuardAdjudication::Clear),
            (0.50, 0.50, GuardAdjudication::Flagged), // boundary: >= flags
            (0.51, 0.50, GuardAdjudication::Flagged),
            (1.00, 0.50, GuardAdjudication::Flagged),
            (0.70, 0.90, GuardAdjudication::Clear),
            (0.90, 0.90, GuardAdjudication::Flagged),
        ];
        for (p, tau, want) in cases {
            assert_eq!(decide(Some(*p), *tau), *want, "p={p} tau={tau}");
        }
    }

    /// A non-finite score must take the `Unmeasured` door, not fall
    /// through the `>=` guard into `Clear`. Named for the door it must
    /// NOT take: `NaN >= tau` is false at every `tau`, so the naive
    /// three-arm match sends it to `Clear` — a fail-open reached
    /// through `Some`.
    #[test]
    fn a_non_finite_score_is_unmeasured_and_never_clear() {
        for p in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for tau in [0.0, DEFAULT_TAU, 1.0] {
                assert_eq!(
                    decide(Some(p), tau),
                    GuardAdjudication::Unmeasured,
                    "p={p} tau={tau} must be Unmeasured, not a verdict"
                );
            }
        }
    }

    /// Pins the VALUE only. That `DEFAULT_TAU` is not a fitted
    /// threshold is a fact about how it was chosen and is not
    /// assertable; it lives in the const's doc comment and in D9, not
    /// in this test's name.
    #[test]
    fn default_tau_is_the_model_cards_default() {
        assert_eq!(DEFAULT_TAU, 0.5);
    }
}
