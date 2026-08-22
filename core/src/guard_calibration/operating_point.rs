//! D7's τ criterion: an operating point rather than a separating line.
//!
//! [`super::report::best_tau`] answers "where do the classes
//! separate?" and returns [`NoTau::Overlap`] when they do not. That is
//! the right question for the 24-case proof-of-concept corpus, whose
//! classes separate precisely because the cases were hand-picked. It is
//! the wrong question for measurement 3's corpus, where real captured
//! documents will overlap somewhere — and an `Err` there is not a
//! finding, it is a harness that has run out of things to say.
//!
//! So this module answers a different question: **what is the best
//! threshold available while paying at most N false positives?**
//!
//! The asymmetry behind that framing is spec D7's, and it is not the
//! one a high-risk setting first suggests. A false negative is not a
//! regression: the tier is escalate-up only and fails open, so a missed
//! attack leaves exactly today's catalogue-only behaviour. A false
//! positive is a live capability loss — a document withheld from the
//! planner, most often the security and technical prose the agent reads
//! most.
//!
//! Reported *alongside* `best_tau`, never instead of it: on a corpus
//! that does separate, the margin-maximising answer is the better one.

use crate::guard_calibration::corpus::Label;
use crate::guard_calibration::report::{confusion_at, Confusion, NoTau, ScoredCase};

/// A threshold and what it costs on the corpus it was fitted to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OperatingPoint {
    pub tau: f32,
    pub confusion: Confusion,
}

/// The threshold maximising true positives subject to at most
/// `max_false_positives`.
///
/// **Candidates are the observed probabilities themselves.** [`crate::
/// cassandra::guard_model::decide`] compares `p >= tau`, so setting τ to
/// an observed `p` is exactly the threshold at which that case starts to
/// flag. Every distinct behaviour of the classifier on this corpus is
/// reachable from that set, and no value strictly between two adjacent
/// observations behaves differently from the upper one.
///
/// **Ordering: more true positives wins; on equal recall, fewer false
/// positives wins.** There is deliberately no third tie-break, because
/// a third tie is unreachable — see the comment on the comparison
/// itself. An earlier draft had one, guarded by a test that turned out
/// to be tautological: mutation-testing the tie-break direction changed
/// nothing, because the test's corpus contained no tie.
///
/// Short-circuits on an unmeasured or non-finite probability for the
/// same reason [`super::report::best_tau`] does: fitting a threshold
/// while skipping such a case fits it over a silently smaller
/// population than the report describes. Left to ordinary `f32`
/// comparison a `NaN` would simply never win a maximisation, and so
/// would vanish rather than being refused.
///
/// Pure.
pub fn operating_point(
    cases: &[ScoredCase],
    max_false_positives: u32,
) -> Result<OperatingPoint, NoTau> {
    let mut candidates: Vec<f32> = Vec::new();
    let mut saw_attack = false;
    let mut saw_benign = false;

    for case in cases.iter().filter(|c| c.is_adjudicated()) {
        let Some(p) = case.probability.filter(|p| p.is_finite()) else {
            return Err(NoTau::Unmeasured);
        };
        match case.label {
            Label::Attack => saw_attack = true,
            Label::Benign => saw_benign = true,
        }
        candidates.push(p);
    }

    match (saw_attack, saw_benign) {
        (false, false) => return Err(NoTau::Empty),
        (true, false) => return Err(NoTau::SingleClass(Label::Attack)),
        (false, true) => return Err(NoTau::SingleClass(Label::Benign)),
        (true, true) => {}
    }

    // `total_cmp` because every candidate is already known finite (the
    // loop above refused anything else), and it avoids a partial-ord
    // unwrap that would be a panic door on a security control's
    // calibration path.
    candidates.sort_by(f32::total_cmp);
    candidates.dedup();

    let mut best: Option<OperatingPoint> = None;
    for &tau in &candidates {
        let confusion = confusion_at(cases, tau);
        if confusion.false_positive > max_false_positives {
            continue;
        }
        // Maximise recall, then minimise cost. `Reverse` on the false
        // positives makes "fewer is better" part of the tuple order, so
        // the whole comparison is one expression rather than a nest of
        // match arms that a reader has to hold in their head.
        //
        // **No third tie-break, because a third tie cannot happen.**
        // Candidates are the observed scores, so raising tau from one
        // candidate to the next un-flags exactly the cases sitting at
        // the lower one — which necessarily changes TP or FP. Two
        // candidates therefore cannot agree on both counts. An earlier
        // draft broke such ties toward the larger tau; mutation-testing
        // showed flipping that comparison changed no test's outcome,
        // which is what dead code looks like when a tautological test
        // is standing guard over it.
        let key = (confusion.true_positive, std::cmp::Reverse(confusion.false_positive));
        let better = match &best {
            None => true,
            Some(b) => {
                key > (
                    b.confusion.true_positive,
                    std::cmp::Reverse(b.confusion.false_positive),
                )
            }
        };
        if better {
            best = Some(OperatingPoint { tau, confusion });
        }
    }

    // Unreachable in practice: the largest candidate flags only the
    // cases scoring exactly it, so some candidate always stays within
    // any budget >= the number of benign cases sharing the top score.
    // Returned rather than `expect`ed because this is a security
    // control's calibration path, where a handled `Err` beats a panic.
    best.ok_or(NoTau::Overlap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard_calibration::corpus::{Label, Provenance};
    use crate::guard_calibration::report::ScoredCase;

    /// Build an adjudicated case (catalogue score 0.0 keeps it below
    /// BLOCK_THRESHOLD, so `is_adjudicated()` is true).
    fn case(id: &str, label: Label, p: f32) -> ScoredCase {
        ScoredCase {
            id: id.to_string(),
            label,
            provenance: Provenance::Captured,
            catalogue_score: 0.0,
            probability: Some(p),
        }
    }

    /// With separable classes and a zero-FP budget, the operating point
    /// must catch every attack and pay nothing.
    #[test]
    fn separable_classes_catch_everything_at_zero_cost() {
        let cases = vec![
            case("b1", Label::Benign, 0.10),
            case("b2", Label::Benign, 0.20),
            case("a1", Label::Attack, 0.80),
            case("a2", Label::Attack, 0.90),
        ];
        let got = operating_point(&cases, 0).expect("separable");
        assert_eq!(got.confusion.true_positive, 2);
        assert_eq!(got.confusion.false_positive, 0);
        assert_eq!(got.confusion.false_negative, 0);
        assert!(got.tau > 0.20 && got.tau <= 0.80, "tau={}", got.tau);
    }

    /// THE POINT OF THIS FUNCTION. `best_tau` returns Err(Overlap)
    /// here; `operating_point` must still produce a usable threshold by
    /// spending its false-positive budget.
    #[test]
    fn overlapping_classes_still_yield_a_threshold() {
        let cases = vec![
            case("b1", Label::Benign, 0.10),
            case("b2", Label::Benign, 0.85), // overlaps the attacks
            case("a1", Label::Attack, 0.80),
            case("a2", Label::Attack, 0.90),
        ];
        assert!(
            crate::guard_calibration::report::best_tau(&cases).is_err(),
            "precondition: this corpus is NOT separable"
        );
        let got = operating_point(&cases, 1).expect("budget of 1 FP suffices");
        assert_eq!(got.confusion.true_positive, 2, "both attacks caught");
        assert_eq!(got.confusion.false_positive, 1, "paid exactly the budget");
    }

    /// The budget is a HARD bound, not a target. With no budget and an
    /// overlap, recall must be sacrificed rather than the bound broken.
    #[test]
    fn a_zero_budget_is_never_exceeded_even_at_the_cost_of_recall() {
        let cases = vec![
            case("b1", Label::Benign, 0.85),
            case("a1", Label::Attack, 0.80), // below the benign: uncatchable for free
            case("a2", Label::Attack, 0.90),
        ];
        let got = operating_point(&cases, 0).expect("tau above 0.85 catches a2 only");
        assert_eq!(got.confusion.false_positive, 0, "budget must NOT be exceeded");
        assert_eq!(got.confusion.true_positive, 1);
        assert_eq!(got.confusion.false_negative, 1);
    }

    /// On EQUAL recall the cheaper threshold wins. This is the branch
    /// that actually discriminates, and it replaces a test that claimed
    /// to pin a third tie-break which mutation-testing proved could
    /// never fire: in that corpus tau=0.80 gave TP=2 and tau=0.90 gave
    /// TP=1, so the recall comparison decided it and the tie-break was
    /// never reached.
    ///
    /// Here every candidate yields TP=1, so only the false-positive
    /// count separates them — and the budget is deliberately loose
    /// enough (2) that all three candidates are admissible, so the
    /// choice is made by preference and not by the bound.
    #[test]
    fn on_equal_recall_the_cheaper_threshold_wins() {
        let cases = vec![
            case("b1", Label::Benign, 0.10),
            case("b2", Label::Benign, 0.20),
            case("a1", Label::Attack, 0.80),
        ];
        let got = operating_point(&cases, 2).expect("budget admits every candidate");
        assert_eq!(got.confusion.true_positive, 1, "recall is equal at every tau");
        assert_eq!(got.confusion.false_positive, 0, "must pick the cheapest");
        assert_eq!(got.tau, 0.80);
    }

    /// Two candidates can never agree on BOTH counts, which is why the
    /// implementation has no third tie-break. Raising tau past a
    /// candidate un-flags the cases sitting at it, so TP or FP must
    /// move. Pinned as a property over a corpus with both classes and
    /// duplicate scores, so a future edit that reintroduces a third
    /// tie-break has to explain this test rather than quietly pass it.
    #[test]
    fn distinct_candidates_never_share_a_confusion_matrix() {
        let cases = vec![
            case("b1", Label::Benign, 0.10),
            case("b2", Label::Benign, 0.10), // duplicate score
            case("b3", Label::Benign, 0.60),
            case("a1", Label::Attack, 0.60), // same score, other class
            case("a2", Label::Attack, 0.90),
        ];
        let mut seen: Vec<(u32, u32)> = Vec::new();
        for tau in [0.10f32, 0.60, 0.90] {
            let c = crate::guard_calibration::report::confusion_at(&cases, tau);
            let key = (c.true_positive, c.false_positive);
            assert!(
                !seen.contains(&key),
                "tau={tau} repeated matrix {key:?}; a third tie-break would be needed"
            );
            seen.push(key);
        }
    }

    /// An unmeasured case must short-circuit exactly as `best_tau`
    /// does: fitting a threshold while ignoring it fits over a silently
    /// smaller population.
    #[test]
    fn an_unmeasured_case_short_circuits() {
        let mut cases = vec![
            case("b1", Label::Benign, 0.10),
            case("a1", Label::Attack, 0.90),
        ];
        cases.push(ScoredCase {
            id: "a2".to_string(),
            label: Label::Attack,
            provenance: Provenance::Captured,
            catalogue_score: 0.0,
            probability: None,
        });
        assert!(matches!(operating_point(&cases, 0), Err(NoTau::Unmeasured)));
    }

    /// A non-finite probability takes the same door as `None`, matching
    /// `decide` and `best_tau`. Left to `f32::max` it would be silently
    /// discarded.
    #[test]
    fn a_non_finite_probability_is_unmeasured() {
        let cases = vec![
            case("b1", Label::Benign, 0.10),
            case("a1", Label::Attack, f32::NAN),
        ];
        assert!(matches!(operating_point(&cases, 0), Err(NoTau::Unmeasured)));
    }

    /// One class is no boundary to fit, and must not be reported as a
    /// perfect score.
    #[test]
    fn a_single_class_corpus_has_no_operating_point() {
        let cases = vec![
            case("a1", Label::Attack, 0.80),
            case("a2", Label::Attack, 0.90),
        ];
        assert!(matches!(
            operating_point(&cases, 0),
            Err(NoTau::SingleClass(Label::Attack))
        ));
    }

    /// Cases the catalogue already blocks are excluded from the fit,
    /// consistent with `is_adjudicated`. A corpus of only such cases
    /// has nothing to fit.
    #[test]
    fn a_corpus_the_catalogue_already_blocks_is_empty_not_perfect() {
        let mut c = case("a1", Label::Attack, 0.90);
        c.catalogue_score = 1.0; // >= BLOCK_THRESHOLD
        assert!(matches!(operating_point(&[c], 0), Err(NoTau::Empty)));
    }
}
