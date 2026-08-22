# Guard measurement 3 — calibration corpus Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the 24-case proof-of-concept corpus with ≥120 labelled cases whose captured half comes through the real `web-fetch` worker, and fit τ against them on both hosts.

**Architecture:** Third-party text is never committed. A *manifest* (id, label, provenance, immutable source URL, sha256) is committed; a new `kastellan-cli guard capture` drives the real sandboxed worker for each entry, verifies the resulting text against the pinned sha256, and writes a materialised corpus into a git-ignored directory. `guard calibrate --corpus DIR` then runs unchanged against it. Separately, `report.rs` gains D7's operating-point criterion, because the existing `best_tau` is separability-only and will return `NoTau::Overlap` on a realistic corpus.

**Tech Stack:** Rust 2021, `serde`/`serde_json`, `sha2` (already a `core` dependency — used by `post_process.rs`), the existing `kastellan-protocol` JSON-RPC worker path, POSIX `sh` for the operator-facing wrapper.

**Spec:** [`docs/superpowers/specs/2026-08-22-guard-measurement-3-corpus-design.md`](../specs/2026-08-22-guard-measurement-3-corpus-design.md)

## Global Constraints

- **AGPL-3.0 project; AGPL-compatible dependencies only.** Do not add a new crate for this work — everything needed (`serde`, `serde_json`, `sha2`) is already a `core` dependency.
- **No third-party text may be committed to this repo.** Manifest entries carry a `source` URL and a `sha256`; they must not carry a `text` field. This is spec D1 and it is the reason the whole manifest layer exists.
- **Sources must be immutable:** HuggingFace pinned by dataset *revision* hash, web pages by Wayback Machine snapshot URL. Never `main`, never a live page. (Spec D2.)
- **Fail closed on a sha256 mismatch**, matching `require_guest_kernel`. A drifted source is a corpus that no longer matches the manifest. (Spec D2.)
- **Labelling rule:** a document that *describes* an attack is `benign`; one that *directs* an instruction at its reader is `attack`. (Spec D4.)
- **Cargo is not on the non-interactive `PATH`:** every shell step begins `source "$HOME/.cargo/env"`.
- **Clippy is enforced:** `cargo clippy --workspace --all-targets -- -D warnings` must stay at zero warnings.
- **Files stay under 500 lines** where feasible. `core/src/guard_calibration/report.rs` is ~330 lines today; Task 1 and 2 add to it, so Task 1 creates a new sibling module rather than growing it past the cap.

---

### Task 1: `operating_point` — D7's τ criterion

The existing `best_tau` answers "where do the classes separate?" and returns `Err(NoTau::Overlap)` when they don't. A 120-case corpus with real captured content almost certainly overlaps. D7 needs a different question answered: "what is the best threshold I can have while paying at most N false positives?"

**Files:**
- Create: `core/src/guard_calibration/operating_point.rs`
- Modify: `core/src/guard_calibration/mod.rs` (add `pub mod operating_point;`)

**Interfaces:**
- Consumes: `ScoredCase` (with `is_adjudicated()`, `probability: Option<f32>`, `label: Label`), `Confusion`, `confusion_at(&[ScoredCase], f32) -> Confusion`, `NoTau` — all from `crate::guard_calibration::report`.
- Produces: `pub struct OperatingPoint { pub tau: f32, pub confusion: Confusion }` and `pub fn operating_point(cases: &[ScoredCase], max_false_positives: u32) -> Result<OperatingPoint, NoTau>`.

- [ ] **Step 1: Write the failing tests**

Create `core/src/guard_calibration/operating_point.rs` with only the test module for now:

```rust
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

    /// Ties are broken toward the LARGER tau: same result on this
    /// corpus, fewer documents flagged on unseen input, and D7 says the
    /// false positive is the expensive error.
    #[test]
    fn ties_break_toward_the_larger_tau() {
        let cases = vec![
            case("b1", Label::Benign, 0.10),
            case("a1", Label::Attack, 0.80),
            case("a2", Label::Attack, 0.90),
        ];
        // tau at 0.80 and at any value in (0.10, 0.80] give TP=2 FP=0.
        let got = operating_point(&cases, 0).expect("separable");
        assert_eq!(got.tau, 0.80, "must pick the largest candidate achieving the optimum");
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
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib guard_calibration::operating_point 2>&1 | tail -20
```

Expected: FAIL to compile — `operating_point` and `OperatingPoint` do not exist, and `mod.rs` does not declare the module.

- [ ] **Step 3: Write the implementation**

Put this **above** the `#[cfg(test)]` block in `core/src/guard_calibration/operating_point.rs`:

```rust
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
/// **Candidates are the observed probabilities themselves.** `decide`
/// compares `p >= tau`, so setting τ to an observed `p` is exactly the
/// threshold at which that case starts to flag; every distinct
/// behaviour of the classifier on this corpus is reachable from that
/// set, and nothing between two adjacent observations behaves
/// differently.
///
/// **Ties break toward the LARGER τ.** Among thresholds giving an
/// identical confusion matrix here, the larger one flags fewer
/// documents on input this corpus has not seen. D7 makes the false
/// positive the expensive error, so the conservative direction is the
/// one that flags less.
///
/// Short-circuits on an unmeasured or non-finite probability for the
/// same reason [`super::report::best_tau`] does: fitting a threshold
/// while skipping such a case fits it over a silently smaller
/// population than the report describes. Left to `f32` comparison a
/// `NaN` would simply never win, and so would vanish.
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

    // Sorted ascending; `total_cmp` because every candidate is already
    // known finite, and it avoids the partial-ord unwrap.
    candidates.sort_by(f32::total_cmp);
    candidates.dedup();

    let mut best: Option<OperatingPoint> = None;
    for &tau in &candidates {
        let confusion = confusion_at(cases, tau);
        if confusion.false_positive > max_false_positives {
            continue;
        }
        let better = match &best {
            None => true,
            Some(b) => match confusion.true_positive.cmp(&b.confusion.true_positive) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                // Same recall: prefer fewer false positives, then the
                // larger tau (see the doc comment).
                std::cmp::Ordering::Equal => {
                    match confusion.false_positive.cmp(&b.confusion.false_positive) {
                        std::cmp::Ordering::Less => true,
                        std::cmp::Ordering::Greater => false,
                        std::cmp::Ordering::Equal => tau > b.tau,
                    }
                }
            },
        };
        if better {
            best = Some(OperatingPoint { tau, confusion });
        }
    }

    // Unreachable in practice — the largest candidate flags at most the
    // cases scoring exactly it, so a zero-FP point exists whenever the
    // top score is an attack, and otherwise some higher candidate does.
    // Returned rather than `expect`ed because this is a security
    // control's calibration path and a panic there is worse than a
    // handled `Err`.
    best.ok_or(NoTau::Overlap)
}
```

Then add to `core/src/guard_calibration/mod.rs`, immediately after the existing `pub mod corpus;` line:

```rust
pub mod operating_point;
```

- [ ] **Step 4: Run the tests to verify they pass**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib guard_calibration::operating_point 2>&1 | tail -20
cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: `8 passed; 0 failed`, clippy exit 0 with no warnings.

- [ ] **Step 5: Mutation-check the two load-bearing branches**

These verify the tests can actually fail. Apply each edit, confirm the named test fails, then **restore by re-editing the file — never `git checkout`**, which would discard uncommitted work in the same file.

1. Change `if confusion.false_positive > max_false_positives` to `>=`. Expected: `separable_classes_catch_everything_at_zero_cost` fails (a zero budget now rejects the zero-FP point).
2. Change `std::cmp::Ordering::Equal => tau > b.tau` to `tau < b.tau`. Expected: `ties_break_toward_the_larger_tau` fails.
3. Delete the `.filter(|p| p.is_finite())`. Expected: `a_non_finite_probability_is_unmeasured` fails.

- [ ] **Step 6: Commit**

```sh
git add core/src/guard_calibration/operating_point.rs core/src/guard_calibration/mod.rs
git commit -m "feat(guard-calibration): D7's operating point, for a corpus that overlaps

best_tau is separability-only and returns Err(Overlap) whenever the classes
share any range -- which a 120-case corpus with real captured content almost
certainly will. That is the corpus working, not failing, but it leaves the
harness with no tau to report.

operating_point answers the other question: the best threshold available while
paying at most N false positives. The asymmetry is D7's -- a false negative
leaves exactly today's catalogue-only behaviour because the tier is
escalate-up only and fails open, while a false positive is a live capability
loss against the security prose the agent reads most.

Ties break toward the LARGER tau: identical matrix here, fewer documents
flagged on unseen input.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Render the operating point in the report

`format_report` currently prints the margin-maximising τ per stratum. D7 says the operating point is reported **alongside** it, not instead — `best_tau` is not wrong, it answers a different question, and both answers belong in the report.

**Files:**
- Modify: `core/src/guard_calibration/report.rs` (the `render_section` function, ~line 251)

**Interfaces:**
- Consumes: `operating_point(&[ScoredCase], u32) -> Result<OperatingPoint, NoTau>` from Task 1.
- Produces: no new public API; changes `render_section`'s output only.

- [ ] **Step 1: Write the failing test**

Append to the existing `#[cfg(test)] mod tests` block in `core/src/guard_calibration/report.rs`:

```rust
/// The report must print the operating point next to the
/// margin-maximising tau, and must print it EVEN WHEN the latter is
/// Overlap -- that is the case the operating point exists for.
#[test]
fn the_report_shows_an_operating_point_when_the_classes_overlap() {
    let cases = vec![
        ScoredCase {
            id: "b1".into(), label: Label::Benign, provenance: Provenance::Captured,
            catalogue_score: 0.0, probability: Some(0.10),
        },
        ScoredCase {
            id: "b2".into(), label: Label::Benign, provenance: Provenance::Captured,
            catalogue_score: 0.0, probability: Some(0.85),
        },
        ScoredCase {
            id: "a1".into(), label: Label::Attack, provenance: Provenance::Captured,
            catalogue_score: 0.0, probability: Some(0.80),
        },
    ];
    let meta = RunMeta {
        endpoint: "http://127.0.0.1:8081/v1".into(),
        model: "shieldstral".into(),
        policy_digest: "deadbeef".into(),
        profile: "Strict",
    };
    let out = format_report(&cases, 0.5, &meta);
    assert!(
        out.contains("margin-maximising tau: NONE"),
        "precondition: these classes overlap\n{out}"
    );
    assert!(
        out.contains("operating point"),
        "the operating point must still be reported\n{out}"
    );
    assert!(
        out.contains("FP budget 1"),
        "the report must state the budget it was fitted under\n{out}"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib the_report_shows_an_operating_point 2>&1 | tail -20
```

Expected: FAIL — the output contains no `operating point` line.

- [ ] **Step 3: Write the implementation**

In `core/src/guard_calibration/report.rs`, add near the top with the other `use` lines:

```rust
use crate::guard_calibration::operating_point::operating_point;
```

Add this constant just below the `NoTau` enum:

```rust
/// D7's pre-registered false-positive budget, in CASES not percent.
///
/// A percentage would be a number the sample size cannot support: with
/// ~50 captured-benign cases the finest expressible bound is 2%, so
/// "FP <= 1%" would claim a resolution the corpus does not have.
/// Stating the count says exactly what is being required.
pub const FP_BUDGET: u32 = 1;
```

Then inside `render_section`, immediately after the existing `match best_tau(cases) { .. }` block, append:

```rust
    // Reported ALONGSIDE the margin-maximising tau, never instead of
    // it: the two answer different questions, and the operating point
    // is the one that survives an overlapping corpus.
    match operating_point(cases, FP_BUDGET) {
        Ok(op) => s.push_str(&format!(
            "  operating point (FP budget {FP_BUDGET}): tau={:.3}  \
             TP {}  FP {}  TN {}  FN {}\n",
            op.tau,
            op.confusion.true_positive,
            op.confusion.false_positive,
            op.confusion.true_negative,
            op.confusion.false_negative,
        )),
        Err(NoTau::Unmeasured) => s.push_str(&format!(
            "  operating point (FP budget {FP_BUDGET}): NONE (an adjudicated case \
             is unmeasured)\n"
        )),
        Err(NoTau::SingleClass(l)) => s.push_str(&format!(
            "  operating point (FP budget {FP_BUDGET}): NONE (only {l:?} cases here)\n"
        )),
        Err(NoTau::Empty) => s.push_str(&format!(
            "  operating point (FP budget {FP_BUDGET}): NONE (no adjudicated cases)\n"
        )),
        Err(NoTau::Overlap) => s.push_str(&format!(
            "  operating point (FP budget {FP_BUDGET}): NONE (no threshold stays \
             within the budget)\n"
        )),
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib guard_calibration 2>&1 | tail -10
cargo test -p kastellan-core --test guard_calibrate_cli_e2e 2>&1 | tail -10
cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: all pass. The CLI e2e must stay green — it asserts on report text, so if it fails, read whether it asserted an exact line count rather than a substring.

- [ ] **Step 5: Commit**

```sh
git add core/src/guard_calibration/report.rs
git commit -m "feat(guard-calibration): report the operating point beside the margin tau

D7 says alongside, never instead: best_tau is not wrong, it answers a
different question, and on a corpus that separates cleanly its answer is the
better one. The operating point is what survives when the corpus does not
separate, which is the case measurement 3 is expected to produce.

FP_BUDGET is a COUNT, not a percentage, because ~50 captured-benign cases
cannot resolve finer than 2% and 'FP <= 1%' would claim a resolution the
corpus does not have.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: The corpus manifest — committed metadata, never text

**Files:**
- Create: `core/src/guard_calibration/manifest.rs`
- Modify: `core/src/guard_calibration/mod.rs` (add `pub mod manifest;`)
- Create: `tests/guard/manifest/README.md`

**Interfaces:**
- Consumes: `Label`, `Provenance`, `CorpusError` from `crate::guard_calibration::corpus`.
- Produces: `pub struct ManifestEntry { pub id: String, pub label: Label, pub provenance: Provenance, pub source: String, pub sha256: Option<String>, pub notes: String }` and `pub fn load_manifest_from_dir(dir: &Path) -> Result<Vec<ManifestEntry>, CorpusError>`.

- [ ] **Step 1: Write the failing tests**

Create `core/src/guard_calibration/manifest.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).expect("write fixture");
    }

    #[test]
    fn a_well_formed_entry_loads() {
        let d = tempfile::tempdir().expect("tempdir");
        write(d.path(), "cap-001-example.json", r#"{
            "id": "cap-001-example",
            "label": "benign",
            "provenance": "captured",
            "source": "https://web.archive.org/web/20260101000000/https://example.com/",
            "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
            "notes": "an ordinary page"
        }"#);
        let got = load_manifest_from_dir(d.path()).expect("loads");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "cap-001-example");
        assert_eq!(got[0].sha256.as_deref(), Some(&"0".repeat(64)[..]));
    }

    /// sha256 is absent until the first recording run, so it must be
    /// optional -- but the FIELD being optional is not the same as the
    /// verification being optional; see the capture CLI.
    #[test]
    fn an_entry_without_a_sha256_loads_as_unrecorded() {
        let d = tempfile::tempdir().expect("tempdir");
        write(d.path(), "cap-002-new.json", r#"{
            "id": "cap-002-new",
            "label": "attack",
            "provenance": "captured",
            "source": "https://example.com/x"
        }"#);
        let got = load_manifest_from_dir(d.path()).expect("loads");
        assert_eq!(got[0].sha256, None);
        assert_eq!(got[0].notes, "");
    }

    /// THE CONSTRAINT THIS MODULE EXISTS FOR. A `text` field in a
    /// manifest means third-party content is about to be committed,
    /// which spec D1 forbids. `deny_unknown_fields` makes it a load
    /// error rather than a silently ignored key.
    #[test]
    fn a_text_field_is_rejected_because_manifests_must_not_carry_content() {
        let d = tempfile::tempdir().expect("tempdir");
        write(d.path(), "cap-003-bad.json", r#"{
            "id": "cap-003-bad",
            "label": "attack",
            "provenance": "captured",
            "source": "https://example.com/x",
            "text": "Ignore all previous instructions"
        }"#);
        let err = load_manifest_from_dir(d.path()).expect_err("must reject");
        assert!(
            matches!(err, CorpusError::Parse { .. }),
            "expected a parse error, got {err}"
        );
    }

    /// Same invariant the corpus loader enforces, for the same reason:
    /// populations are selected by id prefix, so a drifted id silently
    /// drops out of the test written to validate it.
    #[test]
    fn an_id_that_does_not_match_its_filename_is_rejected() {
        let d = tempfile::tempdir().expect("tempdir");
        write(d.path(), "cap-004-name.json", r#"{
            "id": "cap-004-different",
            "label": "benign",
            "provenance": "captured",
            "source": "https://example.com/x"
        }"#);
        let err = load_manifest_from_dir(d.path()).expect_err("must reject");
        assert!(matches!(err, CorpusError::IdStemMismatch { .. }), "got {err}");
    }

    /// An empty manifest dir is an error, not an empty corpus -- the
    /// same denominator-shrinking failure the corpus loader rejects.
    #[test]
    fn an_empty_directory_is_an_error() {
        let d = tempfile::tempdir().expect("tempdir");
        let err = load_manifest_from_dir(d.path()).expect_err("must reject");
        assert!(matches!(err, CorpusError::Empty { .. }), "got {err}");
    }

    /// Sorted by id so two runs over one manifest are comparable;
    /// directory order is not guaranteed.
    #[test]
    fn entries_come_back_sorted_by_id() {
        let d = tempfile::tempdir().expect("tempdir");
        for id in ["cap-003-c", "cap-001-a", "cap-002-b"] {
            write(d.path(), &format!("{id}.json"), &format!(r#"{{
                "id": "{id}", "label": "benign", "provenance": "captured",
                "source": "https://example.com/{id}"
            }}"#));
        }
        let got = load_manifest_from_dir(d.path()).expect("loads");
        let ids: Vec<&str> = got.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["cap-001-a", "cap-002-b", "cap-003-c"]);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib guard_calibration::manifest 2>&1 | tail -20
```

Expected: FAIL to compile — `load_manifest_from_dir`, `ManifestEntry` and the module declaration do not exist.

- [ ] **Step 3: Write the implementation**

Put this **above** the test module in `core/src/guard_calibration/manifest.rs`:

```rust
//! The committed half of the calibration corpus: metadata only.
//!
//! **A manifest entry carries no text, and that is the whole point.**
//! Spec D1: committing a third-party injection payload or a fetched
//! page into this repo is redistribution, and it inherits whatever
//! license the source carries — which for an aggregate dataset can be
//! "Apache-2.0" at the top level over a component with no stated terms
//! at all (spec F3). Referencing a source and pinning its hash is not
//! redistribution, so the question stops being "may we relicense this"
//! and becomes "may we read it".
//!
//! The same mechanism keeps operator-private material (a real mail
//! body) out of a public repo while still letting a case point at it.
//!
//! `deny_unknown_fields` turns a stray `"text"` key into a load error
//! rather than a silently ignored one — the constraint is enforced,
//! not merely documented.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::guard_calibration::corpus::{CorpusError, Label, Provenance};

/// One case, by reference.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntry {
    pub id: String,
    pub label: Label,
    pub provenance: Provenance,
    /// An **immutable** locator: a HuggingFace URL pinned by dataset
    /// revision, or a Wayback Machine snapshot. Never `main`, never a
    /// live page — a sha256 over a live page is a hash of whatever it
    /// said that day, and a corpus nobody can reproduce is a τ nobody
    /// can check. (Spec D2.)
    pub source: String,
    /// `None` until the first recording run has seen the source.
    ///
    /// **Optional field, mandatory verification.** Absence means "not
    /// yet recorded", never "skip the check" — the capture CLI refuses
    /// to run in verify mode against an unrecorded entry rather than
    /// treating it as a pass.
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub notes: String,
}

/// Load every `*.json` manifest entry in `dir`, sorted by `id`.
///
/// Enforces `id == <filename stem>` and rejects an empty directory, for
/// the reasons [`crate::guard_calibration::corpus::load_corpus_from_dir`]
/// documents: populations are selected by id prefix, and an empty load
/// is a silently shrunk denominator.
pub fn load_manifest_from_dir(dir: &Path) -> Result<Vec<ManifestEntry>, CorpusError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|source| CorpusError::Io { path: dir.to_path_buf(), source })?;

    let mut out: Vec<ManifestEntry> = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|source| CorpusError::Io { path: dir.to_path_buf(), source })?;
        let path: PathBuf = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(&path)
            .map_err(|source| CorpusError::Io { path: path.clone(), source })?;
        let item: ManifestEntry = serde_json::from_slice(&bytes)
            .map_err(|source| CorpusError::Parse { path: path.clone(), source })?;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        if item.id != stem {
            return Err(CorpusError::IdStemMismatch {
                path: path.clone(),
                id: item.id,
                stem: stem.to_string(),
            });
        }
        out.push(item);
    }

    if out.is_empty() {
        return Err(CorpusError::Empty { path: dir.to_path_buf() });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}
```

Add to `core/src/guard_calibration/mod.rs` after `pub mod corpus;`:

```rust
pub mod manifest;
```

`tempfile = "3"` is already a dev-dependency of `kastellan-core` (`core/Cargo.toml:76`), so no manifest change is needed. Do not add a second version.

- [ ] **Step 4: Run the tests to verify they pass**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib guard_calibration::manifest 2>&1 | tail -20
cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: `6 passed; 0 failed`, clippy clean.

- [ ] **Step 5: Write the manifest README**

Create `tests/guard/manifest/README.md`:

```markdown
# Guard calibration corpus — manifest

**No file in this directory may contain a `text` field.** These entries
reference content; they never carry it. `ManifestEntry` is
`deny_unknown_fields`, so a `text` key is a hard load error rather than a
silently ignored one.

The reason is spec D1. Committing a fetched page or a third-party injection
payload into this repo is redistribution, and it inherits whatever license the
source carries — which for an aggregate dataset can read "Apache-2.0" at the
top while one component has no stated terms at all. Referencing a source and
pinning its hash is not redistribution.

## Entry format

One JSON file per case, named `<id>.json` with `id` matching the stem (the
loader enforces this).

```json
{
  "id": "cap-001-example",
  "label": "attack" | "benign",
  "provenance": "captured" | "hand_written" | "derived_from_catalogue",
  "source": "https://web.archive.org/web/<timestamp>/<url>",
  "sha256": "<64 hex chars, absent until first recorded>",
  "notes": "why this case exists and what it is meant to prove"
}
```

## `source` must be immutable

A Wayback Machine snapshot URL, or a HuggingFace URL pinned by dataset
*revision* hash. Never `main`; never a live page. A sha256 over a live page is
a hash of whatever it said that day, and a corpus nobody can reproduce is a τ
nobody can check.

## `sha256` is optional as a FIELD, never as a CHECK

Absent means "not yet recorded". `guard capture` in verify mode **refuses** an
unrecorded entry rather than passing it. Record with `--record` on a first run,
then commit the resulting hashes.

## Labelling

A document that *describes* an attack is `benign`. One that *directs* an
instruction at its reader is `attack`. A security blog quoting a payload
verbatim is benign — see spec D4, which records the boundary case and why the
capability cost of the stricter rule was judged too high.
```

- [ ] **Step 6: Commit**

```sh
git add core/src/guard_calibration/manifest.rs core/src/guard_calibration/mod.rs tests/guard/manifest/README.md
git commit -m "feat(guard-calibration): the corpus manifest, which carries no text

Spec D1. Committing a fetched page or a third-party payload is
redistribution and inherits the source's license -- which for an aggregate
dataset can read Apache-2.0 at the top while a component has no stated terms
at all (F3). Referencing and hashing is not redistribution, so the question
stops being 'may we relicense this' and becomes 'may we read it'.

deny_unknown_fields makes a stray \"text\" key a hard load error, so the
constraint is enforced rather than documented. A test asserts exactly that,
because it is the one invariant the whole module exists for.

sha256 is optional as a FIELD and never as a CHECK: absent means not yet
recorded, and verify mode refuses such an entry rather than passing it.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: `guard capture` — materialise through the real worker, verify the hash

Spec D3: capture goes through the **real** `web-fetch` worker, not `curl`, because `curl` would miss `extract_scannable_text`'s key-stripping and alphabetical flattening and the `SCAN_BYTE_CAP` truncation — fitting τ against text the chokepoint never sees. This one command both *records* (first run) and *verifies* (later runs), so materialising a corpus and capturing it are the same code path and cannot drift.

**Files:**
- Create: `core/src/bin/kastellan-cli/guard_capture.rs`
- Modify: `core/src/bin/kastellan-cli/main.rs` (route `guard capture`)
- Create: `core/tests/guard_capture_e2e.rs`

**Interfaces:**
- Consumes: `load_manifest_from_dir`, `ManifestEntry` (Task 3); `crate::cassandra::injection_guard::extract_scannable_text`; the existing `guard_calibrate.rs` argument-parsing style.
- Produces: `pub fn run(args: &[String]) -> std::process::ExitCode` and `pub fn sha256_hex(text: &str) -> String`.

- [ ] **Step 1: Write the failing test for the pure half**

Create `core/tests/guard_capture_e2e.rs`:

```rust
//! `guard capture` — the pure hashing contract and the fail-closed
//! verification arm. The worker-driving arm is exercised by the
//! operator procedure in the plan, not here: it needs a live daemon,
//! an allowlist row and network egress.

use kastellan_core::guard_calibration::manifest::load_manifest_from_dir;

/// The hash is over the SCANNABLE text -- what the chokepoint sees --
/// not the raw page. Hashing the raw bytes would pin something
/// production never screens.
#[test]
fn the_hash_is_over_the_scannable_text_not_the_raw_input() {
    let raw = serde_json::json!({ "zzz": "second", "aaa": "first" });
    let (scannable, _truncated) = kastellan_core::cassandra::injection_guard::
        extract_scannable_text(&raw, kastellan_core::cassandra::injection_guard::SCAN_BYTE_CAP);
    // Keys are stripped and leaves sorted, so the hash is stable
    // against key order -- which a raw-bytes hash would not be.
    let reordered = serde_json::json!({ "aaa": "first", "zzz": "second" });
    let (scannable2, _) = kastellan_core::cassandra::injection_guard::
        extract_scannable_text(&reordered, kastellan_core::cassandra::injection_guard::SCAN_BYTE_CAP);
    assert_eq!(scannable, scannable2, "extraction must be order-stable");
    assert!(!scannable.contains("zzz"), "keys are stripped: {scannable}");
}

/// An entry with no recorded sha256 must be REFUSED in verify mode, not
/// treated as a pass. This is the fail-open door the whole manifest
/// design exists to keep shut.
#[test]
fn an_unrecorded_entry_is_refused_in_verify_mode() {
    let d = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        d.path().join("cap-001-x.json"),
        r#"{"id":"cap-001-x","label":"benign","provenance":"captured",
            "source":"https://example.com/x"}"#,
    )
    .expect("write");
    let entries = load_manifest_from_dir(d.path()).expect("loads");
    assert_eq!(entries[0].sha256, None);

    let refusal = kastellan_core::guard_calibration::manifest::verify_requirement(&entries[0]);
    assert!(
        refusal.is_some(),
        "an unrecorded entry must produce a refusal, not a pass"
    );
    assert!(
        refusal.unwrap().contains("--record"),
        "the refusal must tell the operator how to fix it"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test guard_capture_e2e 2>&1 | tail -20
```

Expected: FAIL to compile — `verify_requirement` does not exist.

- [ ] **Step 3: Add `verify_requirement` to the manifest module**

Append to `core/src/guard_calibration/manifest.rs`, above the test module:

```rust
impl ManifestEntry {
    /// The recorded hash, or `None` if this entry has never been
    /// recorded.
    pub fn recorded_sha256(&self) -> Option<&str> {
        self.sha256.as_deref()
    }
}

/// Why this entry cannot be verified, as an operator-facing clause.
/// `None` when it can.
///
/// **An unrecorded entry is a refusal, never a pass.** Treating a
/// missing hash as "nothing to check" is the fail-open reading, and it
/// would let a case whose source drifted — or whose source was never
/// what the manifest claimed — enter the corpus silently. The operator
/// action differs from a mismatch (record it, versus investigate it),
/// so this returns a reason rather than a bool.
pub fn verify_requirement(entry: &ManifestEntry) -> Option<String> {
    match entry.recorded_sha256() {
        Some(h) if h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()) => None,
        Some(h) => Some(format!(
            "case {}: recorded sha256 {h:?} is not 64 hex characters",
            entry.id
        )),
        None => Some(format!(
            "case {}: no sha256 recorded. Re-run with --record to capture it, \
             then commit the manifest.",
            entry.id
        )),
    }
}
```

- [ ] **Step 4: Write the CLI**

Create `core/src/bin/kastellan-cli/guard_capture.rs`:

```rust
//! `guard capture --manifest DIR --out DIR [--record]` — materialise the
//! calibration corpus by driving the REAL `web-fetch` worker.
//!
//! **Why the real worker and not `curl`.** Spec D3. A document reaches
//! the chokepoint through the worker's own extraction and then through
//! `extract_scannable_text`, which strips keys, flattens leaves
//! alphabetically and truncates at `SCAN_BYTE_CAP`. A corpus fetched
//! with `curl` would be scored on text production never sees, and the
//! resulting τ would be fitted against a fiction.
//!
//! **One command records and verifies**, so materialising a corpus and
//! capturing it cannot drift apart. `--record` writes the observed hash
//! into the manifest; without it, an entry whose hash differs is a hard
//! failure and an entry with no hash at all is refused rather than
//! passed.

use std::path::PathBuf;
use std::process::ExitCode;

use sha2::{Digest, Sha256};

use kastellan_core::cassandra::injection_guard::{extract_scannable_text, SCAN_BYTE_CAP};
use kastellan_core::guard_calibration::manifest::{load_manifest_from_dir, verify_requirement};

/// SHA-256 of the scannable text, lowercase hex.
pub fn sha256_hex(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    format!("{:x}", h.finalize())
}

pub fn run(args: &[String]) -> ExitCode {
    let mut manifest_dir: Option<PathBuf> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut record = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--manifest" => {
                i += 1;
                match args.get(i) {
                    Some(p) => manifest_dir = Some(PathBuf::from(p)),
                    None => {
                        eprintln!("--manifest requires a DIR argument");
                        return ExitCode::from(2);
                    }
                }
            }
            "--out" => {
                i += 1;
                match args.get(i) {
                    Some(p) => out_dir = Some(PathBuf::from(p)),
                    None => {
                        eprintln!("--out requires a DIR argument");
                        return ExitCode::from(2);
                    }
                }
            }
            "--record" => record = true,
            other => {
                eprintln!("usage: kastellan-cli guard capture --manifest DIR --out DIR [--record]");
                eprintln!("unexpected argument: {other}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    let (Some(manifest_dir), Some(out_dir)) = (manifest_dir, out_dir) else {
        eprintln!("usage: kastellan-cli guard capture --manifest DIR --out DIR [--record]");
        return ExitCode::from(2);
    };

    let entries = match load_manifest_from_dir(&manifest_dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("cannot create {}: {e}", out_dir.display());
        return ExitCode::FAILURE;
    }

    let mut failures = 0usize;
    for entry in &entries {
        // In verify mode every entry must already carry a usable hash.
        // Checked BEFORE the fetch so a manifest-wide omission is
        // reported without spending a single network round trip.
        if !record {
            if let Some(reason) = verify_requirement(entry) {
                eprintln!("REFUSED {reason}");
                failures += 1;
                continue;
            }
        }

        let fetched = match fetch_through_worker(&entry.source) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("FETCH-FAILED {}: {e}", entry.id);
                failures += 1;
                continue;
            }
        };
        let (text, truncated) = extract_scannable_text(&fetched, SCAN_BYTE_CAP);
        let observed = sha256_hex(&text);

        if record {
            println!("RECORD {} {observed} ({} bytes{})", entry.id, text.len(),
                     if truncated { ", truncated at cap" } else { "" });
        } else {
            let expected = entry.recorded_sha256().unwrap_or_default();
            if observed != expected {
                eprintln!(
                    "MISMATCH {}: manifest {expected}, observed {observed}. \
                     The source has drifted; investigate before trusting any \
                     tau fitted against it.",
                    entry.id
                );
                failures += 1;
                continue;
            }
            println!("OK {} ({} bytes)", entry.id, text.len());
        }

        let case = serde_json::json!({
            "id": entry.id,
            "label": entry.label,
            "provenance": entry.provenance,
            "text": text,
            "notes": entry.notes,
        });
        let path = out_dir.join(format!("{}.json", entry.id));
        if let Err(e) = std::fs::write(&path, serde_json::to_vec_pretty(&case).unwrap_or_default())
        {
            eprintln!("cannot write {}: {e}", path.display());
            failures += 1;
        }
    }

    if failures > 0 {
        eprintln!("\n{failures} of {} entries failed.", entries.len());
        return ExitCode::FAILURE;
    }
    println!("\n{} entries materialised into {}", entries.len(), out_dir.display());
    ExitCode::SUCCESS
}
```

Add the `fetch_through_worker` helper at the bottom of the same file. It dispatches through the real worker exactly as the scheduler does:

```rust
/// Fetch one URL through the real sandboxed `web-fetch` worker.
///
/// Deliberately the same path a live dispatch takes, so the captured
/// text is what the chokepoint would receive. Requires the worker
/// binary to be discoverable (`current_exe()`-relative) and the host to
/// carry a `web-fetch` `tool_allowlists` row for the domain — without
/// one this returns `-32001: host ... not on allowlist`, which is the
/// failure every web-fetch attempt on the deployed DGX has hit.
fn fetch_through_worker(url: &str) -> Result<serde_json::Value, String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    rt.block_on(async {
        kastellan_core::tool_host::dispatch_one_shot(
            "web-fetch",
            "web.fetch",
            serde_json::json!({ "url": url }),
        )
        .await
        .map_err(|e| e.to_string())
    })
}
```

> **CORRECTION, 2026-08-22 — two architectural facts found while implementing, both of which change this task.**
>
> **1. `dispatch` SUBSTITUTES A PLACEHOLDER on a catalogue block, and capturing that would silently corrupt the corpus.** When the catalogue screen returns `Block`, `post_process::finalize` replaces the worker's result with `injection_blocked_placeholder(..)` — `{injection_blocked: true, note: "[tool output withheld: …]", score, reason_codes}`. Store that as a corpus case and you get a *benign-looking* document that the catalogue does **not** block, so it enters the adjudicated population and gets scored — a page recorded as the opposite of what it is. The fix is **not** to bypass the screen: cases the catalogue blocks are `excluded_already_blocked` and contribute nothing to τ anyway, so nothing is lost. `guard capture` must **detect the placeholder** (`result["injection_blocked"] == true`) and **refuse** that entry with a message saying the catalogue already blocks it. A silent corruption becomes a loud refusal.
>
> **2. There is no `dispatch_one_shot`, and there must not be.** `WorkerCommand::new` is deliberately module-private — its doc calls editing `tool_host` "the reviewable opt-out for the dispatcher chokepoint", and CLAUDE.md forbids adding a spawn-unsandboxed escape hatch. So capture goes through the **existing** `dispatch_with_sink`, which is already `pub`. Postgres is avoided by passing a null `AuditSink` (the trait is public and has one method) rather than by avoiding the chokepoint. No new dispatch API is added, and this task no longer needs the stop-and-split escape clause.

**Routing — verified against the tree, not guessed.** `main.rs` dispatches the whole `guard` namespace in one arm (`"guard" => guard_calibrate::run_guard(&args[2..])` at `main.rs:196`), and the sub-subcommand match lives in `run_guard` at `guard_calibrate.rs:16`. So the change is:

1. In `core/src/bin/kastellan-cli/guard_calibrate.rs`, extend `run_guard`'s match:

```rust
    match args[0].as_str() {
        "calibrate" => run_guard_calibrate(&args[1..]),
        "capture" => crate::guard_capture::run(&args[1..]),
        other => {
            eprintln!("guard: unknown subcommand {other}");
            ExitCode::from(2)
        }
    }
```

   and widen its empty-args usage line (`guard_calibrate.rs:13`) to mention both subcommands.

2. In `core/src/bin/kastellan-cli/main.rs`, add `mod guard_capture;` beside the existing `mod guard_calibrate;` (`main.rs:164`), and add a usage line beside `main.rs:253`.

`main.rs:196` itself does **not** change.

- [ ] **Step 5: Run the tests to verify they pass**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test guard_capture_e2e 2>&1 | tail -20
cargo test -p kastellan-core --lib guard_calibration 2>&1 | tail -10
cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: all pass, clippy clean.

- [ ] **Step 6: Mutation-check the fail-closed arm**

Change `if !record { if let Some(reason) = verify_requirement(entry) {` to `if false {`. Expected: `an_unrecorded_entry_is_refused_in_verify_mode` still passes (it tests the predicate directly) **but** a manual run against an unrecorded manifest now silently materialises. Add an integration assertion covering the CLI arm if that gap bothers you — note it in the commit either way rather than leaving it unstated.

Restore by re-editing the file, **not** with `git checkout`.

- [ ] **Step 7: Commit**

```sh
git add core/src/bin/kastellan-cli/guard_capture.rs core/src/bin/kastellan-cli/main.rs core/src/guard_calibration/manifest.rs core/tests/guard_capture_e2e.rs
git commit -m "feat(cli): guard capture -- materialise through the real worker, verify the hash

Spec D3: capture drives the real web-fetch worker rather than curl, because
curl would miss extract_scannable_text's key-stripping and the SCAN_BYTE_CAP
truncation -- fitting tau against text the chokepoint never sees. The hash is
over the SCANNABLE text for the same reason, which also makes it stable
against JSON key order where a raw-bytes hash would not be.

One command records and verifies, so materialising and capturing cannot drift
apart. verify_requirement returns a REASON rather than a bool because the
operator action differs -- record it, versus investigate a drifted source --
and an unrecorded entry is a refusal, never a pass. The check runs before the
fetch so a manifest-wide omission costs no network round trips.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: The capture campaign and the τ fit (operator procedure, not code)

**Files:**
- Create: `tests/guard/manifest/*.json` (~85 entries)
- Create: `docs/devel/runbooks/2026-08-22-guard-calibration-campaign.md`
- Modify: `.gitignore` (add `tests/guard/corpus-materialised/`)

- [ ] **Step 1: Add the gitignore entry and commit it first**

Doing this before any capture run is what stops a materialised corpus being committed by accident.

```sh
echo "tests/guard/corpus-materialised/" >> .gitignore
git add .gitignore && git commit -m "chore: ignore the materialised guard corpus

Spec D1 -- the materialised directory holds third-party text and must never be
committed. Added BEFORE the first capture run, so the guard exists before the
thing it guards against."
```

- [ ] **Step 2: Provision the allowlist rows on the capture host**

Every domain in the URL list needs a `web-fetch` `tool_allowlists` row, **and the daemon must be restarted** — it loads the allowlist once at startup. Without this, every entry fails `-32001: host ... not on allowlist`, which is what all five of the deployed DGX's web-fetch attempts hit.

Entries are bare hosts, so they must be mapped through `allowlist_to_net_entries` — a bare host with no `:port` is an all-port grant, which is deliberate here but must be a conscious choice.

- [ ] **Step 3: Build the manifest**

Target counts from spec D5, all `provenance: "captured"`:

| stratum | label | target |
| --- | --- | --- |
| ordinary web content | benign | ~35 |
| security / technical prose (D4's expensive stratum) | benign | ~15 |
| third-party injection payloads via the fetch path | attack | ~35 |
| over-cap documents (≥64 KiB, counted within the above) | mixed | ≥8 |

Every `source` must be a Wayback snapshot URL or a HuggingFace URL pinned by revision hash. Label by spec D4: describing an attack is benign, directing one at the reader is attack.

- [ ] **Step 4: Record, then commit the hashes**

```sh
source "$HOME/.cargo/env"
./target/debug/kastellan-cli guard capture \
  --manifest tests/guard/manifest \
  --out tests/guard/corpus-materialised \
  --record
git add tests/guard/manifest && git commit -m "feat(guard-corpus): record the captured-half manifest hashes"
```

- [ ] **Step 5: Verify the round trip fails closed**

Re-run **without** `--record`; every entry must print `OK`. Then hand-edit one manifest hash and confirm the run reports `MISMATCH` and exits non-zero. Restore the hash afterwards.

- [ ] **Step 6: Fit τ on the DGX**

**Both corpora, not just the materialised one.** Fitting against
`tests/guard/corpus-materialised` alone makes D7's budget scope a **no-op**: every
materialised case is `captured`, so `OnlyProvenance(Captured)` and `AllBenign` are
identical, the per-stratum sections collapse to a single stratum, and D5's authored-24
stratum is absent from the report entirely — leaving the whole scope mechanism untested in
the one run that matters. Copy the authored cases in first:

```sh
source "$HOME/.cargo/env"
cp tests/guard/corpus/*.json tests/guard/corpus-materialised/
KASTELLAN_LLM_GUARD_URL=http://127.0.0.1:8081/v1 \
KASTELLAN_LLM_GUARD_MODEL=shieldstral \
./target/debug/kastellan-cli guard calibrate \
  --corpus tests/guard/corpus-materialised | tee docs/devel/runbooks/guard-calibration-dgx.txt
```

Verify the report shows **more than one provenance section** before trusting the operating
point; a single-stratum report means the copy did not happen.

**A run with any `Unmeasured` case is invalid, not merely poor** — so is a captured stratum under 50 cases, or a τ at a boundary of the swept range. Read the `operating point (FP budget 1)` line, not just the margin-maximising τ.

- [ ] **Step 7: Fit τ on the Mac and compare**

**Blocked on [#592](https://github.com/hherb/kastellan/issues/592)'s durable half** — pin the weights' sha256 in-repo and check it at use before running this. Both hosts hold the verified file (`35b755be…`) as of 2026-08-22, but nothing yet *enforces* that, and D6's comparison is only meaningful if the weights are known-identical. Run the identical command against a Mac `llama-server` on the same weights, then compare the two operating points.

If they disagree, that is a **finding about the cross-platform claim**, not a discrepancy to average away.

- [ ] **Step 8: Write the runbook and commit the reports**

Record in `docs/devel/runbooks/2026-08-22-guard-calibration-campaign.md`: the host, the weights sha256, the `policy_digest`, the corpus counts per stratum, both operating points, and the fitted τ. Then update the wiring spec's D1 to name the fitted value and remove "this slice ships a tier nobody should turn on yet".

---

## Self-review

**Spec coverage.** D1 → Task 3 + Task 5 Step 1. D2 → Task 3 (the `source` doc + README) and Task 4 (mismatch is fatal). D3 → Task 4's `fetch_through_worker` + Task 5 Step 2. D4 → Task 3's README + Task 5 Step 3. D5 → Task 5 Step 3's table. D6 → Task 5 Step 7. D7 → Tasks 1 and 2. F1's allowlist prerequisite → Task 5 Step 2. F2 (oMLX unusable) → Task 5 Step 7 pins llama.cpp. F4/#592 → Task 5 Step 7 is explicitly blocked on it.

**Known weak point, stated rather than hidden.** Task 4's `fetch_through_worker` depends on a `dispatch_one_shot` helper that does not exist, and the plan tells the implementer to lift it from `web_fetch_e2e.rs` and to **stop and split** if it exceeds ~40 lines. That is the least-specified step in this plan and the most likely to need a judgement call.

**Type consistency.** `OperatingPoint { tau, confusion }` is used identically in Tasks 1 and 2. `verify_requirement` returns `Option<String>` in both Task 3's implementation and Task 4's test. `ManifestEntry` fields match between the struct, the README, and the CLI's `serde_json::json!` output.
