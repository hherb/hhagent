//! Unit tests for the boot report's payload and rate fields (issue #627).
//!
//! Pure throughout — no tier, no pool, no backend. That is the point of
//! the module: every one of these assertions used to be reachable only
//! by starting a daemon against a live guard endpoint, so none of them
//! existed.

use super::*;
use std::time::Duration;

use super::super::timeout::{Clamped, PinBand, UnprobedReason};

/// The eleven keys a configured boot row is contracted to carry.
///
/// Spelled out rather than derived from the payload, so *deleting* one
/// fails here. A `json!` literal cannot lose a key by accident in any
/// way a test that reads the literal back would notice.
const CONFIGURED_KEYS: &[&str] = &[
    "attempted_samples",
    "configured",
    "coverage_finding",
    "measured_samples",
    "n_ctx",
    "policy_digest",
    "slowest_tok_per_s",
    "tau",
    "timeout_basis",
    "timeout_ms",
    "tok_per_s",
];

/// The contended DGX boot as a basis: three attempts, three measured,
/// 6 090 tok/s fastest against 270 slowest.
///
/// Not invented — the three rates one unchanged DGX backend produced on
/// three consecutive boots, which is what #624 was filed about. The 22x
/// spread is what the row must be able to show.
fn contended_probed() -> GuardTimeout {
    GuardTimeout {
        timeout: Duration::from_millis(21_752),
        basis: TimeoutBasis::Probed {
            tok_per_s: 6_090.0,
            slowest_tok_per_s: 269.6,
            measured_samples: 3,
            attempted_samples: 3,
            derived_ms: 21_752,
            clamped: Clamped::No,
        },
    }
}

fn keys_of(v: &Value) -> Vec<String> {
    let mut ks: Vec<String> = v
        .as_object()
        .expect("payload is a JSON object")
        .keys()
        .cloned()
        .collect();
    ks.sort();
    ks
}

#[test]
fn a_configured_row_carries_exactly_the_documented_key_set() {
    assert_eq!(keys_of(&boot_payload(0.795, 66_048, &contended_probed())), CONFIGURED_KEYS);
}

/// The whole of #627: the fastest rate must land in `tok_per_s` and the
/// slowest in `slowest_tok_per_s`, never the reverse.
///
/// Swapping them keeps every key, every type and every non-null, so the
/// key-set test above cannot see it. What it breaks is the documented
/// operator query `slowest_tok_per_s < tok_per_s / 2`: a busy host stops
/// reporting as busy and a quiet one starts, which is the exact
/// diagnostic #624 shipped the two fields to provide.
#[test]
fn the_fastest_rate_is_tok_per_s_and_the_slowest_is_slowest_tok_per_s() {
    let p = boot_payload(0.795, 66_048, &contended_probed());
    let fast = p["tok_per_s"].as_f64().expect("a probed row has a rate");
    let slow = p["slowest_tok_per_s"].as_f64().expect("a probed row has a spread");
    assert!((fast - 6_090.0).abs() < 1.0, "tok_per_s must be the FASTEST sample, got {fast}");
    assert!((slow - 269.6).abs() < 1.0, "slowest_tok_per_s must be the SLOWEST, got {slow}");
    assert!(slow < fast, "the slowest sample cannot outrun the fastest");
    // The operator query itself, run against the row it was written for.
    assert!(slow < fast / 2.0, "a 22x spread must satisfy the busy-boot query");
}

/// A quiet host must NOT satisfy the busy-boot query — the other half of
/// the same contract, and the direction a swap would break loudly.
#[test]
fn a_quiet_hosts_row_does_not_satisfy_the_busy_boot_query() {
    let budget = GuardTimeout {
        timeout: Duration::from_millis(19_000),
        basis: TimeoutBasis::Probed {
            tok_per_s: 7_026.0,
            slowest_tok_per_s: 6_953.0,
            measured_samples: 3,
            attempted_samples: 3,
            derived_ms: 19_000,
            clamped: Clamped::No,
        },
    };
    let p = boot_payload(0.795, 66_048, &budget);
    let fast = p["tok_per_s"].as_f64().unwrap();
    let slow = p["slowest_tok_per_s"].as_f64().unwrap();
    assert!(slow >= fast / 2.0, "three agreeing samples are a measurement, not a busy boot");
}

/// One sample reports its own rate as both ends of the spread.
///
/// Honest rather than a fabricated `null`: the probe DID observe a
/// slowest rate, and it was the only one. The invariant is documented on
/// `TimeoutBasis::Probed`; this pins that the payload does not
/// special-case it away.
#[test]
fn one_measured_sample_reports_the_same_rate_at_both_ends() {
    let budget = GuardTimeout {
        timeout: Duration::from_millis(30_000),
        basis: TimeoutBasis::Probed {
            tok_per_s: 1_582.0,
            slowest_tok_per_s: 1_582.0,
            measured_samples: 1,
            attempted_samples: 3,
            derived_ms: 30_000,
            clamped: Clamped::No,
        },
    };
    let p = boot_payload(0.795, 66_048, &budget);
    assert_eq!(p["tok_per_s"], p["slowest_tok_per_s"]);
    assert_eq!(p["measured_samples"], 1);
    // The denominator is what makes `measured_samples: 1` readable: one
    // sample that worked, or three of which two did not?
    assert_eq!(p["attempted_samples"], 3);
}

/// Every basis that measured nothing reports `null`, not `0.0`.
///
/// A fabricated zero would be logged and stored as if it had been
/// observed, and `tok_per_s = 0` is a perfectly plausible reading for a
/// wedged backend — so the two would be indistinguishable.
#[test]
fn a_basis_with_no_measurement_reports_null_rates_not_zeroes() {
    let unmeasured: Vec<TimeoutBasis> = vec![
        TimeoutBasis::Operator { band: PinBand::InBand },
        TimeoutBasis::Operator { band: PinBand::BelowFloor },
        TimeoutBasis::Operator { band: PinBand::AboveCeiling },
        TimeoutBasis::Saturated { budget_ms: 20_000, attempted_samples: 1 },
        TimeoutBasis::Unprobed { reason: UnprobedReason::Failed, attempted_samples: 3 },
        TimeoutBasis::Unprobed { reason: UnprobedReason::NoTokenCount, attempted_samples: 3 },
        TimeoutBasis::Unprobed {
            reason: UnprobedReason::TooFewUncachedTokens,
            attempted_samples: 3,
        },
        TimeoutBasis::Unprobed { reason: UnprobedReason::Nonsensical, attempted_samples: 3 },
    ];
    for basis in unmeasured {
        let budget = GuardTimeout { timeout: Duration::from_millis(120_000), basis };
        let p = boot_payload(0.795, 66_048, &budget);
        let kind = budget.basis.kind();
        for field in ["tok_per_s", "slowest_tok_per_s", "measured_samples"] {
            assert!(
                p[field].is_null(),
                "{kind} measured nothing, so {field} must be null, got {}",
                p[field]
            );
        }
        // The key set does not shrink when the values go null — a reader
        // querying `payload->>'tok_per_s'` must find the key everywhere.
        assert_eq!(keys_of(&p), CONFIGURED_KEYS, "{kind} must carry the same keys");
    }
}

/// `attempted_samples` is present wherever a probe RAN, and null only
/// for the one basis that runs no probe at all.
///
/// On `Saturated`/`Unprobed` it is not a leftover: it is the strength of
/// the evidence behind the finding, and one failed call predicts a
/// wholly fail-open tier far more weakly than three.
#[test]
fn attempted_samples_is_null_only_for_an_operator_pin() {
    let pinned = GuardTimeout {
        timeout: Duration::from_millis(350_000),
        basis: TimeoutBasis::Operator { band: PinBand::AboveCeiling },
    };
    assert!(
        boot_payload(0.795, 66_048, &pinned)["attempted_samples"].is_null(),
        "an operator pin runs no probe, so it attempted nothing"
    );

    let probed_bases = [
        TimeoutBasis::Saturated { budget_ms: 20_000, attempted_samples: 1 },
        TimeoutBasis::Unprobed { reason: UnprobedReason::Failed, attempted_samples: 3 },
    ];
    for basis in probed_bases {
        let expected = match basis {
            TimeoutBasis::Saturated { attempted_samples, .. }
            | TimeoutBasis::Unprobed { attempted_samples, .. } => attempted_samples,
            _ => unreachable!(),
        };
        let budget = GuardTimeout { timeout: Duration::from_millis(120_000), basis };
        let kind = budget.basis.kind();
        assert_eq!(
            boot_payload(0.795, 66_048, &budget)["attempted_samples"],
            expected,
            "{kind} ran {expected} probe call(s) and the row must say so"
        );
    }
}

/// The finding reaches the DURABLE row, not only the `warn!` line.
///
/// `kind()` folds `Clamped::ToCeiling` into a bare `"probed"`, so
/// without this key the row for a host that cannot adjudicate a
/// worst-case document is indistinguishable from a healthy one.
#[test]
fn a_coverage_finding_reaches_the_row_and_a_routine_boot_leaves_it_null() {
    let clamped = GuardTimeout {
        timeout: Duration::from_millis(120_000),
        basis: TimeoutBasis::Probed {
            tok_per_s: 269.6,
            slowest_tok_per_s: 269.6,
            measured_samples: 1,
            attempted_samples: 3,
            derived_ms: 489_000,
            clamped: Clamped::ToCeiling,
        },
    };
    let finding = boot_payload(0.795, 66_048, &clamped)["coverage_finding"].clone();
    assert!(!finding.is_null(), "a ceiling clamp is a finding and must be stored");
    assert_eq!(
        finding.as_str(),
        clamped.basis.coverage_finding(),
        "the row must carry the basis's own sentence, not a paraphrase"
    );

    assert!(
        boot_payload(0.795, 66_048, &contended_probed())["coverage_finding"].is_null(),
        "a routine boot must leave the finding null so the query for affected hosts is exact"
    );
}

/// The three scalars are carried through unaltered, and `timeout_basis`
/// is the basis's own token rather than a second spelling of it.
#[test]
fn the_scalars_and_the_basis_token_are_carried_verbatim() {
    let budget = contended_probed();
    let p = boot_payload(0.79552656, 66_048, &budget);
    assert_eq!(p["configured"], true);
    assert_eq!(p["tau"].as_f64().unwrap() as f32, 0.79552656_f32);
    assert_eq!(p["n_ctx"], 66_048);
    assert_eq!(p["timeout_ms"], 21_752, "the millisecond budget, not the Duration's seconds");
    assert_eq!(
        p["timeout_ms"].as_u64().unwrap(),
        timeout_ms(&budget),
        "the row and the log lines must spell the budget the same way"
    );
    assert_eq!(p["timeout_basis"], budget.basis.kind());
    assert_eq!(
        p["policy_digest"].as_str().unwrap(),
        super::super::policy::policy_digest(),
        "the row pins the prompt the tier actually ran"
    );
}

/// The unconfigured row spells "no tier ran" with the SAME token the
/// per-dispatch `guard.state` vocabulary uses.
///
/// A second spelling would split every query over unconfigured hosts
/// into a live half and an orphaned half.
#[test]
fn the_unconfigured_row_reuses_the_guard_state_vocabulary() {
    let p = not_configured_payload();
    assert_eq!(keys_of(&p), ["configured", "state"]);
    assert_eq!(p["configured"], false);
    assert_eq!(p["state"], Unadjudicated::NotConfigured.as_str());
}

/// `BootRates` is derived once and shared by the row, the `info!` line
/// and the `warn!` finding, so the three cannot disagree.
///
/// Asserted at the source as well as through the payload: the log sites
/// read the struct's named fields, and nothing else in the tree can see
/// them.
#[test]
fn boot_rates_reads_a_probed_basis_without_transposing_it() {
    let rates = BootRates::from_basis(&contended_probed().basis);
    assert_eq!(
        rates,
        BootRates {
            tok_per_s: Some(6_090.0),
            slowest_tok_per_s: Some(269.6),
            measured_samples: Some(3),
            attempted_samples: Some(3),
        }
    );
}
