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

use crate::guard_calibration::corpus::{Label, Provenance};
use crate::guard_calibration::report::{confusion_at, Confusion, NoTau, ScoredCase};

/// Which benign cases the false-positive budget is counted over.
///
/// **D7 bounds the budget across the *captured-benign strata*, not
/// across every benign case**, and the difference is not cosmetic.
/// Hand-written benign cases are ones somebody thought of; captured
/// ones are what production actually reads. Letting a synthetic case
/// consume the budget raises τ above what the pre-registered criterion
/// permits, so the tier flags less than intended — and it does so
/// invisibly, because the reported matrix still looks reasonable.
///
/// Made an explicit parameter rather than hard-coded so the divergence
/// cannot be reintroduced by a caller who did not read D7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetScope {
    /// Every benign case counts. Useful for a single-stratum corpus and
    /// for tests; **not** D7's criterion.
    AllBenign,
    /// Only benign cases of this provenance count — D7 uses
    /// [`Provenance::Captured`].
    OnlyProvenance(Provenance),
}

impl BudgetScope {
    fn counts(&self, case: &ScoredCase) -> bool {
        match self {
            BudgetScope::AllBenign => true,
            BudgetScope::OnlyProvenance(p) => case.provenance == *p,
        }
    }
}

/// A threshold and what it costs on the corpus it was fitted to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OperatingPoint {
    pub tau: f32,
    /// The full-corpus matrix at `tau`.
    pub confusion: Confusion,
    /// False positives counted **within the budget scope** — the number
    /// the budget actually bounds. Equal to `confusion.false_positive`
    /// only under [`BudgetScope::AllBenign`]. Carried separately
    /// because a report showing only the full-corpus count would let a
    /// reader check the budget against the wrong number.
    pub scoped_false_positives: u32,
    /// `true` when the chosen `tau` sits above every observed score, so
    /// the tier flags nothing at all.
    ///
    /// **Carried as a flag because the number alone cannot say it.**
    /// The sentinel is one ULP above the maximum observed score, which
    /// renders identically to it at any sane precision — a report
    /// printing `tau=0.900` for both a threshold that flags the top case
    /// and one that flags nothing would be actively misleading.
    pub above_all_observed: bool,
}

/// The smallest `f32` strictly greater than `x`.
///
/// `f32::next_up()` is the obvious call and was the first version of
/// this — but it stabilised in Rust **1.86** and this workspace's MSRV
/// is **1.78**, so clippy's `incompatible_msrv` rejected it. Caught by
/// the lint, not by a test, which is the lint earning its keep.
///
/// For a finite non-negative float the IEEE-754 bit pattern increases
/// monotonically with the value, so incrementing it steps to the next
/// representable float. Every candidate here is exactly that: the loop
/// above refuses non-finite scores, and a probability is non-negative.
fn next_above(x: f32) -> f32 {
    debug_assert!(
        x.is_finite() && x >= 0.0,
        "candidates are finite non-negative probabilities, got {x}"
    );
    f32::from_bits(x.to_bits() + 1)
}

/// False positives at `tau`, counted only over cases the budget applies
/// to.
fn scoped_false_positives(cases: &[ScoredCase], tau: f32, scope: BudgetScope) -> u32 {
    cases
        .iter()
        .filter(|c| c.is_adjudicated() && c.label == Label::Benign && scope.counts(c))
        .filter(|c| match c.probability {
            Some(p) if p.is_finite() => p >= tau,
            _ => false,
        })
        .count() as u32
}

/// The threshold maximising true positives subject to at most
/// `max_false_positives` **within `scope`**.
///
/// **Candidates are the observed scores plus one sentinel above them
/// all.** [`crate::cassandra::guard_model::decide`] compares
/// `p >= tau`, so setting τ to an observed score is exactly the
/// threshold at which that case starts to flag. That reaches every
/// *non-empty* flagged set — but not the empty one, which needs τ
/// strictly greater than the maximum score. The sentinel supplies it.
///
/// Omitting it was a real defect, not a theoretical gap. Without the
/// sentinel a corpus of `benign 0.90, benign 0.50, benign 0.50,
/// attack 0.40` at budget 1 returns τ=0.90 with **TP 0, FP 1** — a
/// threshold that catches nothing and still spends the expensive
/// error — because the free τ that also catches nothing was
/// unreachable. And `benign 0.90, benign 0.90, attack 0.80` at
/// budget 1 returned `Err` outright while a feasible point existed.
///
/// **Ordering: more true positives wins; on equal recall, fewer scoped
/// false positives wins.** There is deliberately no third tie-break,
/// because a third tie is unreachable — see the comment on the
/// comparison. An earlier draft had one, guarded by a test that turned
/// out to be tautological.
///
/// Short-circuits on an unmeasured or non-finite probability for the
/// same reason [`super::report::best_tau`] does: fitting a threshold
/// while skipping such a case fits it over a silently smaller
/// population than the report describes.
///
/// Pure.
pub fn operating_point(
    cases: &[ScoredCase],
    max_false_positives: u32,
    scope: BudgetScope,
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

    // The flags-nothing sentinel: one ULP above the largest observed
    // score, so `p >= tau` is false for every case. Always within any
    // budget, which is what makes the `Err(Overlap)` arm below
    // genuinely unreachable rather than merely believed to be.
    let sentinel = candidates
        .last()
        .copied()
        .map(next_above)
        .expect("both classes were observed, so candidates is non-empty");
    candidates.push(sentinel);

    let mut best: Option<OperatingPoint> = None;
    for &tau in &candidates {
        let confusion = confusion_at(cases, tau);
        let scoped = scoped_false_positives(cases, tau, scope);
        if scoped > max_false_positives {
            continue;
        }
        // Maximise recall, then minimise cost. `Reverse` on the scoped
        // false positives makes "fewer is better" part of the tuple
        // order, so the whole comparison is one expression.
        //
        // **No third tie-break, because a third tie cannot happen —
        // and the load-bearing word is ADJUDICATED, not "observed".**
        // Between two adjacent deduped candidates there is at least one
        // *adjudicated* case sitting at the lower one, flagged there and
        // not above it, which moves TP or FP. That argument depends on
        // this loop's `is_adjudicated` filter matching the one inside
        // `confusion_at`; drop it here alone and two candidates could
        // differ only by excluded cases, making a third tie reachable.
        let key = (confusion.true_positive, std::cmp::Reverse(scoped));
        let better = match &best {
            None => true,
            Some(b) => key > (b.confusion.true_positive, std::cmp::Reverse(b.scoped_false_positives)),
        };
        if better {
            best = Some(OperatingPoint {
                tau,
                confusion,
                scoped_false_positives: scoped,
                above_all_observed: tau == sentinel,
            });
        }
    }

    // Unreachable: the sentinel flags nothing, so its scoped count is 0
    // and it is admissible under every budget. Kept as a handled `Err`
    // rather than an `expect` because this is a security control's
    // calibration path, where a returned error beats a panic.
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
        let got = operating_point(&cases, 0, BudgetScope::AllBenign).expect("separable");
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
        let got = operating_point(&cases, 1, BudgetScope::AllBenign).expect("budget of 1 FP suffices");
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
        let got = operating_point(&cases, 0, BudgetScope::AllBenign).expect("tau above 0.85 catches a2 only");
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
        let got = operating_point(&cases, 2, BudgetScope::AllBenign).expect("budget admits every candidate");
        assert_eq!(got.confusion.true_positive, 1, "recall is equal at every tau");
        assert_eq!(got.confusion.false_positive, 0, "must pick the cheapest");
        assert_eq!(got.tau, 0.80);
    }

    /// The equal-recall tie-break, against the REAL function and with
    /// duplicate scores present.
    ///
    /// This replaces a test that could not fail: it asserted a property
    /// of the candidate set by calling `confusion_at` with three
    /// hand-typed tau values and never calling `operating_point` at
    /// all, so no mutation anywhere in this module could break it. That
    /// is the same tautological-test defect the previous round removed,
    /// reintroduced one layer along — found by a reviewer, not by me.
    ///
    /// Here tau=0.10 gives (TP 2, FP 3) and tau=0.60 gives (TP 2, FP 1):
    /// equal recall, so only the false-positive count separates them,
    /// and the budget of 3 admits both so the choice is made by
    /// preference rather than by the bound.
    #[test]
    fn equal_recall_prefers_the_cheaper_threshold_with_duplicate_scores() {
        let cases = vec![
            case("b1", Label::Benign, 0.10),
            case("b2", Label::Benign, 0.10), // duplicate score
            case("b3", Label::Benign, 0.60),
            case("a1", Label::Attack, 0.60), // same score, other class
            case("a2", Label::Attack, 0.90),
        ];
        let got = operating_point(&cases, 3, BudgetScope::AllBenign).expect("feasible");
        assert_eq!(got.tau, 0.60, "must prefer the cheaper of the two TP=2 thresholds");
        assert_eq!(got.confusion.true_positive, 2);
        assert_eq!(got.scoped_false_positives, 1);
    }

    /// REGRESSION, found by review. Without a candidate above every
    /// observed score the empty flagged-set is unreachable, so this
    /// corpus returned `Err(Overlap)` even though a feasible point
    /// existed.
    #[test]
    fn a_corpus_with_no_affordable_detection_still_yields_a_flags_nothing_point() {
        let cases = vec![
            case("b1", Label::Benign, 0.90),
            case("b2", Label::Benign, 0.90),
            case("a1", Label::Attack, 0.80),
        ];
        let got = operating_point(&cases, 1, BudgetScope::AllBenign)
            .expect("the flags-nothing point is always affordable");
        assert_eq!(got.confusion.true_positive, 0);
        assert_eq!(got.scoped_false_positives, 0);
        assert!(got.above_all_observed, "must be flagged as flagging nothing");
    }

    /// REGRESSION, found by review. The function must not spend a false
    /// positive for zero detection when a free threshold with the same
    /// (zero) recall exists. Its own stated ordering says the cheaper
    /// point wins; before the sentinel that point was unreachable.
    #[test]
    fn a_useless_threshold_is_never_preferred_over_a_free_one() {
        let cases = vec![
            case("b1", Label::Benign, 0.90),
            case("b2", Label::Benign, 0.50),
            case("b3", Label::Benign, 0.50),
            case("a1", Label::Attack, 0.40),
        ];
        let got = operating_point(&cases, 1, BudgetScope::AllBenign).expect("feasible");
        assert_eq!(got.confusion.true_positive, 0, "no attack is affordably catchable");
        assert_eq!(
            got.scoped_false_positives, 0,
            "must NOT pay a false positive for zero detection"
        );
        assert!(got.above_all_observed);
    }

    /// D7 bounds the budget over the CAPTURED-benign strata. A
    /// hand-written benign case must not consume it — doing so raises
    /// tau above what the criterion permits, so the tier flags less
    /// than intended and the matrix still looks reasonable.
    #[test]
    fn only_the_scoped_provenance_consumes_the_budget() {
        let mut hand = case("h1", Label::Benign, 0.85);
        hand.provenance = Provenance::HandWritten;
        let cases = vec![
            hand,
            case("b1", Label::Benign, 0.10),
            case("a1", Label::Attack, 0.80),
        ];
        // AllBenign: the hand-written 0.85 counts, so tau=0.80 costs 1.
        let all = operating_point(&cases, 0, BudgetScope::AllBenign).expect("feasible");
        assert_eq!(all.confusion.true_positive, 0, "budget 0 forbids catching a1");

        // Captured-only: the hand-written case is outside the scope, so
        // tau=0.80 is free and the attack IS caught.
        let scoped =
            operating_point(&cases, 0, BudgetScope::OnlyProvenance(Provenance::Captured))
                .expect("feasible");
        assert_eq!(scoped.tau, 0.80);
        assert_eq!(scoped.confusion.true_positive, 1, "the attack is now affordable");
        assert_eq!(scoped.scoped_false_positives, 0);
        assert_eq!(
            scoped.confusion.false_positive, 1,
            "the full-corpus count still sees the out-of-scope FP"
        );
    }

    /// The benign half of the single-class door. Covering only the
    /// attack side let a mutation to this arm alone survive.
    #[test]
    fn a_benign_only_corpus_has_no_operating_point() {
        let cases = vec![
            case("b1", Label::Benign, 0.10),
            case("b2", Label::Benign, 0.20),
        ];
        assert!(matches!(
            operating_point(&cases, 0, BudgetScope::AllBenign),
            Err(NoTau::SingleClass(Label::Benign))
        ));
    }

    /// Both infinities take the unmeasured door too, matching `decide`'s
    /// own table. The original test pinned NaN only.
    #[test]
    fn both_infinities_are_unmeasured_not_just_nan() {
        for bad in [f32::INFINITY, f32::NEG_INFINITY] {
            let cases = vec![
                case("b1", Label::Benign, 0.10),
                case("a1", Label::Attack, bad),
            ];
            assert!(
                matches!(
                    operating_point(&cases, 0, BudgetScope::AllBenign),
                    Err(NoTau::Unmeasured)
                ),
                "p={bad} must be Unmeasured"
            );
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
        assert!(matches!(
            operating_point(&cases, 0, BudgetScope::AllBenign),
            Err(NoTau::Unmeasured)
        ));
    }

    /// A NaN takes the same door as `None`, matching `decide` and
    /// `best_tau`. Left to ordinary comparison it would never win a
    /// maximisation and so would vanish rather than be refused.
    #[test]
    fn a_non_finite_probability_is_unmeasured() {
        let cases = vec![
            case("b1", Label::Benign, 0.10),
            case("a1", Label::Attack, f32::NAN),
        ];
        assert!(matches!(
            operating_point(&cases, 0, BudgetScope::AllBenign),
            Err(NoTau::Unmeasured)
        ));
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
            operating_point(&cases, 0, BudgetScope::AllBenign),
            Err(NoTau::SingleClass(Label::Attack))
        ));
    }

    /// Cases the catalogue already blocks are excluded from the fit,
    /// consistent with `is_adjudicated`. A corpus of only such cases
    /// has nothing to fit -- and must not read as a perfect score.
    #[test]
    fn a_corpus_the_catalogue_already_blocks_is_empty_not_perfect() {
        let mut c = case("a1", Label::Attack, 0.90);
        c.catalogue_score = 1.0; // >= BLOCK_THRESHOLD
        assert!(matches!(
            operating_point(&[c], 0, BudgetScope::AllBenign),
            Err(NoTau::Empty)
        ));
    }

    /// The MSRV-safe stand-in for `f32::next_up` must genuinely step by
    /// one representable value -- not merely return something larger,
    /// which a naive `x + EPSILON` would fail to do for large `x`.
    #[test]
    fn next_above_steps_to_the_next_representable_float() {
        for x in [0.0f32, 0.5, 0.9, 1.0] {
            let up = next_above(x);
            assert!(up > x, "next_above({x}) = {up} must be strictly greater");
            assert_eq!(
                f32::from_bits(up.to_bits() - 1),
                x,
                "nothing representable may sit between {x} and {up}"
            );
        }
    }
}
