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

use crate::cassandra::guard_model::{decide, GuardAdjudication};
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
    /// How this scope reads in a report, so the artefact's statement of
    /// its own scope is DERIVED from the scope actually used rather
    /// than written beside it as an independent literal.
    pub fn as_str(&self) -> &'static str {
        match self {
            BudgetScope::AllBenign => "all benign",
            BudgetScope::OnlyProvenance(Provenance::Captured) => "captured-benign",
            BudgetScope::OnlyProvenance(Provenance::HandWritten) => "hand-written-benign",
            BudgetScope::OnlyProvenance(Provenance::DerivedFromCatalogue) => {
                "catalogue-derived-benign"
            }
        }
    }

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
    /// The scope the budget was counted over, **carried on the result
    /// so a report renders it from the fit rather than from its own
    /// parameter.** Rendering the caller's copy made the "derived, never
    /// written beside it" property true only for as long as there was
    /// exactly one caller passing the same value twice; fitting under
    /// one scope and printing another type-checked.
    pub scope: BudgetScope,
    /// How many benign cases the budget was counted over. Printed beside
    /// the count, because `0 of 1 allowed` over an empty population
    /// looks identical to a real pass.
    pub scope_population: u32,
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
/// representable float. Every candidate is exactly that, and both halves
/// are **enforced** rather than assumed: [`operating_point`]'s candidate
/// loop refuses a score that is not finite *or* is negative, and returns
/// [`NoTau::Unmeasured`] for it.
///
/// The negative half used to be a `debug_assert!` over a producer's
/// property (`binary_token_probability` is a sigmoid). That is the
/// consumer trusting the producer across a `pub` boundary, checked only
/// where the code does not ship: incrementing the bit pattern of a
/// negative float steps *away* from zero, so the sentinel would land
/// below the maximum, flag the cases it exists to exclude, and still
/// report `above_all_observed`.
fn next_above(x: f32) -> f32 {
    debug_assert!(
        x.is_finite() && x >= 0.0,
        "candidates are finite non-negative probabilities, got {x}"
    );
    f32::from_bits(x.to_bits() + 1)
}

/// Benign cases the budget applies to, whatever `tau`.
///
/// **Zero here makes D7's criterion vacuous**, which is why
/// [`operating_point`] refuses rather than reporting a threshold: the
/// budget bounds a population that does not exist, so `scoped > budget`
/// is never true and the fit degenerates to "catch every attack at any
/// benign cost" — while the report still prints `0 of 1 allowed`, which
/// reads as the criterion being honoured. The shipped 24-case corpus
/// has no captured cases at all, so this is the default run, not a
/// corner.
fn scope_population(cases: &[ScoredCase], scope: BudgetScope) -> u32 {
    cases
        .iter()
        .filter(|c| c.is_adjudicated() && c.label == Label::Benign && scope.counts(c))
        .count() as u32
}

/// False positives at `tau`, counted only over cases the budget applies
/// to.
///
/// **Delegates to the shipping [`decide`]** for the same reason
/// [`confusion_at`] does, and it matters more here: this is the count
/// the *budget* is checked against, so an inline `p >= tau` that drifted
/// from the adjudicator would admit a τ violating D7 under the real
/// thing while the report said otherwise.
fn scoped_false_positives(cases: &[ScoredCase], tau: f32, scope: BudgetScope) -> u32 {
    cases
        .iter()
        .filter(|c| c.is_adjudicated() && c.label == Label::Benign && scope.counts(c))
        .filter(|c| matches!(decide(c.probability, tau), GuardAdjudication::Flagged))
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
/// false positives wins; on equal both, fewer false positives overall.**
/// The third key is load-bearing and its absence was a live bug — see
/// the comment on the comparison for the worked counterexample.
/// (This paragraph used to say there was deliberately no third key,
/// "because a third tie is unreachable". That was true before
/// [`BudgetScope`] existed and was left standing when the fix landed one
/// commit later, pointing the reader at an inline comment that by then
/// argued the opposite.)
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
        // `>= 0.0` alongside `is_finite()`, because `next_above` is
        // correct only for non-negative input: incrementing the bit
        // pattern of a negative float steps AWAY from zero, so the
        // "sentinel" would land below the maximum, flag what it was
        // built to exclude, and report `above_all_observed` for a
        // threshold that flags things. It was a `debug_assert!`, which
        // is compiled out exactly where a security control ships.
        let Some(p) = case.probability.filter(|p| p.is_finite() && *p >= 0.0) else {
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

    // Checked AFTER the class check so a single-class corpus still
    // reports the cause an operator can act on. Under `AllBenign` this
    // is unreachable -- `saw_benign` implies a population -- so it fires
    // only where D7's scope actually restricts, which is where it
    // matters.
    let population = scope_population(cases, scope);
    if population == 0 {
        return Err(NoTau::EmptyBudgetScope);
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
        // Maximise recall, then minimise the cost the budget bounds,
        // then minimise cost overall.
        //
        // **The third key is not belt-and-braces — it was a live bug.**
        // An earlier version stopped at `(TP, Reverse(scoped))` and
        // carried a comment arguing a third tie was unreachable. That
        // argument was true when the count was over *every* benign
        // (between adjacent candidates some case un-flags, moving TP or
        // FP) and it silently became FALSE when `BudgetScope` was
        // added: if the case that un-flags is OUT of scope, the scoped
        // count does not move and neither does TP.
        //
        // Worked counterexample, at budget 1: hand-written benign 0.95,
        // captured benigns 0.90 and 0.91, captured attack 0.85. Both
        // τ=0.95 and the sentinel score `(TP 0, scoped 0)`; the first
        // encountered wins, and that is τ=0.95, which carries a
        // full-corpus false positive the sentinel does not. Strictly
        // worse, for free.
        //
        // With `false_positive` as the final key the ordering is total:
        // between two adjacent candidates at least one adjudicated case
        // un-flags, and it is either an attack (TP moves) or a benign
        // (`false_positive` moves), so no two candidates agree on all
        // three.
        //
        // The lesson generalises past this function: adding a filter to
        // one input of a comparison invalidates arguments made about
        // the comparison, and nothing re-checks them for you.
        let key = (
            confusion.true_positive,
            std::cmp::Reverse(scoped),
            std::cmp::Reverse(confusion.false_positive),
        );
        let better = match &best {
            None => true,
            Some(b) => {
                key > (
                    b.confusion.true_positive,
                    std::cmp::Reverse(b.scoped_false_positives),
                    std::cmp::Reverse(b.confusion.false_positive),
                )
            }
        };
        if better {
            best = Some(OperatingPoint {
                tau,
                confusion,
                scoped_false_positives: scoped,
                scope,
                scope_population: population,
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
        case_with(id, label, Provenance::Captured, p)
    }

    /// The same, with the provenance spelled out — needed by anything
    /// that must render or fit under a scope narrower than `AllBenign`.
    fn case_with(id: &str, label: Label, provenance: Provenance, p: f32) -> ScoredCase {
        ScoredCase {
            id: id.to_string(),
            label,
            provenance,
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

    /// REGRESSION, found by review. Introducing [`BudgetScope`] made a
    /// third tie reachable and invalidated the comment claiming it was
    /// not: when the case that un-flags between two adjacent candidates
    /// is OUT of scope, neither the scoped count nor TP moves.
    ///
    /// Here tau=0.95 and the sentinel both score (TP 0, scoped 0), but
    /// tau=0.95 flags the out-of-scope hand-written benign and the
    /// sentinel flags nothing. Without a third key the first candidate
    /// encountered wins -- tau=0.95 -- taking a full-corpus false
    /// positive for no gain whatsoever.
    #[test]
    fn an_out_of_scope_false_positive_is_never_taken_for_free() {
        let mut hand = case("h1", Label::Benign, 0.95);
        hand.provenance = Provenance::HandWritten;
        let cases = vec![
            hand,
            case("b1", Label::Benign, 0.90),
            case("b2", Label::Benign, 0.91),
            case("a1", Label::Attack, 0.85),
        ];
        let got =
            operating_point(&cases, 1, BudgetScope::OnlyProvenance(Provenance::Captured))
                .expect("the sentinel is always affordable");
        assert_eq!(got.confusion.true_positive, 0, "no attack is affordable here");
        assert_eq!(got.scoped_false_positives, 0);
        assert_eq!(
            got.confusion.false_positive, 0,
            "must not flag the out-of-scope benign for zero gain"
        );
        assert!(got.above_all_observed, "the free choice is the sentinel");
    }

    /// The scope's report text is derived from the scope, so an artefact
    /// cannot state one scope while another was used.
    #[test]
    fn every_budget_scope_names_itself() {
        assert_eq!(BudgetScope::AllBenign.as_str(), "all benign");
        assert_eq!(
            BudgetScope::OnlyProvenance(Provenance::Captured).as_str(),
            "captured-benign"
        );
        // PAIRWISE, over ALL FOUR. Checking one pair left the
        // `DerivedFromCatalogue` arm free to return "captured-benign"
        // with the suite green -- which is precisely the property this
        // test's own rationale says must not hold.
        let all = [
            BudgetScope::AllBenign,
            BudgetScope::OnlyProvenance(Provenance::Captured),
            BudgetScope::OnlyProvenance(Provenance::HandWritten),
            BudgetScope::OnlyProvenance(Provenance::DerivedFromCatalogue),
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(
                    a.as_str(),
                    b.as_str(),
                    "two scopes must not share a name, or a report cannot \
                     distinguish them: {a:?} vs {b:?}"
                );
            }
        }
    }

    /// Pins the equivalence that makes the report's TP-vs-sentinel
    /// keying a free choice: a catches-nothing result and a sentinel
    /// win imply each other.
    ///
    /// It holds only because of the third tie-break -- any observed
    /// candidate flags the case at its own score, so `TP == 0` forces
    /// `FP >= 1`, and the sentinel then dominates on the full-corpus
    /// key. Remove that key and this breaks, which is the point of
    /// pinning it here rather than leaving it as a comment.
    ///
    /// **The scoped leg below is what makes that last sentence true.**
    /// Every corpus here once ran under `AllBenign` only -- where the
    /// scoped count is *identically* `confusion.false_positive`, so keys
    /// 2 and 3 hold the same value and deleting key 3 cannot change a
    /// single comparison the test makes. The doc claimed the test pinned
    /// a key it could not reach. Under a narrower scope the two counts
    /// come apart, and the mutation fails here.
    #[test]
    fn catches_nothing_iff_the_sentinel_won() {
        let corpora: Vec<Vec<ScoredCase>> = vec![
            // Separable: catches everything, sentinel does not win.
            vec![
                case("b1", Label::Benign, 0.10),
                case("a1", Label::Attack, 0.90),
            ],
            // Nothing affordable: sentinel wins, TP 0.
            vec![
                case("b1", Label::Benign, 0.90),
                case("b2", Label::Benign, 0.91),
                case("a1", Label::Attack, 0.85),
            ],
            // The third-tie corpus.
            vec![
                case("b1", Label::Benign, 0.90),
                case("b2", Label::Benign, 0.91),
                case("a1", Label::Attack, 0.85),
                case("a2", Label::Attack, 0.99),
            ],
        ];
        for (i, cases) in corpora.iter().enumerate() {
            for budget in 0..=2u32 {
                let got = operating_point(cases, budget, BudgetScope::AllBenign)
                    .expect("the sentinel is always affordable");
                assert_eq!(
                    got.confusion.true_positive == 0,
                    got.above_all_observed,
                    "corpus {i} budget {budget}: TP==0 and above_all_observed must \
                     imply each other, got TP={} above={}",
                    got.confusion.true_positive,
                    got.above_all_observed
                );
            }
        }

        // The scoped leg. At budget 1 the admissible candidates are
        // tau=0.91 (scoped 1, FP 2), tau=0.95 (scoped 0, FP 1) and the
        // sentinel (scoped 0, FP 0). All three catch nothing, so ONLY
        // the third key separates 0.95 from the sentinel -- and with the
        // key deleted 0.95 is encountered first and wins, leaving
        // `TP == 0` with `above_all_observed == false`.
        let mixed = vec![
            case_with("hw1", Label::Benign, Provenance::HandWritten, 0.95),
            case_with("c1", Label::Benign, Provenance::Captured, 0.90),
            case_with("c2", Label::Benign, Provenance::Captured, 0.91),
            case_with("a1", Label::Attack, Provenance::Captured, 0.85),
        ];
        let got = operating_point(&mixed, 1, BudgetScope::OnlyProvenance(Provenance::Captured))
            .expect("the sentinel is always affordable");
        assert_eq!(got.confusion.true_positive, 0, "nothing is affordable here");
        assert!(
            got.above_all_observed,
            "with TP 0 the free sentinel must win on the third key, not a \
             threshold that pays a full-corpus false positive for nothing"
        );
    }

    /// The result states the scope it was fitted under, so a report
    /// cannot render one scope over a fit made under another.
    #[test]
    fn the_fit_carries_its_own_scope_and_population() {
        let cases = vec![
            case_with("hw1", Label::Benign, Provenance::HandWritten, 0.10),
            case_with("c1", Label::Benign, Provenance::Captured, 0.20),
            case_with("a1", Label::Attack, Provenance::Captured, 0.90),
        ];
        let scoped = BudgetScope::OnlyProvenance(Provenance::Captured);
        let got = operating_point(&cases, 0, scoped).expect("separable");
        assert_eq!(got.scope, scoped);
        assert_eq!(got.scope_population, 1, "one captured benign is in scope");

        let all = operating_point(&cases, 0, BudgetScope::AllBenign).expect("separable");
        assert_eq!(all.scope, BudgetScope::AllBenign);
        assert_eq!(all.scope_population, 2, "both benigns are in scope");
    }

    /// **A budget over an empty population is not a criterion.**
    ///
    /// With no benign case in scope the budget never binds, so the fit
    /// degenerates to "catch every attack at any benign cost" -- and the
    /// report would print `0 of 1 allowed`, which reads as the criterion
    /// being honoured. The shipped 24-case corpus has NO captured cases,
    /// so this is what a default `guard calibrate` run does.
    #[test]
    fn an_empty_budget_scope_is_refused_rather_than_fitted_vacuously() {
        let cases = vec![
            case_with("hw1", Label::Benign, Provenance::HandWritten, 0.80),
            case_with("hw2", Label::Benign, Provenance::HandWritten, 0.81),
            case_with("a1", Label::Attack, Provenance::Captured, 0.50),
        ];
        assert_eq!(
            operating_point(&cases, 1, BudgetScope::OnlyProvenance(Provenance::Captured)),
            Err(NoTau::EmptyBudgetScope),
            "no captured benign means D7's budget bounds nothing"
        );
        // The very same corpus under a scope that DOES hold benigns
        // fits, so the refusal is about the scope and not the corpus.
        assert!(operating_point(&cases, 1, BudgetScope::AllBenign).is_ok());
    }

    /// A single-class corpus reports THAT, not the empty scope: the
    /// cause an operator can act on comes first.
    #[test]
    fn a_single_class_corpus_reports_its_class_not_its_empty_scope() {
        let cases = vec![case_with("a1", Label::Attack, Provenance::Captured, 0.9)];
        assert_eq!(
            operating_point(&cases, 1, BudgetScope::OnlyProvenance(Provenance::Captured)),
            Err(NoTau::SingleClass(Label::Attack))
        );
    }
}
