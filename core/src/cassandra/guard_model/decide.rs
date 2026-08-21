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
    /// No probability could be derived. NOT a pass.
    Unmeasured,
}

impl GuardAdjudication {
    /// True only for [`GuardAdjudication::Flagged`].
    ///
    /// The tier is escalate-up only: it may turn an `Allow` into a
    /// `Block` and never the reverse, so this is the single predicate a
    /// wiring site needs. `Unmeasured` deliberately answers `false`
    /// here — fail-open — but the variant survives so the caller can
    /// still audit the distinction.
    pub fn escalates(self) -> bool {
        matches!(self, GuardAdjudication::Flagged)
    }
}

/// Map a probability to an adjudication.
///
/// `p >= tau` flags. Inclusive on purpose: an exactly-at-threshold
/// score is the ambiguous case, and the tier escalates up.
///
/// Pure.
pub fn decide(p: Option<f32>, tau: f32) -> GuardAdjudication {
    match p {
        None => GuardAdjudication::Unmeasured,
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

    #[test]
    fn default_tau_is_the_model_cards_default_and_is_not_a_fitted_threshold() {
        assert_eq!(DEFAULT_TAU, 0.5);
    }

    #[test]
    fn flagged_is_the_only_variant_that_escalates() {
        assert!(GuardAdjudication::Flagged.escalates());
        assert!(!GuardAdjudication::Clear.escalates());
        assert!(!GuardAdjudication::Unmeasured.escalates());
    }
}
