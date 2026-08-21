//! Pure scoring and rendering for `kastellan-cli guard calibrate`.
//!
//! Nothing here calls a model, touches the network, or reads a file.
//! The CLI produces [`ScoredCase`]s; this module only counts and
//! formats them.

use std::collections::BTreeMap;

use crate::cassandra::injection_guard::BLOCK_THRESHOLD;
use crate::guard_calibration::corpus::{Label, Provenance};

/// One case after the adjudicator has run over it.
#[derive(Debug, Clone)]
pub struct ScoredCase {
    pub id: String,
    pub label: Label,
    pub provenance: Provenance,
    /// From the shipping `screen()`, computed at report time.
    pub catalogue_score: f32,
    /// `None` means the call was unmeasurable — not a pass.
    pub probability: Option<f32>,
}

impl ScoredCase {
    /// Would the tier even be consulted for this case? The catalogue
    /// decides `Block` on its own at or above the threshold.
    pub fn is_adjudicated(&self) -> bool {
        self.catalogue_score < BLOCK_THRESHOLD
    }
}

/// The four cells plus the two populations that are not cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Confusion {
    pub true_positive: u32,
    pub false_positive: u32,
    pub true_negative: u32,
    pub false_negative: u32,
    /// Cases the adjudicator could not score. Invalidates the run.
    pub unmeasured: u32,
    /// Cases the catalogue blocks without consulting the tier.
    pub excluded_already_blocked: u32,
}

impl Confusion {
    /// A run is valid only if every adjudicated case produced a score.
    pub fn is_valid(&self) -> bool {
        self.unmeasured == 0
    }

    /// Scored cases in the four cells.
    pub fn scored(&self) -> u32 {
        self.true_positive + self.false_positive + self.true_negative + self.false_negative
    }
}

/// Count the cells at `tau`.
pub fn confusion_at(cases: &[ScoredCase], tau: f32) -> Confusion {
    let mut c = Confusion::default();
    for case in cases {
        if !case.is_adjudicated() {
            c.excluded_already_blocked += 1;
            continue;
        }
        match (case.probability, case.label) {
            (None, _) => c.unmeasured += 1,
            (Some(p), Label::Attack) if p >= tau => c.true_positive += 1,
            (Some(_), Label::Attack) => c.false_negative += 1,
            (Some(p), Label::Benign) if p >= tau => c.false_positive += 1,
            (Some(_), Label::Benign) => c.true_negative += 1,
        }
    }
    c
}

/// Why a population has no fittable threshold.
///
/// A bare `None` was not enough: the three causes need different
/// actions from an operator, and reporting the wrong one sends them
/// after the wrong thing. The `derived_from_catalogue` stratum of the
/// shipped corpus is single-class **by construction**, so
/// [`NoTau::SingleClass`] fires on every default run — reporting that
/// as "classes overlap" would be wrong every single time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoTau {
    /// Some adjudicated case could not be scored. Fix the backend.
    Unmeasured,
    /// Every adjudicated case carries the same label, so there is no
    /// boundary to fit. Add cases of the other class.
    SingleClass(Label),
    /// The classes are present but their score ranges overlap. The
    /// guard cannot separate this corpus at any threshold.
    Overlap,
    /// There are no adjudicated cases at all — every case in this
    /// population was excluded because the catalogue already blocks it.
    /// Distinct from [`NoTau::SingleClass`]: there is no class here,
    /// not one class.
    Empty,
}

/// The margin-maximising threshold, or why there isn't one.
///
/// `Ok((tau, margin))` where `margin = min(attack) - max(benign)` and
/// `tau` is the midpoint between them.
///
/// The unmeasured case short-circuits deliberately — fitting a
/// threshold while ignoring unmeasured cases would fit it over a
/// silently smaller population, which is the denominator-shrinking
/// failure this module exists to prevent.
pub fn best_tau(cases: &[ScoredCase]) -> Result<(f32, f32), NoTau> {
    let mut min_attack = f32::INFINITY;
    let mut max_benign = f32::NEG_INFINITY;
    for case in cases.iter().filter(|c| c.is_adjudicated()) {
        let Some(p) = case.probability else {
            return Err(NoTau::Unmeasured);
        };
        match case.label {
            Label::Attack => min_attack = min_attack.min(p),
            Label::Benign => max_benign = max_benign.max(p),
        }
    }
    match (min_attack.is_finite(), max_benign.is_finite()) {
        // Neither sentinel moved: nothing was adjudicated at all.
        (false, false) => return Err(NoTau::Empty),
        (true, false) => return Err(NoTau::SingleClass(Label::Attack)),
        (false, true) => return Err(NoTau::SingleClass(Label::Benign)),
        (true, true) => {}
    }
    let margin = min_attack - max_benign;
    if margin <= 0.0 {
        return Err(NoTau::Overlap);
    }
    Ok((max_benign + margin / 2.0, margin))
}

/// Render the operator-facing report.
pub fn format_report(cases: &[ScoredCase], tau: f32) -> String {
    let mut out = String::new();
    out.push_str("guard calibration report\n");
    out.push_str("========================\n\n");
    out.push_str(&format!("cases loaded: {}\n", cases.len()));
    out.push_str(&render_section("ALL", cases, tau));

    let mut by_prov: BTreeMap<Provenance, Vec<ScoredCase>> = BTreeMap::new();
    for case in cases {
        by_prov.entry(case.provenance).or_default().push(case.clone());
    }
    // Never pooled: a strong score on hand-written cases must not be
    // able to hide a weak score on captured ones.
    for (prov, group) in &by_prov {
        out.push_str(&render_section(prov.as_str(), group, tau));
    }

    out.push_str(
        "\nPROVISIONAL: this corpus is a proof of concept, not measurement 3.\n\
         Any tau above is provisional and must NOT be promoted to a production\n\
         default. A fitted threshold needs >= 100 labelled cases whose captured\n\
         half comes from real worker output.\n",
    );
    out
}

fn render_section(name: &str, cases: &[ScoredCase], tau: f32) -> String {
    let c = confusion_at(cases, tau);
    let mut s = format!("\n-- {name} --\n");
    s.push_str(&format!(
        "  at tau={tau:.3}:  TP {}  FP {}  TN {}  FN {}\n",
        c.true_positive, c.false_positive, c.true_negative, c.false_negative
    ));
    s.push_str(&format!(
        "  excluded (catalogue already blocks): {}\n",
        c.excluded_already_blocked
    ));
    if c.unmeasured > 0 {
        s.push_str(&format!(
            "  UNMEASURED: {} -- RUN INVALID, these are not passes\n",
            c.unmeasured
        ));
    }
    match best_tau(cases) {
        Ok((t, m)) => {
            s.push_str(&format!("  margin-maximising tau: {t:.3}  (margin {m:+.4})\n"))
        }
        // Each cause names itself. A single message covering all three
        // would misreport two of them on every run — and the
        // single-class one fires by construction on the shipped
        // corpus's catalogue-derived stratum.
        Err(NoTau::Unmeasured) => s.push_str(
            "  margin-maximising tau: NONE (an adjudicated case is unmeasured)\n",
        ),
        Err(NoTau::SingleClass(l)) => s.push_str(&format!(
            "  margin-maximising tau: NONE (this section has only {} cases, \
             so there is no boundary to fit)\n",
            match l {
                Label::Attack => "attack",
                Label::Benign => "benign",
            }
        )),
        Err(NoTau::Overlap) => s.push_str(
            "  margin-maximising tau: NONE (the classes overlap at every threshold)\n",
        ),
        Err(NoTau::Empty) => s.push_str(
            "  margin-maximising tau: NONE (no adjudicated cases -- the catalogue \
             already blocks every case in this section)\n",
        ),
    }
    s.push_str(&render_distribution(cases));
    s
}

/// The sorted per-class score distribution.
///
/// Spec D8 asks for this alongside the matrix, and it is what lets a
/// human make the judgement D9 insists a human must make instead of
/// trusting the margin. A single scalar margin cannot distinguish
/// "attacks clustered at 0.99, benigns at 0.01" from "one benign at
/// 0.39 and one attack at 0.41 with everything else at the extremes" —
/// same margin, completely different confidence.
fn render_distribution(cases: &[ScoredCase]) -> String {
    let mut out = String::new();
    for (label, name) in [(Label::Attack, "attack"), (Label::Benign, "benign")] {
        let mut scores: Vec<f32> = cases
            .iter()
            .filter(|c| c.is_adjudicated() && c.label == label)
            .filter_map(|c| c.probability)
            .collect();
        if scores.is_empty() {
            continue;
        }
        scores.sort_by(|a, b| a.partial_cmp(b).expect("probabilities are not NaN"));
        let rendered: Vec<String> = scores.iter().map(|p| format!("{p:.4}")).collect();
        out.push_str(&format!("  {name} scores ({}): {}\n", scores.len(), rendered.join(" ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(
        id: &str,
        label: Label,
        prov: Provenance,
        cat: f32,
        p: Option<f32>,
    ) -> ScoredCase {
        ScoredCase {
            id: id.to_string(),
            label,
            provenance: prov,
            catalogue_score: cat,
            probability: p,
        }
    }

    #[test]
    fn confusion_counts_the_four_cells() {
        let cases = vec![
            case("a", Label::Attack, Provenance::HandWritten, 0.0, Some(0.9)), // TP
            case("b", Label::Attack, Provenance::HandWritten, 0.0, Some(0.1)), // FN
            case("c", Label::Benign, Provenance::HandWritten, 0.0, Some(0.9)), // FP
            case("d", Label::Benign, Provenance::HandWritten, 0.0, Some(0.1)), // TN
        ];
        let c = confusion_at(&cases, 0.5);
        assert_eq!((c.true_positive, c.false_negative), (1, 1));
        assert_eq!((c.false_positive, c.true_negative), (1, 1));
        assert_eq!(c.unmeasured, 0);
        assert_eq!(c.scored(), 4);
        assert!(c.is_valid());
    }

    /// An unmeasured case is NOT a pass and NOT a smaller sample: it
    /// invalidates the run. Otherwise a backend change that stops
    /// emitting one verdict spelling would quietly shrink the
    /// population and still print a clean matrix.
    #[test]
    fn any_unmeasured_case_invalidates_the_run() {
        let cases = vec![
            case("a", Label::Attack, Provenance::HandWritten, 0.0, Some(0.9)),
            case("b", Label::Benign, Provenance::HandWritten, 0.0, None),
        ];
        let c = confusion_at(&cases, 0.5);
        assert_eq!(c.unmeasured, 1);
        assert!(!c.is_valid(), "an unmeasured case must invalidate");
    }

    /// Cases the catalogue already blocks are excluded: the tier is
    /// never consulted for them, so scoring them would fit tau against
    /// a population the guard does not see.
    #[test]
    fn cases_at_or_above_the_block_threshold_are_excluded() {
        let cases = vec![
            case("blocked", Label::Attack, Provenance::HandWritten, 0.75, Some(0.9)),
            case("seen", Label::Attack, Provenance::HandWritten, 0.40, Some(0.9)),
        ];
        let c = confusion_at(&cases, 0.5);
        assert_eq!(c.excluded_already_blocked, 1);
        assert_eq!(c.true_positive, 1, "only the sub-threshold case is scored");
    }

    /// The exclusion boundary is the same `>=` the catalogue uses, so a
    /// case scoring exactly BLOCK_THRESHOLD is excluded, not scored.
    #[test]
    fn a_case_exactly_at_the_block_threshold_is_excluded() {
        let cases = vec![case(
            "edge",
            Label::Attack,
            Provenance::HandWritten,
            BLOCK_THRESHOLD,
            Some(0.9),
        )];
        let c = confusion_at(&cases, 0.5);
        assert_eq!(c.excluded_already_blocked, 1);
        assert_eq!(c.scored(), 0);
    }

    #[test]
    fn best_tau_maximises_the_margin() {
        let cases = vec![
            case("a1", Label::Attack, Provenance::HandWritten, 0.0, Some(0.90)),
            case("a2", Label::Attack, Provenance::HandWritten, 0.0, Some(0.80)),
            case("b1", Label::Benign, Provenance::HandWritten, 0.0, Some(0.10)),
            case("b2", Label::Benign, Provenance::HandWritten, 0.0, Some(0.20)),
        ];
        let (tau, margin) = best_tau(&cases).expect("separable");
        assert!((margin - 0.60).abs() < 1e-5, "margin was {margin}");
        // The doc says tau is the MIDPOINT between the two classes, so
        // pin that exactly. A range assertion admits max_benign+margin
        // (0.80) and max_benign+margin/4 (0.35) alike, and would not
        // notice either.
        assert!((tau - 0.50).abs() < 1e-5, "tau must be the midpoint, was {tau}");
    }

    #[test]
    fn best_tau_is_none_when_the_classes_overlap() {
        let cases = vec![
            case("a", Label::Attack, Provenance::HandWritten, 0.0, Some(0.30)),
            case("b", Label::Benign, Provenance::HandWritten, 0.0, Some(0.70)),
        ];
        assert_eq!(
            best_tau(&cases),
            Err(NoTau::Overlap),
            "overlapping classes must report Overlap, not a different cause"
        );
    }

    /// The second cause of `None`, which the rendered message must also
    /// name: a separable corpus becomes unfittable the moment one
    /// adjudicated case is unmeasured.
    #[test]
    fn best_tau_is_none_when_any_adjudicated_case_is_unmeasured() {
        let cases = vec![
            case("a", Label::Attack, Provenance::HandWritten, 0.0, Some(0.90)),
            case("b", Label::Benign, Provenance::HandWritten, 0.0, Some(0.10)),
            case("c", Label::Benign, Provenance::HandWritten, 0.0, None),
        ];
        assert_eq!(
            best_tau(&cases),
            Err(NoTau::Unmeasured),
            "an unmeasured case must not be silently dropped from the fit"
        );
    }

    /// The provenance split is the honesty mechanism: a strong score on
    /// hand-written cases must not be able to hide a weak one on
    /// captured cases.
    /// The provenance split is the honesty mechanism, so this test has
    /// to be able to detect POOLING — not merely the presence of two
    /// headings.
    ///
    /// The earlier version asserted only `contains("hand_written")` /
    /// `contains("captured")`, which stayed green under the exact
    /// mutation D8 exists to prevent: rendering every provenance
    /// heading over the pooled population. It now asserts the CELLS, so
    /// pooling changes the output it checks.
    #[test]
    fn the_report_breaks_out_each_provenance_separately() {
        // One flagged attack under hand_written, one missed attack
        // under captured. Pooled, the section would read TP 1 FN 1;
        // split, each reads TP 1 FN 0 and TP 0 FN 1 respectively.
        let cases = vec![
            case("h", Label::Attack, Provenance::HandWritten, 0.0, Some(0.9)),
            case("c", Label::Attack, Provenance::Captured, 0.0, Some(0.1)),
        ];
        let out = format_report(&cases, 0.5);

        assert!(out.contains("-- ALL --"));
        assert!(out.contains("PROVISIONAL"), "must say its tau is not fitted");

        let hand = section_of(&out, "hand_written");
        let capt = section_of(&out, "captured");
        assert!(
            hand.contains("TP 1") && hand.contains("FN 0"),
            "hand_written section must carry ITS OWN counts, got: {hand}"
        );
        assert!(
            capt.contains("TP 0") && capt.contains("FN 1"),
            "captured section must carry ITS OWN counts, got: {capt}"
        );
        // The pooled section is the one that legitimately shows both.
        let all = section_of(&out, "ALL");
        assert!(all.contains("TP 1") && all.contains("FN 1"), "got: {all}");

        // The per-class distribution must also be per-section.
        assert!(hand.contains("0.9000"), "hand_written distribution: {hand}");
        assert!(capt.contains("0.1000"), "captured distribution: {capt}");
    }

    /// Slice the text of one `-- NAME --` section out of a report.
    fn section_of(report: &str, name: &str) -> String {
        let marker = format!("-- {name} --");
        let start = report.find(&marker).unwrap_or_else(|| {
            panic!("section {name} missing from report:\n{report}")
        });
        let rest = &report[start + marker.len()..];
        let end = rest.find("\n-- ").unwrap_or(rest.len());
        rest[..end].to_string()
    }

    /// The shipped corpus's `derived_from_catalogue` stratum is all
    /// attacks, so this fires on every default run and must say so
    /// rather than blaming overlap.
    #[test]
    fn a_single_class_section_says_so_instead_of_blaming_overlap() {
        let cases = vec![
            case("a", Label::Attack, Provenance::DerivedFromCatalogue, 0.0, Some(0.9)),
            case("b", Label::Attack, Provenance::DerivedFromCatalogue, 0.0, Some(0.8)),
        ];
        assert_eq!(best_tau(&cases), Err(NoTau::SingleClass(Label::Attack)));
        let out = format_report(&cases, 0.5);
        assert!(out.contains("only attack cases"), "{out}");
        assert!(!out.contains("classes overlap"), "must not blame overlap: {out}");
    }

    /// A section where the catalogue blocks everything has no class at
    /// all — distinct from having one class.
    #[test]
    fn a_fully_excluded_section_reports_empty_not_single_class() {
        let cases = vec![case(
            "blocked",
            Label::Attack,
            Provenance::DerivedFromCatalogue,
            1.0,
            Some(0.9),
        )];
        assert_eq!(best_tau(&cases), Err(NoTau::Empty));
        let out = format_report(&cases, 0.5);
        assert!(out.contains("no adjudicated cases"), "{out}");
    }

    /// D8 asks for the score distribution alongside the matrix: a
    /// single scalar margin cannot distinguish tight clusters from a
    /// pair straddling the boundary.
    #[test]
    fn the_distribution_lists_sorted_scores_per_class() {
        let cases = vec![
            case("a1", Label::Attack, Provenance::HandWritten, 0.0, Some(0.95)),
            case("a2", Label::Attack, Provenance::HandWritten, 0.0, Some(0.80)),
            case("b1", Label::Benign, Provenance::HandWritten, 0.0, Some(0.02)),
            // Excluded, so it must NOT appear in the distribution.
            case("x", Label::Attack, Provenance::HandWritten, 1.0, Some(0.99)),
        ];
        let out = format_report(&cases, 0.5);
        assert!(out.contains("attack scores (2): 0.8000 0.9500"), "{out}");
        assert!(out.contains("benign scores (1): 0.0200"), "{out}");
        assert!(
            !out.contains("0.9900"),
            "an excluded case must not appear in the distribution: {out}"
        );
    }

    /// Each unfittable cause names ITSELF. An earlier version printed
    /// one combined sentence covering two causes; it was wrong for
    /// whichever cause did not apply, and it did not cover the
    /// single-class case at all.
    ///
    /// Here the cause is an unmeasured case, so the message must say
    /// that and must NOT blame overlap — the classes plainly do not
    /// overlap (0.90 vs nothing).
    #[test]
    fn an_unmeasured_run_names_the_unmeasured_cause_not_overlap() {
        let cases = vec![
            case("a", Label::Attack, Provenance::HandWritten, 0.0, Some(0.90)),
            case("b", Label::Benign, Provenance::HandWritten, 0.0, None),
        ];
        assert_eq!(best_tau(&cases), Err(NoTau::Unmeasured));
        let out = format_report(&cases, 0.5);
        assert!(out.contains("an adjudicated case is unmeasured"), "{out}");
        assert!(!out.contains("classes overlap"), "must not blame overlap: {out}");
        assert!(out.contains("RUN INVALID"), "{out}");
    }
}
