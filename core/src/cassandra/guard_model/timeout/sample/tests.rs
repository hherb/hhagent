//! Unit tests for one probe sample (wiring-spec D9).
//!
//! Pure throughout — no server, no clock, no Postgres. Lifted out of
//! `timeout/tests.rs` unchanged when `timeout.rs` was split, so that
//! each production file carries the tests that name it. Living here is
//! also what gives them the private [`PROBE_BODY`] without widening its
//! visibility for a test's convenience.

use std::time::Duration;

// The few assertions below that cross into the derivation: a rejected
// sample must still produce a finite budget, and must never produce a
// more permissive one than a real measurement.
use super::super::{derive_guard_timeout, GuardTimeout, TIMEOUT_FLOOR_MS};
use super::*;

/// Derive from a single sample, exactly as a one-sample run does.
///
/// Goes through [`summarise`] rather than hand-building a
/// [`ProbeSummary`], so these tests keep exercising the real path.
fn derive_from_one(outcome: &ProbeOutcome) -> GuardTimeout {
    derive_guard_timeout(&summarise(std::slice::from_ref(outcome)))
}

// ── probe_sample: turning a raw reading into an outcome ──────────────

/// M2 row 3, the contaminated repeat, is the fixture that kills the
/// "forget to subtract `cached_tokens`" mutation.
///
/// 810 prompt tokens of which 809 were cached, in 38 ms. Subtracting
/// gives 1 uncached token, which the floor rejects. NOT subtracting
/// gives 810 / 0.038 = **21,316 tok/s** — a 4x over-estimate of a
/// server measured at ~5,000, which derives a timeout 4x too short and
/// so converts real adjudications into fail-open timeouts.
#[test]
fn a_cache_contaminated_sample_is_rejected_rather_than_believed() {
    let outcome = probe_sample(ProbeReading {
        prompt_tokens: Some(810),
        cached_tokens: Some(809),
        elapsed_ms: 38,
    });
    assert_eq!(
        outcome,
        ProbeOutcome::TooFewUncachedTokens { uncached_tokens: 1, elapsed_ms: 38 },
        "cached tokens were never processed and must not count as throughput"
    );

    // And the consequence, end to end: the contaminated reading must
    // not derive a shorter timeout than the honest one. M2's cold
    // sibling reading is the honest one; it is a local literal here
    // rather than the parent's `DGX_MEASURED` fixture, which this
    // module cannot reach.
    let contaminated = derive_from_one(&outcome);
    let honest = derive_from_one(&ProbeOutcome::Measured {
        uncached_tokens: 810,
        elapsed_ms: 159,
    });
    assert!(
        contaminated.timeout <= honest.timeout,
        "a rejected sample must never produce a MORE permissive timeout \
         than a real measurement ({:?} vs {:?})",
        contaminated.timeout,
        honest.timeout
    );
}

/// The cold reading M2 actually took is accepted in full.
#[test]
fn a_cold_reading_is_measured_with_every_token_counted() {
    let outcome = probe_sample(ProbeReading {
        prompt_tokens: Some(810),
        cached_tokens: Some(0),
        elapsed_ms: 159,
    });
    assert_eq!(outcome, ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 159 });
}

/// A backend that reports no cache block at all is treated as
/// zero-cached — which is only safe because the uncached floor still
/// applies to the result.
#[test]
fn an_absent_cache_block_counts_no_tokens_as_cached() {
    let outcome = probe_sample(ProbeReading {
        prompt_tokens: Some(810),
        cached_tokens: None,
        elapsed_ms: 159,
    });
    assert_eq!(outcome, ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 159 });
}

#[test]
fn no_prompt_token_count_is_its_own_outcome() {
    assert_eq!(
        probe_sample(ProbeReading {
            prompt_tokens: None,
            cached_tokens: None,
            elapsed_ms: 159
        }),
        ProbeOutcome::NoTokenCount
    );
    // Even with a cache count present — the numerator is what is
    // missing, and a cache count alone cannot supply it.
    assert_eq!(
        probe_sample(ProbeReading {
            prompt_tokens: None,
            cached_tokens: Some(0),
            elapsed_ms: 159
        }),
        ProbeOutcome::NoTokenCount
    );
}

/// A zero wall clock is rejected rather than divided by.
#[test]
fn a_zero_elapsed_reading_is_rejected_not_divided_by() {
    let outcome = probe_sample(ProbeReading {
        prompt_tokens: Some(810),
        cached_tokens: Some(0),
        elapsed_ms: 0,
    });
    assert_eq!(
        outcome,
        ProbeOutcome::TooFewUncachedTokens { uncached_tokens: 810, elapsed_ms: 0 }
    );
    // And the derivation stays finite.
    let t = derive_from_one(&outcome);
    assert_eq!(t.timeout, Duration::from_millis(TIMEOUT_FLOOR_MS));
}

/// A backend reporting more cached than prompt tokens saturates to
/// zero rather than wrapping to four billion.
#[test]
fn more_cached_than_prompt_tokens_saturates_to_zero() {
    assert_eq!(
        probe_sample(ProbeReading {
            prompt_tokens: Some(10),
            cached_tokens: Some(999),
            elapsed_ms: 5
        }),
        ProbeOutcome::TooFewUncachedTokens { uncached_tokens: 0, elapsed_ms: 5 }
    );
}

/// The floor itself is the boundary, and it is inclusive-accepting:
/// exactly `MIN_UNCACHED_PROBE_TOKENS` is enough.
#[test]
fn the_uncached_token_floor_accepts_exactly_the_minimum() {
    let at = probe_sample(ProbeReading {
        prompt_tokens: Some(MIN_UNCACHED_PROBE_TOKENS),
        cached_tokens: Some(0),
        elapsed_ms: 50,
    });
    assert!(
        matches!(at, ProbeOutcome::Measured { .. }),
        "exactly the minimum must be usable, got {at:?}"
    );
    let below = probe_sample(ProbeReading {
        prompt_tokens: Some(MIN_UNCACHED_PROBE_TOKENS - 1),
        cached_tokens: Some(0),
        elapsed_ms: 50,
    });
    assert!(
        matches!(below, ProbeOutcome::TooFewUncachedTokens { .. }),
        "one below the minimum must be rejected, got {below:?}"
    );
}

// ── the probe document ──────────────────────────────────────────────

/// The varying part goes FIRST. A prefix cache matches from position
/// 0, so putting it at the end would leave the body cached and
/// reproduce the 4x over-estimate this whole module is built to avoid.
#[test]
fn the_probe_document_leads_with_the_cache_buster() {
    let doc = probe_document("boot-1724371200");
    assert!(
        doc.starts_with("boot-1724371200"),
        "the cache-buster must be a PREFIX; a suffix leaves the body cacheable"
    );
    assert!(doc.contains(PROBE_BODY), "the constant body must be present in full");
    // Two boots differ from the first byte.
    assert_ne!(probe_document("a"), probe_document("b"));
}

/// The probe is dense enough to stand in for a worst-case document,
/// and big enough to clear the uncached-token floor.
///
/// M2 measured this body at 810 tokens for 1024 bytes (1.26
/// bytes/token). A prose probe would measure ~6.5 and over-estimate
/// throughput per byte by ~5x — exactly the error D2 made.
#[test]
fn the_probe_body_is_the_measured_size_and_carries_no_prose() {
    assert_eq!(
        PROBE_BODY.len(),
        PROBE_BYTES,
        "M2's 810-token measurement was taken at {PROBE_BYTES} bytes"
    );
    assert!(
        !PROBE_BODY.contains(' '),
        "spaces make text mergeable; the probe must stay token-dense"
    );
    // At M2's measured 1.26 bytes/token this clears the floor with
    // room to spare, which is what makes a cache hit detectable rather
    // than merely unlikely.
    let expected_tokens = (PROBE_BYTES as f32 / 1.26) as u32;
    assert!(
        expected_tokens > MIN_UNCACHED_PROBE_TOKENS * 2,
        "the probe must clear the uncached floor comfortably, got ~{expected_tokens}"
    );
}

// ── #624: many samples, one number ───────────────────────────────────

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

    // The budget bound is exclusive, so a sample that spent exactly the
    // whole budget ends the probe — which is what makes a saturating
    // FIRST sample cost exactly one budget with no special case.
    assert!(more_samples_wanted(1, PROBE_TOTAL_BUDGET_MS - 1));
    assert!(!more_samples_wanted(1, PROBE_TOTAL_BUDGET_MS));
    assert!(!more_samples_wanted(1, PROBE_BUDGET_MS), "a saturated first sample stops here");
}

/// The probe's total budget must not be shorter than one sample's, or
/// the second sample could never be reached on a host that needs it.
///
/// A `const` assertion rather than a test body: the relation is between
/// two constants, so it can be a **compile** error.
const _: () = assert!(
    PROBE_TOTAL_BUDGET_MS >= PROBE_BUDGET_MS,
    "PROBE_TOTAL_BUDGET_MS must be at least one sample's budget"
);

/// Three samples, pinned to a literal.
///
/// Every other test here expresses itself in terms of `PROBE_SAMPLES`,
/// so changing it would move the assertions along with it and nothing
/// would fail. It is a boot-time cost as well as a measurement-quality
/// knob, so a change to it should be a visible diff here.
#[test]
fn the_sample_count_is_pinned_to_its_documented_value() {
    assert_eq!(PROBE_SAMPLES, 3);
    assert_eq!(PROBE_TOTAL_BUDGET_MS, 20_000);
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
