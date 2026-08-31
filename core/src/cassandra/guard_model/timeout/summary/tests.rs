//! Unit tests for folding a run of samples into one number (issue #624).
//!
//! Moved here with the production code when `sample.rs` was split at its
//! own `#624` divider, so each file carries the tests that name it.
//! Pure throughout — no server, no clock, no Postgres.

use super::super::sample::{ProbeOutcome, PROBE_BUDGET_MS};
use super::*;

/// The contended DGX boot, as three samples.
///
/// Not invented: these are the three `tok_per_s` figures one unchanged
/// DGX backend produced on three consecutive boots (6 073 / 269.6 /
/// 1 582), expressed at the probe's own 810-token sample size. The
/// backend's reproducible uncontended rate, measured directly minutes
/// later, was ~7 000 — so the fastest of the three is the one that is
/// about the host.
fn contended_dgx_samples() -> Vec<ProbeOutcome> {
    vec![
        ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 133 }, // ~6 090 tok/s
        ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 3_004 }, // ~270 tok/s
        ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 512 }, // ~1 582 tok/s
    ]
}

/// The fastest sample wins, because contention only ever slows one down.
///
/// This is the whole of #624. Taking the first sample (or the last, or a
/// mean) lets daemon-startup contention set a security control's budget:
/// the 270 tok/s reading derives ~489 s, clamps to the 120 s ceiling and
/// fires the "this host cannot adjudicate a worst-case document"
/// finding, on a host that adjudicates one in ~19 s.
#[test]
fn the_fastest_sample_wins_because_contention_only_slows() {
    let s = summarise(&contended_dgx_samples());
    assert_eq!(
        s.best,
        ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 133 },
        "the fastest sample is the one measuring the HOST; the others measure the load"
    );
    assert_eq!(s.measured_samples, 3);
    let slowest = s.slowest_tok_per_s.expect("three samples measured");
    assert!((slowest - 269.6).abs() < 1.0, "slowest should be the 270 tok/s boot, got {slowest}");
}

/// A mean would not do: it is dragged by the contended samples.
///
/// Stated as a test because "just average them" is the obvious
/// alternative, and it is wrong for a one-sided error. The mean of the
/// three real DGX rates is ~2 647 tok/s — still 2.6x below the host's
/// measured ~7 000, and still enough to derive a budget shaped by how
/// busy the boot was rather than by the machine.
#[test]
fn the_summary_is_not_a_mean_of_the_samples() {
    let s = summarise(&contended_dgx_samples());
    let best = sample_tok_per_s(&s.best).expect("a measured best has a rate");
    let mean: f64 = contended_dgx_samples().iter().filter_map(sample_tok_per_s).sum::<f64>() / 3.0;
    assert!(mean < 3_000.0, "sanity: the mean really is dragged down, got {mean}");
    assert!(best > mean * 2.0, "the summary must not be pulled toward the contended samples");
}

/// One sample reports itself as its own slowest, and says so honestly.
#[test]
fn a_single_measuring_sample_reports_no_spread_rather_than_a_fake_one() {
    let s = summarise(&[ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 159 }]);
    assert_eq!(s.measured_samples, 1);
    let fastest = sample_tok_per_s(&s.best).expect("measured");
    let slowest = f64::from(s.slowest_tok_per_s.expect("one sample still has a rate"));
    assert!((fastest - slowest).abs() < 1.0, "one sample observed one rate");
}

/// A real measurement beats a saturated sample — a cold model warming up
/// must not set the budget for the host it warmed up on.
///
/// `Saturated` takes the CEILING and fires a coverage finding, so this
/// rung decides whether one 20 s stall on an otherwise fast host
/// announces that the host cannot screen. It must not.
///
/// **What this does NOT cover**, stated rather than implied by the
/// name: it cannot tell `informativeness`'s ranking apart from
/// `summarise`'s rate tie-break, because only a measuring sample has a
/// rate and the tie-break alone would produce this same result. The
/// ranking is pinned separately by
/// [`the_informativeness_ranking_is_strictly_ordered`].
#[test]
fn one_saturated_sample_does_not_outrank_a_real_measurement() {
    let s = summarise(&[
        ProbeOutcome::Saturated { budget_ms: PROBE_BUDGET_MS },
        ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 159 },
    ]);
    assert_eq!(s.best, ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 159 });
    assert_eq!(s.measured_samples, 1);
}

/// With nothing measured, the ranking of the failures decides which
/// finding fires — and every rung of it is pinned here.
///
/// All three lower outcomes derive the same floor, so this ordering
/// changes exactly one observable thing: whether the boot warns that
/// every dispatch is likely to fail open.
#[test]
fn with_no_measurement_the_most_informative_failure_wins() {
    let saturated = ProbeOutcome::Saturated { budget_ms: PROBE_BUDGET_MS };
    let failed = ProbeOutcome::Failed { why: "connection refused".to_string() };
    let thin = ProbeOutcome::TooFewUncachedTokens { uncached_tokens: 1, elapsed_ms: 38 };
    let none = ProbeOutcome::NoTokenCount;

    // Saturated outranks every failure: it is the only one that bounds
    // throughput, and the only one that takes the ceiling.
    assert_eq!(summarise(&[failed.clone(), saturated.clone()]).best, saturated);
    assert_eq!(summarise(&[thin.clone(), saturated.clone()]).best, saturated);

    // A failed CALL outranks a merely-unusable SAMPLE: the first is a
    // fact about the backend, the second about the measurement.
    assert_eq!(summarise(&[thin.clone(), failed.clone()]).best, failed);
    assert_eq!(summarise(&[none.clone(), failed.clone()]).best, failed);

    // ...and a thin sample still outranks no token count at all, which
    // carries no numbers whatever.
    assert_eq!(summarise(&[none, thin.clone()]).best, thin);

    // None of them measured anything, so none of them reports a rate.
    assert_eq!(summarise(&[failed, saturated, thin]).slowest_tok_per_s, None);
}

/// An empty run is total rather than a panic.
///
/// Unreachable through [`more_samples_wanted`], which always grants a
/// first sample — and answered anyway, because this is a security
/// control and "unreachable" is a property of a different function.
#[test]
fn no_samples_at_all_is_answered_rather_than_panicked() {
    let s = summarise(&[]);
    assert_eq!(s.best, ProbeOutcome::NoTokenCount);
    assert_eq!(s.measured_samples, 0);
    assert_eq!(s.slowest_tok_per_s, None);
}

/// Only measuring samples contribute a rate.
///
/// `Saturated` bounds throughput from above but measures none, so
/// counting it would put a fabricated number in `measured_samples` and
/// a wrong one in `slowest_tok_per_s`.
#[test]
fn a_saturated_sample_contributes_no_rate_to_the_spread() {
    assert_eq!(sample_tok_per_s(&ProbeOutcome::Saturated { budget_ms: PROBE_BUDGET_MS }), None);
    assert_eq!(sample_tok_per_s(&ProbeOutcome::NoTokenCount), None);
    assert_eq!(
        sample_tok_per_s(&ProbeOutcome::TooFewUncachedTokens { uncached_tokens: 1, elapsed_ms: 38 }),
        None
    );
    // `Measured` is never BUILT with a zero wall clock (`probe_sample`
    // rejects it), but `ProbeOutcome` is public and this function divides
    // by the value. Without this the guard is unreached, and deleting it
    // puts `slowest_tok_per_s: inf` -- which serialises to `null` -- in a
    // durable boot row.
    assert_eq!(
        sample_tok_per_s(&ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 0 }),
        None,
        "a zero wall clock is not a throughput of infinity"
    );
    let s = summarise(&[
        ProbeOutcome::Saturated { budget_ms: PROBE_BUDGET_MS },
        ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 159 },
        ProbeOutcome::NoTokenCount,
    ]);
    assert_eq!(s.measured_samples, 1, "only the one Measured sample has a rate");
}

/// Every sample of one boot sends a DIFFERENT prompt.
///
/// The defect this prevents is not cosmetic. Identical prompts are
/// served from the prefix cache, and on a backend that reports
/// `cached_tokens` the repeats collapse to `TooFewUncachedTokens` — so
/// the multi-sample probe silently becomes a single-sample one again.
/// On a backend that does NOT report it, they read as enormous
/// throughputs instead, and `summarise` prefers the FASTEST sample: the
/// probe would pick the most cache-contaminated reading and derive a
/// timeout several times too short, which fails OPEN.
#[test]
fn each_sample_of_a_boot_sends_a_different_document() {
    let docs: Vec<String> = (0..PROBE_SAMPLES)
        .map(|i| probe_document(&sample_cache_buster("kastellan-guard-probe-1724371200", i)))
        .collect();
    let distinct: std::collections::BTreeSet<&String> = docs.iter().collect();
    assert_eq!(distinct.len(), PROBE_SAMPLES, "every sample must be cold: {docs:?}");
    // ...and they diverge as early as the document allows, so the
    // cacheable common prefix is as short as possible.
    assert!(docs[0].starts_with('0') && docs[1].starts_with('1'));
}

/// The summary counts what it TOOK, not only what it could use.
///
/// `measured_samples` alone reads three ways at once — one sample that
/// worked, three with two served from cache, three with two failed calls
/// — and they call for opposite actions. The denominator is what makes
/// `attempted > measured` a query rather than a guess, and #625's review
/// found it unwritable.
#[test]
fn the_summary_counts_the_samples_it_took_not_only_the_usable_ones() {
    let s = summarise(&[
        ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 159 },
        ProbeOutcome::Failed { why: "connection refused".to_string() },
        ProbeOutcome::TooFewUncachedTokens { uncached_tokens: 1, elapsed_ms: 38 },
    ]);
    assert_eq!(s.attempted_samples, 3, "three calls were made to the backend");
    assert_eq!(s.measured_samples, 1, "only one of them measured anything");
    assert!(
        s.attempted_samples > s.measured_samples,
        "which is exactly the row that says: read that boot's warn! lines"
    );

    // A run that stopped early reports the smaller number honestly rather
    // than PROBE_SAMPLES.
    assert_eq!(summarise(&[ProbeOutcome::Saturated { budget_ms: PROBE_BUDGET_MS }])
        .attempted_samples, 1);
    assert_eq!(summarise(&[]).attempted_samples, 0);
}

/// Two boots differ too, so the cache is defeated across restarts as
/// well as within one probe.
#[test]
fn the_same_sample_index_of_two_boots_still_differs() {
    assert_ne!(sample_cache_buster("boot-a", 0), sample_cache_buster("boot-b", 0));
}

/// The stopping rule: [`PROBE_SAMPLES`] samples, or
/// [`PROBE_TOTAL_BUDGET_MS`] of wall clock, whichever comes first.
#[test]
fn the_probe_stops_at_the_sample_count_or_the_total_budget() {
    assert!(more_samples_wanted(0, 0), "a probe must always get at least one sample");
    assert!(more_samples_wanted(PROBE_SAMPLES - 1, 0), "the last sample is still wanted");
    assert!(!more_samples_wanted(PROBE_SAMPLES, 0), "never more than PROBE_SAMPLES");

    // The budget bound is exclusive, so a run that has spent exactly the
    // whole total ends the probe.
    assert!(more_samples_wanted(1, PROBE_TOTAL_BUDGET_MS - 1));
    assert!(!more_samples_wanted(1, PROBE_TOTAL_BUDGET_MS));
}

/// A saturating FIRST sample buys a SECOND one (issue [#626]).
///
/// The whole of #626, expressed as the one call that used to answer
/// `false`. A [`ProbeOutcome::Saturated`] means the per-request budget
/// expired, so the run stands at exactly [`PROBE_BUDGET_MS`] — and while
/// [`PROBE_TOTAL_BUDGET_MS`] *equalled* that, the elapsed check ended the
/// probe there. A cold `llama-server` paging in its weights therefore
/// derived the ceiling and fired
/// [`super::super::TimeoutBasis::Saturated`]'s finding on a host that
/// adjudicates a document in ~19 s a moment later, with the fast samples
/// that would have contradicted it never taken.
///
/// **The retry is what makes the finding evidence rather than noise.**
/// [`summarise`] ranks `Measured` above `Saturated`, so one fast follow-up
/// sample replaces the ceiling with a real budget; if the host really is
/// slow, the second sample saturates too and the finding fires on
/// `attempted_samples: 2`.
///
/// Written as its own test rather than a line in the stopping-rule test
/// above because it is a different claim: that one pins the *rule*, this
/// one pins the *consequence* the rule exists for.
///
/// [#626]: https://github.com/hherb/kastellan/issues/626
#[test]
fn a_saturating_first_sample_still_buys_another() {
    assert!(
        more_samples_wanted(1, PROBE_BUDGET_MS),
        "a probe whose first sample saturated must take another: one stalled call \
         is a cold model as often as it is a slow host, and stopping there fires \
         the ceiling finding on evidence of one"
    );
}

/// The probe's total budget must be STRICTLY longer than one sample's.
///
/// A `const` assertion rather than a test body: the relation is between
/// two constants, so it can be a **compile** error.
///
/// **Strictly, not `>=`, and that is issue [#626]** — the two were equal,
/// which made `more_samples_wanted` unable to grant a second sample after
/// a saturating first one no matter what it was asked. Equality is
/// therefore not a tunable choice to be re-made later but the defect
/// itself, and re-making it should not compile.
///
/// [#626]: https://github.com/hherb/kastellan/issues/626
const _: () = assert!(
    PROBE_TOTAL_BUDGET_MS > PROBE_BUDGET_MS,
    "PROBE_TOTAL_BUDGET_MS must EXCEED one sample's budget, or a saturating \
     first sample ends the probe at one unrepresentative measurement (#626)"
);

/// Both boot-cost knobs, pinned to literals: three samples, and a total
/// budget of two per-sample budgets.
///
/// Every other test here expresses itself in terms of [`PROBE_SAMPLES`]
/// and [`PROBE_TOTAL_BUDGET_MS`], so changing either would move the
/// assertions along with it and nothing would fail. Both are boot-time
/// costs as well as measurement-quality knobs, so a change to either
/// should be a visible diff here.
///
/// **The total is asserted as `40_000`, not as `2 * PROBE_BUDGET_MS`**,
/// and the literal is the point: writing the relation would make this
/// test move with the definition it exists to pin. The `const _` above
/// covers the *relation* (strictly greater); this covers the *value*, so
/// changing the factor to 3x is a failing test rather than a silent 20 s
/// of extra worst-case daemon startup.
#[test]
fn the_sample_count_is_pinned_to_its_documented_value() {
    assert_eq!(PROBE_SAMPLES, 3);
    assert_eq!(PROBE_TOTAL_BUDGET_MS, 40_000);
}

/// The ranking table itself, asked directly rather than through
/// [`summarise`].
///
/// **Written because a mutation survived.** Collapsing `Saturated`'s
/// rank to `Measured`'s changed nothing observable in `summarise`:
/// only a measuring sample has a rate, so the tie-break already puts
/// every non-measuring outcome at negative infinity and `Measured` wins
/// that rung whatever its rank says. The rung is real, it is simply
/// held twice — and a test named
/// `one_saturated_sample_does_not_outrank_a_real_measurement` implies
/// coverage the tie-break was actually providing.
///
/// So this asks [`informativeness`] itself. It is the only assertion
/// here that can see the ranking as distinct from the tie-break, and it
/// is what makes the other three rungs — which `summarise` genuinely
/// does decide — a table rather than four coincidences.
#[test]
fn the_informativeness_ranking_is_strictly_ordered() {
    let ranked = [
        ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 159 },
        ProbeOutcome::Saturated { budget_ms: PROBE_BUDGET_MS },
        ProbeOutcome::Failed { why: "refused".to_string() },
        ProbeOutcome::TooFewUncachedTokens { uncached_tokens: 1, elapsed_ms: 38 },
        ProbeOutcome::NoTokenCount,
    ];
    for pair in ranked.windows(2) {
        assert!(
            informativeness(&pair[0]) > informativeness(&pair[1]),
            "{:?} must outrank {:?}",
            pair[0],
            pair[1]
        );
    }
}
