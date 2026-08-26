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
use super::super::{derive_guard_timeout, TIMEOUT_FLOOR_MS};
use super::*;

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
    let honest = derive_guard_timeout(&ProbeOutcome::Measured {
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
