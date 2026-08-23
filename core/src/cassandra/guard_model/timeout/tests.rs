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
        t.basis.is_coverage_finding(),
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
        !t.basis.is_coverage_finding(),
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
    assert_eq!(clamped_of(&t), Clamped::ToCeiling);
    assert!(t.basis.is_coverage_finding());
    assert_ne!(
        t.timeout,
        Duration::from_millis(TIMEOUT_FLOOR_MS),
        "the floor would be exactly inverted"
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
        assert!(
            !t.basis.is_coverage_finding(),
            "{outcome:?} says nothing about coverage"
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
    assert_eq!(t.basis, TimeoutBasis::Operator);
    assert!(
        !t.basis.is_coverage_finding(),
        "an operator's own number is not a finding about the host"
    );
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

/// The nonce goes FIRST. A prefix cache matches from position 0, so a
/// nonce suffix would leave the body cached and reproduce the 4x
/// over-estimate this whole module is built to avoid.
#[test]
fn the_probe_document_leads_with_the_nonce() {
    let doc = probe_document("boot-1724371200");
    assert!(
        doc.starts_with("boot-1724371200"),
        "the nonce must be a PREFIX; a suffix leaves the body cacheable"
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
