//! Unit tests for the guard timeout **derivation** (wiring-spec D9).
//!
//! Pure throughout — no server, no clock, no Postgres. Two fixtures do
//! most of the work and both are real measurements rather than
//! invented numbers: the DGX's M2 sample, and the Mac throughput
//! measurement 3 implies.
//!
//! The other two thirds of this module's tests live beside the
//! production files they name: what one sample IS is tested in
//! `sample/tests.rs`, and how a budget reports its provenance in
//! `basis/tests.rs`.

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

/// Derive from a single sample, exactly as a one-sample run does.
///
/// Goes through [`summarise`] rather than hand-building a
/// [`ProbeSummary`], so these tests keep exercising the real path and a
/// change to the fold cannot leave them passing against a shortcut.
fn derive_from_one(outcome: &ProbeOutcome) -> GuardTimeout {
    derive_guard_timeout(&summarise(std::slice::from_ref(outcome)))
}

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
    let t = derive_from_one(&DGX_MEASURED);
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
    let t = derive_from_one(&MAC_MEASURED);
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
    let t = derive_from_one(&ProbeOutcome::Measured {
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
    let t = derive_from_one(&ProbeOutcome::Saturated { budget_ms: PROBE_BUDGET_MS });
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
    let t = derive_from_one(&ProbeOutcome::Saturated { budget_ms: PROBE_BUDGET_MS });
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
        let t = derive_from_one(outcome);
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
    let timed_out = derive_from_one(&probe_error_outcome(true, String::new(), PROBE_BUDGET_MS));
    let refused =
        derive_from_one(&probe_error_outcome(false, "refused".to_string(), PROBE_BUDGET_MS));
    assert_eq!(timed_out.timeout, Duration::from_millis(TIMEOUT_CEILING_MS));
    assert_eq!(refused.timeout, Duration::from_millis(TIMEOUT_FLOOR_MS));
    assert!(
        timed_out.timeout > refused.timeout,
        "a slow host must end up with a LONGER budget than an unreachable one"
    );
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

// ── #624: the spread reaches the budget's own provenance ─────────────

/// The contended DGX boot derives a working budget and fires NO finding.
///
/// The regression test for issue #624 stated end to end. Boot 2 of three
/// on one unchanged DGX backend measured 269.6 tok/s; on its own that
/// derives ~489 s, clamps to the 120 s ceiling and emits "this host
/// cannot adjudicate a worst-case document" — the loudest signal the
/// tier has, spent on a host that adjudicates one in ~19 s. Taking all
/// three samples, the fastest is the one about the host.
#[test]
fn the_contended_boot_derives_a_working_budget_and_no_false_finding() {
    let samples = [
        ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 133 }, // ~6 090 tok/s
        ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 3_004 }, // ~270 tok/s
        ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 512 }, // ~1 582 tok/s
    ];

    // What the single contended sample would have done, kept in the test
    // so the defect is visible beside its fix rather than only described.
    let contended_alone = derive_from_one(&samples[1]);
    assert_eq!(contended_alone.timeout, Duration::from_millis(TIMEOUT_CEILING_MS));
    assert_eq!(clamped_of(&contended_alone), Clamped::ToCeiling);
    assert!(contended_alone.basis.coverage_finding().is_some(), "the FALSE finding");

    let t = derive_guard_timeout(&summarise(&samples));
    assert_eq!(clamped_of(&t), Clamped::No, "the host is comfortably inside the band");
    assert!(
        t.basis.coverage_finding().is_none(),
        "a busy boot is not a host that cannot screen"
    );
    // 66048 / 6090 * 1000 * 2 ~= 21.7 s, which is the figure the first
    // live production boot actually recorded (21 752 ms).
    assert!(
        (21_000..=23_000).contains(&(t.timeout.as_millis() as u64)),
        "expected ~21.7 s, got {:?}",
        t.timeout
    );
}

/// The spread reaches the basis, so one boot row can say it was busy.
///
/// Without this a reader of `policy / guard_tier.boot` sees only the
/// winning rate and cannot tell a quiet host from a busy one — which is
/// precisely why #624 took three boots and a direct measurement of the
/// backend to diagnose rather than one row.
#[test]
fn the_basis_carries_the_spread_the_samples_disagreed_by() {
    let t = derive_guard_timeout(&summarise(&[
        ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 133 },
        ProbeOutcome::Measured { uncached_tokens: 810, elapsed_ms: 3_004 },
    ]));
    match t.basis {
        TimeoutBasis::Probed { tok_per_s, slowest_tok_per_s, measured_samples, .. } => {
            assert_eq!(measured_samples, 2);
            assert!(tok_per_s > 6_000.0, "the fastest sample, got {tok_per_s}");
            assert!(slowest_tok_per_s < 300.0, "the slowest sample, got {slowest_tok_per_s}");
            assert!(
                tok_per_s / slowest_tok_per_s > 20.0,
                "a 22x spread is what a contended boot looks like"
            );
        }
        other => panic!("expected a probed basis, got {other:?}"),
    }
}

/// A hand-built summary cannot write a self-contradicting durable row.
///
/// `summarise` never pairs a `Measured` best with zero measuring samples
/// or with no slowest rate — but `ProbeSummary`'s fields are public, and
/// a row claiming `measured_samples: 0` beside a measured `tok_per_s`
/// would be unreadable. The guard is the same shape as the `is_finite`
/// one beside it: cheap, and "unreachable" is a property of another
/// function.
#[test]
fn a_summary_built_by_hand_still_reports_a_coherent_row() {
    let t = derive_guard_timeout(&ProbeSummary {
        best: DGX_MEASURED,
        measured_samples: 0,
        slowest_tok_per_s: None,
    });
    match t.basis {
        TimeoutBasis::Probed { tok_per_s, slowest_tok_per_s, measured_samples, .. } => {
            assert_eq!(measured_samples, 1, "a measured best came from at least one sample");
            assert_eq!(slowest_tok_per_s, tok_per_s, "the only rate there is");
        }
        other => panic!("expected a probed basis, got {other:?}"),
    }
}
