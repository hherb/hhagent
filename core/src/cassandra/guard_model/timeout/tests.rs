//! Unit tests for the guard timeout derivation (wiring-spec D9).
//!
//! Pure throughout — no server, no clock, no Postgres. Two fixtures do
//! most of the work and both are real measurements rather than
//! invented numbers: the DGX's M2 sample, and the Mac throughput
//! measurement 3 implies.

use super::*;

/// M2, 2026-08-23, DGX: 810 uncached tokens in 159.3 ms.
///
/// The cold sample. Its sibling (164.1 ms) agrees within 3%, and both
/// land inside measurement 1's independently taken 4,039-6,660 tok/s
/// band — which is why this number is trusted as a fixture rather than
/// treated as one run's noise.
const DGX_MEASURED: ProbeOutcome =
    ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 159 };

/// Measurement 3's Mac: ~5.5 minutes for a 44,437-token document,
/// i.e. ~135 tok/s. Expressed as a probe-sized sample at the same rate.
const MAC_MEASURED: ProbeOutcome =
    ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 6_000 };

fn tok_per_s_of(t: &GuardTimeout) -> f32 {
    match t.basis {
        TimeoutBasis::Probed { tok_per_s, .. } => tok_per_s,
        ref other => panic!("expected a probed basis, got {other:?}"),
    }
}

fn clamped_of(t: &GuardTimeout) -> Clamped {
    match t.basis {
        TimeoutBasis::Probed { clamped, .. } => clamped,
        ref other => panic!("expected a probed basis, got {other:?}"),
    }
}

/// The worst case the timeout budgets for is the same figure D8
/// refuses to boot below.
///
/// Budgeting for anything smaller would leave a document the server
/// accepts and the timeout does not — a fail-open reachable by an
/// attacker who controls document length.
#[test]
fn the_budgeted_worst_case_is_the_required_context() {
    assert_eq!(WORST_CASE_TOKENS, REQUIRED_GUARD_N_CTX);
}

/// The DGX derives ~26 s and is NOT clamped in either direction.
///
/// The arithmetic, so a reader can check it: 810 / 0.159 = 5,094 tok/s;
/// 66,048 / 5,094 = 12.97 s; x2 for the contention factor = ~25.9 s,
/// inside [15 s, 120 s].
#[test]
fn the_dgx_derives_about_twenty_six_seconds_unclamped() {
    let t = derive_guard_timeout(&DGX_MEASURED);
    assert_eq!(clamped_of(&t), Clamped::No, "26 s is inside the band");
    let rate = tok_per_s_of(&t);
    assert!(
        (4_900.0..5_300.0).contains(&rate),
        "must reproduce M2's ~5,000 tok/s, got {rate}"
    );
    let ms = t.timeout.as_millis() as u64;
    assert!(
        (24_000..28_000).contains(&ms),
        "expected ~26 s from M2's throughput, got {ms} ms"
    );
}

/// The Mac clamps to the CEILING, and the clamp is asserted by basis,
/// not only by value.
///
/// Value alone is not enough: a broken implementation that returned the
/// ceiling for everything would satisfy an `as_millis() == 120_000`
/// assertion while reporting the wrong reason. `Clamped::ToCeiling` is
/// the fact an operator needs — this host will fail open on large
/// documents.
#[test]
fn a_slow_host_clamps_to_the_ceiling_and_says_so() {
    let t = derive_guard_timeout(&MAC_MEASURED);
    assert_eq!(t.timeout, Duration::from_millis(TIMEOUT_CEILING_MS));
    assert_eq!(clamped_of(&t), Clamped::ToCeiling);
    assert!(
        t.basis.coverage_finding().is_some(),
        "a ceiling clamp is a finding about the host, not a routine value"
    );
    // The derived figure is retained even though it was clamped away,
    // so the boot line can say how far past the budget this host is.
    match t.basis {
        TimeoutBasis::Probed { derived_ms, .. } => assert!(
            derived_ms > TIMEOUT_CEILING_MS,
            "the pre-clamp derivation must be kept, got {derived_ms}"
        ),
        ref other => panic!("expected Probed, got {other:?}"),
    }
}

/// A very fast host clamps to the floor, and that is unremarkable.
#[test]
fn a_very_fast_host_clamps_to_the_floor_without_it_being_a_finding() {
    // 810 tokens in 1 ms = 810,000 tok/s; 66,048 / 810,000 * 2000 = 163 ms.
    let t = derive_guard_timeout(&ProbeOutcome::Measured {
        uncached_tokens: 810,
        elapsed_ms: 1,
    });
    assert_eq!(t.timeout, Duration::from_millis(TIMEOUT_FLOOR_MS));
    assert_eq!(clamped_of(&t), Clamped::ToFloor);
    assert!(
        t.basis.coverage_finding().is_none(),
        "a floor clamp costs no coverage -- warning about it buries the one that does"
    );
}

/// `Saturated` derives the CEILING.
///
/// The row a plausible implementation gets backwards. A probe that
/// overran its budget is an upper bound on throughput — the only
/// outcome that says the host is slow — so sending it to the floor
/// would give the slowest hosts the shortest timeout.
#[test]
fn a_saturated_probe_derives_the_ceiling_and_never_the_floor() {
    let t = derive_guard_timeout(&ProbeOutcome::Saturated { budget_ms: PROBE_BUDGET_MS });
    assert_eq!(
        t.timeout,
        Duration::from_millis(TIMEOUT_CEILING_MS),
        "overrunning the probe budget is evidence of slowness, not absence of evidence"
    );
    assert!(t.basis.coverage_finding().is_some());
    assert_ne!(
        t.timeout,
        Duration::from_millis(TIMEOUT_FLOOR_MS),
        "the floor would be exactly inverted"
    );
    assert_eq!(t.basis, TimeoutBasis::Saturated { budget_ms: PROBE_BUDGET_MS });
}

/// A saturated probe reports **no throughput and no derivation**, and
/// that absence is the assertion.
///
/// It used to be reported as `Probed`, which forced two fabrications:
/// a `tok_per_s` computed from `MIN_UNCACHED_PROBE_TOKENS` — a
/// sample-rejection floor, not a count of anything the probe processed,
/// giving 12.8 tok/s against a real upper bound of ~40 — and a
/// `derived_ms` holding the POST-clamp ceiling while the `Probed` arm's
/// `derived_ms` is the PRE-clamp derivation (pinned above by
/// `derived_ms > TIMEOUT_CEILING_MS`). One field, two meanings, and
/// `main.rs` writes the rate into `policy / guard_tier.boot` calling it
/// the number needed to re-derive the timeout — re-deriving from 12.8
/// yields ~10.3 million ms against a recorded 120,000.
#[test]
fn a_saturated_probe_claims_no_measurement_it_did_not_make() {
    let t = derive_guard_timeout(&ProbeOutcome::Saturated { budget_ms: PROBE_BUDGET_MS });
    assert!(
        !matches!(t.basis, TimeoutBasis::Probed { .. }),
        "a probe that never returned a sample must not report one: {:?}",
        t.basis
    );
    assert_eq!(
        t.basis.kind(),
        "probe-saturated",
        "`probed` would spell a budget overrun the same way as a real measurement"
    );
}

/// Every outcome that measured nothing takes the floor, and each names
/// itself so a boot line can say which.
#[test]
fn every_unmeasuring_outcome_takes_the_floor_with_a_distinct_reason() {
    let cases = [
        ProbeOutcome::NoTokenCount,
        ProbeOutcome::TooFewUncachedTokens { uncached_tokens: 1, elapsed_ms: 38 },
        ProbeOutcome::Failed { why: "connection refused".to_string() },
    ];
    let mut seen = std::collections::BTreeSet::new();
    for outcome in &cases {
        let t = derive_guard_timeout(outcome);
        assert_eq!(
            t.timeout,
            Duration::from_millis(TIMEOUT_FLOOR_MS),
            "{outcome:?} must take the floor"
        );
        assert!(
            matches!(t.basis, TimeoutBasis::Unprobed { .. }),
            "{outcome:?} must not claim to have probed"
        );
        // `Failed` IS a coverage finding -- /props answered, so the call
        // that just failed is the call every dispatch will make. The
        // other two say nothing about coverage.
        assert_eq!(
            t.basis.coverage_finding().is_some(),
            matches!(outcome, ProbeOutcome::Failed { .. }),
            "{outcome:?} reports the wrong coverage verdict"
        );
        assert!(
            seen.insert(t.basis.kind()),
            "{outcome:?} reuses a reason token already seen: {:?}",
            t.basis.kind()
        );
    }
}

/// The two directions a failed probe can take, and they are NOT
/// symmetric.
///
/// A timeout is an upper bound on throughput — the only failure that
/// says something about the host — so it must reach the ceiling. Any
/// other failure knows nothing and takes the floor. Getting this
/// backwards hands the slowest hosts the shortest guard timeout, which
/// is a fail-open that shows up as nothing at all.
#[test]
fn a_timed_out_probe_saturates_while_any_other_failure_is_merely_failed() {
    assert_eq!(
        probe_error_outcome(true, "irrelevant".to_string(), PROBE_BUDGET_MS),
        ProbeOutcome::Saturated { budget_ms: PROBE_BUDGET_MS },
        "a timeout is evidence of slowness and must reach the ceiling"
    );
    assert_eq!(
        probe_error_outcome(false, "connection refused".to_string(), PROBE_BUDGET_MS),
        ProbeOutcome::Failed { why: "connection refused".to_string() },
        "a connection failure says nothing about throughput"
    );

    // And the consequence the split exists for, asserted end to end.
    let timed_out = derive_guard_timeout(&probe_error_outcome(true, String::new(), PROBE_BUDGET_MS));
    let refused =
        derive_guard_timeout(&probe_error_outcome(false, "refused".to_string(), PROBE_BUDGET_MS));
    assert_eq!(timed_out.timeout, Duration::from_millis(TIMEOUT_CEILING_MS));
    assert_eq!(refused.timeout, Duration::from_millis(TIMEOUT_FLOOR_MS));
    assert!(
        timed_out.timeout > refused.timeout,
        "a slow host must end up with a LONGER budget than an unreachable one"
    );
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
    // not derive a shorter timeout than the honest one.
    let contaminated = derive_guard_timeout(&outcome);
    let honest = derive_guard_timeout(&DGX_MEASURED);
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
    let t = derive_guard_timeout(&outcome);
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

// ── the operator override ───────────────────────────────────────────

/// A pinned timeout is honoured verbatim and reports itself as the
/// operator's, not as a measurement.
#[test]
fn an_operator_pinned_timeout_is_taken_verbatim() {
    let t = validate_operator_timeout(45_000).expect("a positive value is usable");
    assert_eq!(t.timeout, Duration::from_millis(45_000));
    assert_eq!(t.basis, TimeoutBasis::Operator { band: PinBand::InBand });
    assert_eq!(t.basis.kind(), "operator", "an in-band pin keeps the historic token");
    assert!(
        t.basis.coverage_finding().is_none(),
        "an operator's own IN-BAND number is not a finding about anything"
    );
}

// ── #615: an out-of-band pin is honoured, and reported ──────────────

/// The band boundaries, both **inclusive**.
///
/// A pin exactly at the floor or exactly at the ceiling is a value
/// `derive_guard_timeout` would itself produce, so it must not read as a
/// finding. Walking one step either side of both bounds is what pins the
/// comparison operators: `<` vs `<=` on either end changes exactly one
/// of these four rows, and each error is silent in production.
#[test]
fn classify_pin_puts_the_boundaries_inside_the_band() {
    assert_eq!(classify_pin(TIMEOUT_FLOOR_MS - 1), PinBand::BelowFloor);
    assert_eq!(classify_pin(TIMEOUT_FLOOR_MS), PinBand::InBand);
    assert_eq!(classify_pin(TIMEOUT_CEILING_MS), PinBand::InBand);
    assert_eq!(classify_pin(TIMEOUT_CEILING_MS + 1), PinBand::AboveCeiling);
    // The two extremes an operator can actually reach: 1 ms (accepted,
    // because the refusal is for values that cannot work) and #612's
    // recommended ~350 s for a Metal host.
    assert_eq!(classify_pin(1), PinBand::BelowFloor);
    assert_eq!(classify_pin(350_000), PinBand::AboveCeiling);
}

/// A pin below the floor is applied AND says it weakens the tier.
///
/// The direction that matters: a shorter timeout does not error, it
/// fails OPEN. This is the arm #615 was filed for — the value is still
/// honoured, so the assertion on `t.timeout` is as load-bearing as the
/// one on the finding.
#[test]
fn a_pin_below_the_floor_is_honoured_and_reported() {
    let t = validate_operator_timeout(TIMEOUT_FLOOR_MS - 1).expect("usable");
    assert_eq!(
        t.timeout,
        Duration::from_millis(TIMEOUT_FLOOR_MS - 1),
        "still not clamped -- reporting is not overriding"
    );
    assert_eq!(t.basis.kind(), "operator-below-floor");
    let finding = t.basis.coverage_finding().expect("below the floor is a finding");
    // The phrase spans THREE `\`-continuation boundaries in the literal.
    // A continuation that loses its trailing space welds two words
    // together, and `every_coverage_finding_reads_as_prose` cannot see
    // that -- a double space it can, a missing one it cannot. Pinning a
    // sentence that crosses the joins is what covers the other half.
    assert!(
        finding.contains(
            "an adjudication that runs out of budget does not error -- it fails OPEN \
             to catalogue-only screening"
        ),
        "the finding must name the failure direction, unwelded: {finding}"
    );
}

/// A pin above the ceiling is applied AND says what it costs.
///
/// Reachable by following this project's own advice: #612's mitigation
/// for a Metal host is ~350 s, roughly 3x `TIMEOUT_CEILING_MS`. The
/// finding is not a rebuke — it records a deliberate trade.
#[test]
fn a_pin_above_the_ceiling_is_honoured_and_reported() {
    let t = validate_operator_timeout(350_000).expect("usable");
    assert_eq!(t.timeout, Duration::from_millis(350_000), "still not clamped");
    assert_eq!(t.basis.kind(), "operator-above-ceiling");
    let finding = t.basis.coverage_finding().expect("above the ceiling is a finding");
    // Same reason as the below-floor case: a phrase crossing the
    // continuation joins, not a single word.
    assert!(
        finding.contains(
            "The pin is honoured: a single dispatch may now block for the whole \
             pinned budget."
        ),
        "the finding must name what the trade costs, unwelded: {finding}"
    );
}

/// The two findings are different sentences, not one shared one.
///
/// They describe opposite exposures — screening less versus stalling
/// longer — and an operator who reads the wrong one takes the wrong
/// action. `coverage_finding` returns the sentence rather than a `bool`
/// precisely so this can be asserted.
#[test]
fn the_two_pin_findings_are_distinct() {
    let below = TimeoutBasis::Operator { band: PinBand::BelowFloor }
        .coverage_finding()
        .expect("some");
    let above = TimeoutBasis::Operator { band: PinBand::AboveCeiling }
        .coverage_finding()
        .expect("some");
    assert_ne!(below, above);
    // And neither is the ceiling-CLAMP finding, which is about the host's
    // measured throughput rather than about a configured value.
    let clamped = TimeoutBasis::Probed {
        tok_per_s: 100.0,
        derived_ms: 1_000_000,
        clamped: Clamped::ToCeiling,
    }
    .coverage_finding()
    .expect("some");
    assert_ne!(below, clamped);
    assert_ne!(above, clamped);
}

/// No coverage finding carries a doubled space, and every basis that is
/// not a finding stays quiet.
///
/// A `\`-continued Rust string swallows the newline **and** the next
/// line's indentation, so a continuation with two trailing spaces
/// produces a double space in operator-facing text that goes into a
/// durable audit row. #614's review found exactly that in two panic
/// strings, by reading rather than by a failing test.
///
/// **What this does NOT catch: welding.** A continuation that *loses*
/// its trailing space joins two words, and no general assertion can
/// distinguish `adjudicationthat` from a long identifier. That half is
/// covered per-finding, by asserting a phrase that crosses the
/// continuation joins -- see `a_pin_below_the_floor_is_honoured_and_reported`.
/// Saying so here rather than letting the name imply full coverage.
#[test]
fn every_coverage_finding_reads_as_prose() {
    let bases = [
        TimeoutBasis::Operator { band: PinBand::BelowFloor },
        TimeoutBasis::Operator { band: PinBand::AboveCeiling },
        TimeoutBasis::Probed {
            tok_per_s: 100.0,
            derived_ms: 1_000_000,
            clamped: Clamped::ToCeiling,
        },
        TimeoutBasis::Saturated { budget_ms: PROBE_BUDGET_MS },
        TimeoutBasis::Unprobed { reason: UnprobedReason::Failed },
    ];
    for b in &bases {
        let f = b.coverage_finding().expect("these five are the findings");
        assert!(!f.contains("  "), "collapsed continuation (double space) in: {f}");
        assert!(!f.contains('\n'), "a finding is one line: {f}");
        assert!(f.ends_with('.'), "a finding is a sentence: {f}");
    }
    // And every basis that is NOT a finding stays silent, so the count
    // above is the whole set rather than the ones this test remembered.
    for b in [
        TimeoutBasis::Operator { band: PinBand::InBand },
        TimeoutBasis::Probed { tok_per_s: 5_000.0, derived_ms: 26_000, clamped: Clamped::No },
        TimeoutBasis::Probed { tok_per_s: 9e9, derived_ms: 1, clamped: Clamped::ToFloor },
        TimeoutBasis::Unprobed { reason: UnprobedReason::Nonsensical },
        TimeoutBasis::Unprobed { reason: UnprobedReason::TooFewUncachedTokens },
        TimeoutBasis::Unprobed { reason: UnprobedReason::NoTokenCount },
    ] {
        assert!(b.coverage_finding().is_none(), "routine must stay quiet: {b:?}");
    }
}

/// **Not clamped to the derivation band.**
///
/// The band constrains what this module may *infer*; an operator who
/// pinned a number has already decided, and silently overriding them
/// would make the env var advisory. Both sides of the band are checked
/// so a future "just clamp it for safety" edit fails here.
#[test]
fn an_operator_pinned_timeout_is_not_clamped_to_the_derivation_band() {
    let below = validate_operator_timeout(TIMEOUT_FLOOR_MS - 1).expect("usable");
    assert_eq!(below.timeout, Duration::from_millis(TIMEOUT_FLOOR_MS - 1));
    let above = validate_operator_timeout(TIMEOUT_CEILING_MS + 1).expect("usable");
    assert_eq!(above.timeout, Duration::from_millis(TIMEOUT_CEILING_MS + 1));
    // #615 added the reporting; it must not have quietly added the clamp
    // the paragraph above forbids. Asserting the band as well means a
    // future edit cannot satisfy this test by clamping and relabelling.
    assert_eq!(below.basis, TimeoutBasis::Operator { band: PinBand::BelowFloor });
    assert_eq!(above.basis, TimeoutBasis::Operator { band: PinBand::AboveCeiling });
}

/// Zero is refused — the one value that cannot work.
///
/// No request completes in zero milliseconds, so every adjudication
/// would time out and take the fail-open door: configured, logged as
/// configured, and off. Same silent failure `validate_tau` refuses at
/// both ends of the threshold range, reached through the timeout
/// instead. **1 ms is accepted**, because the refusal is for values
/// that cannot work rather than for values that are unwise — the same
/// line `validate_tau` draws at `f32::MIN_POSITIVE`.
#[test]
fn a_zero_operator_timeout_is_refused_but_one_millisecond_is_not() {
    assert_eq!(validate_operator_timeout(0), Err(TimeoutError::Zero));
    let msg = TimeoutError::Zero.to_string();
    assert!(msg.contains("KASTELLAN_LLM_GUARD_TIMEOUT_MS"), "must name the key: {msg}");
    assert!(msg.contains("OPEN"), "must state the consequence: {msg}");

    assert!(
        validate_operator_timeout(1).is_ok(),
        "the refusal is for the unusable, not for the unwise"
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

/// Every `timeout_basis` token is distinct and log-field shaped.
///
/// These strings go straight into the `policy / guard_tier.boot` row and
/// the boot line. `Unprobed`'s three were covered; `Operator`,
/// `Probed` and `Saturated` were not, and a `Saturated` reported as
/// `"probed"` would spell a budget overrun the same way as a real
/// measurement — which it did until this PR.
#[test]
fn every_timeout_basis_token_is_distinct_and_log_shaped() {
    let bases = [
        TimeoutBasis::Operator { band: PinBand::InBand },
        TimeoutBasis::Operator { band: PinBand::BelowFloor },
        TimeoutBasis::Operator { band: PinBand::AboveCeiling },
        TimeoutBasis::Probed { tok_per_s: 5_000.0, derived_ms: 26_000, clamped: Clamped::No },
        TimeoutBasis::Saturated { budget_ms: PROBE_BUDGET_MS },
        TimeoutBasis::Unprobed { reason: UnprobedReason::Nonsensical },
        TimeoutBasis::Unprobed { reason: UnprobedReason::TooFewUncachedTokens },
        TimeoutBasis::Unprobed { reason: UnprobedReason::NoTokenCount },
        TimeoutBasis::Unprobed { reason: UnprobedReason::Failed },
    ];
    let mut seen = std::collections::BTreeSet::new();
    for b in &bases {
        let k = b.kind();
        assert!(!k.is_empty());
        assert!(!k.chars().any(char::is_whitespace), "not a log token: {k:?}");
        assert!(seen.insert(k), "duplicate timeout_basis token {k:?}");
    }
    assert_eq!(seen.len(), bases.len());
}

/// The band is pinned to literals, not only to itself.
///
/// Every other test in this file expresses its expectation in terms of
/// `TIMEOUT_FLOOR_MS`/`TIMEOUT_CEILING_MS`, so moving a bound moves the
/// assertions with it and nothing fails. `context_pin` pins
/// `REQUIRED_GUARD_N_CTX` to a literal for exactly this reason: a change
/// to a security-relevant constant should be a visible diff here.
#[test]
fn the_derivation_band_is_pinned_to_its_documented_values() {
    assert_eq!(TIMEOUT_FLOOR_MS, 15_000, "D2's value, kept as the floor");
    assert_eq!(TIMEOUT_CEILING_MS, 120_000);
    assert_eq!(PROBE_BUDGET_MS, 20_000);
    assert_eq!(MIN_UNCACHED_PROBE_TOKENS, 256);
}

/// A probe budget at or below the floor would saturate on hosts the
/// floor is perfectly adequate for, sending them to the ceiling.
///
/// A `const` assertion rather than a test body: the relation is between
/// two constants, so it can be a **compile** error rather than a
/// failing run.
const _: () = assert!(
    PROBE_BUDGET_MS > TIMEOUT_FLOOR_MS,
    "PROBE_BUDGET_MS must exceed TIMEOUT_FLOOR_MS"
);
