//! Unit tests for the boot report's payload and rate fields (issue #627).
//!
//! Pure throughout — no tier, no pool, no backend. That is the point of
//! the module: every one of these assertions used to be reachable only
//! by starting a daemon against a live guard endpoint, so none of them
//! existed.

use super::*;
use std::time::Duration;

use super::super::timeout::{Clamped, PinBand, UnprobedReason};

/// The keys a configured boot row is contracted to carry.
///
/// Spelled out rather than derived from the payload, so *deleting* one
/// fails here. A `json!` literal cannot lose a key by accident in any
/// way a test that reads the literal back would notice.
///
/// Deliberately not stating the count in prose: the array IS the count,
/// and a number in the sentence above would be a third place to edit
/// that nothing enforces — the drift `main.rs`'s "three findings" note
/// records having already happened once in this tree.
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
/// ~6 090 tok/s fastest against 270 slowest.
///
/// Not invented — the fastest and slowest of the three rates one
/// unchanged DGX backend produced on three consecutive boots
/// (6 073 / 269.6 / 1 582), which is what #624 was filed about, **the
/// fastest expressed at the probe's own 810-token sample size** (810 /
/// 0.133 s = 6 090). That qualifier is load-bearing: 6 090 is not one of
/// the three recorded figures, and the sibling fixture at
/// `timeout::summary::tests::contended_dgx_samples` spells it the same
/// way for the same reason.
///
/// The 22.5x spread is what the row must be able to show. (The 26x
/// quoted elsewhere in the guard tree is a different ratio — the ~7 000
/// tok/s measured directly minutes later against the 269.6 boot — and
/// no row ever carries the 7 000.)
///
/// `timeout` and `derived_ms` are 21 752, the budget derived from the
/// recorded 6 073 rather than from this fixture's 6 090; nothing
/// enforces that coupling on a hand-built basis, and no assertion here
/// depends on it.
fn contended_probed() -> GuardTimeout {
    GuardTimeout {
        timeout: Duration::from_millis(21_752),
        basis: TimeoutBasis::Probed {
            fastest_tok_per_s: 6_090.0,
            slowest_tok_per_s: 269.6,
            measured_samples: 3,
            attempted_samples: 3,
            derived_ms: 21_752,
            clamped: Clamped::No,
        },
    }
}

/// The slow host that clamped to the ceiling: 269.6 tok/s derives a
/// 489 s budget, which the band cuts to [`TIMEOUT_CEILING_MS`].
///
/// The only fixture here whose `timeout` and `derived_ms` differ, which
/// is what makes it the one that can catch `timeout_ms` reading the
/// wrong one of the two.
///
/// [`TIMEOUT_CEILING_MS`]: super::super::timeout::TIMEOUT_CEILING_MS
fn ceiling_clamped_basis() -> TimeoutBasis {
    TimeoutBasis::Probed {
        fastest_tok_per_s: 269.6,
        slowest_tok_per_s: 269.6,
        measured_samples: 1,
        attempted_samples: 3,
        derived_ms: 489_000,
        clamped: Clamped::ToCeiling,
    }
}

fn ceiling_clamped() -> GuardTimeout {
    GuardTimeout {
        timeout: Duration::from_millis(120_000),
        basis: ceiling_clamped_basis(),
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

/// The durable wire key is `tok_per_s` and **must not** become
/// `fastest_tok_per_s` when the Rust field does.
///
/// `CONFIGURED_KEYS` above already fails on a renamed key — but only
/// *incidentally*, and #632 is what made that worth saying out loud. A
/// global `s/tok_per_s/fastest_tok_per_s/` sweeps `CONFIGURED_KEYS`
/// itself, and what then fails is alphabetical ordering (`keys_of`
/// sorts, and `"fastest_tok_per_s"` no longer sorts where `"tok_per_s"`
/// did) rather than anything about the key. A renamer who notices the
/// ordering failure and re-sorts the array passes every other assertion
/// in this file, because each of them spells the key as a bare literal
/// the same sweep rewrote.
///
/// So this asserts the rename **negatively**, which is the one shape a
/// global rename cannot satisfy: the field is `fastest_tok_per_s` and
/// the key is not.
///
/// Why it cannot move: `policy / guard_tier.boot` rows carrying this key
/// are on disk on live hosts, and the documented operator query
/// `slowest_tok_per_s < tok_per_s / 2` is written against it. Renaming
/// it is a migration, not a refactor.
#[test]
fn the_durable_wire_key_did_not_follow_the_rust_field_rename() {
    let p = boot_payload(0.795, 66_048, &contended_probed());
    assert!(
        p.get("tok_per_s").is_some(),
        "the durable key is `tok_per_s`; live audit_log rows carry it",
    );
    assert!(
        p.get("fastest_tok_per_s").is_none(),
        "#632 renamed the FIELD, not the KEY -- the wire vocabulary is \
         frozen so an operator correlating a boot log line with its \
         audit row reads one vocabulary rather than two. Got: {p:?}",
    );
}

/// The whole of #627: the fastest rate must land in `tok_per_s` and the
/// slowest in `slowest_tok_per_s`, never the reverse.
///
/// **This is one of only two tests in the file that detect the swap**
/// (the other is `boot_rates_reads_a_probed_basis_without_transposing_it`,
/// which asserts it at the struct rather than through the payload). The
/// key set, the types and the non-nulls all survive a transposition, so
/// neither the key-set test above nor the quiet-host test below can see
/// it — do not delete either of the two as redundant.
///
/// What a swap breaks is the documented operator query
/// `slowest_tok_per_s < tok_per_s / 2`, and it breaks it more completely
/// than "inverted": because `slowest <= fastest` always holds, a swapped
/// row asks `fastest < slowest / 2`, which NO row can satisfy. The query
/// returns the empty set on every host forever — a contended host stops
/// reporting as contended and nothing takes its place.
#[test]
fn the_fastest_rate_is_tok_per_s_and_the_slowest_is_slowest_tok_per_s() {
    let p = boot_payload(0.795, 66_048, &contended_probed());
    let fast = p["tok_per_s"].as_f64().expect("a probed row has a rate");
    let slow = p["slowest_tok_per_s"].as_f64().expect("a probed row has a spread");
    // Exact, not tolerance-based: `boot_payload` does no arithmetic on
    // these, so the only transform is the f32 -> f64 widening, which is
    // lossless. A tolerance here would be slop with nothing to absorb,
    // and an invitation to widen it later.
    assert_eq!(fast, 6_090.0_f32 as f64, "tok_per_s must be the FASTEST sample");
    assert_eq!(slow, 269.6_f32 as f64, "slowest_tok_per_s must be the SLOWEST");
    assert!(slow < fast, "the slowest sample cannot outrun the fastest");
    // The operator query itself, run against the row it was written for.
    assert!(slow < fast / 2.0, "a 22.5x spread must satisfy the busy-boot query");
}

/// A quiet host must NOT satisfy the busy-boot query — the accepting
/// arm's complement, so the query is not trivially true of every row.
///
/// ⚠️ This test is **insensitive to the transposition** and must not be
/// counted as swap coverage: with two near-equal rates, `slow >= fast/2`
/// holds in either orientation (7 026/6 953 swapped is 6 953/7 026, and
/// 7 026 >= 3 476.5 either way). `the_fastest_rate_is_...` above is the
/// only payload-level swap detector.
#[test]
fn a_quiet_hosts_row_does_not_satisfy_the_busy_boot_query() {
    let budget = GuardTimeout {
        timeout: Duration::from_millis(19_000),
        basis: TimeoutBasis::Probed {
            fastest_tok_per_s: 7_026.0,
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
///
/// ⚠️ **This is the only fixture in the file where `measured_samples`
/// and `attempted_samples` DIFFER (1 vs 3), and therefore the only test
/// that can detect the two being transposed** — every other probed
/// fixture is 3/3, where a swap is invisible. Do not "simplify" the
/// counts here to match the others.
#[test]
fn one_measured_sample_reports_the_same_rate_at_both_ends() {
    let budget = GuardTimeout {
        timeout: Duration::from_millis(30_000),
        basis: TimeoutBasis::Probed {
            fastest_tok_per_s: 1_582.0,
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
/// observed, and a rate of 0 is a perfectly plausible reading for a
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

/// A ceiling-clamped host's budget is the CLAMPED one, not the derived
/// one the clamp threw away.
///
/// The only fixture where `timeout` and `derived_ms` differ, and so the
/// only test that can tell `timeout_ms` reading the budget from a mutant
/// reading the basis's `derived_ms`. Getting that wrong would put a
/// 489 s figure in the durable row on the one basis whose whole meaning
/// is "the derivation was clamped away", and no operator query would
/// ever match the timeout actually enforced.
#[test]
fn a_clamped_row_reports_the_enforced_budget_not_the_derived_one() {
    let clamped = ceiling_clamped();
    let p = boot_payload(0.795, 66_048, &clamped);
    assert_eq!(p["timeout_ms"], 120_000, "the ENFORCED budget, not the 489 000 ms derived");
    assert_eq!(p["timeout_basis"], "probed", "kind() folds the clamp into a bare probed");
}

/// Every state a [`TimeoutBasis`] can be in, paired with whether it is
/// contracted to carry a `coverage_finding`.
///
/// One table shared by the two tests that must walk *all* of them, so a
/// state added to the enum is added here once. The alternative -- two
/// hand-kept lists -- is how a new state ends up covered by one test and
/// not the other, which is the selective blind spot #627's review found
/// in the first place: a finding routed only for `Probed` is invisible
/// on a healthy host.
///
/// Sharing the table cannot, on its own, keep it complete: this is a
/// `vec![]`, so a state can be missing from it with **both** consumers
/// silently skipping it — covered by neither rather than by one. That is
/// not hypothetical; the table shipped without
/// `Probed { clamped: Clamped::ToFloor }` and neither test noticed.
/// [`state_key`] and [`ALL_STATE_KEYS`] are what close it, asserted in
/// [`the_basis_table_covers_every_state_exactly_once`].
///
/// The `bool` is spelled literally rather than derived from
/// `coverage_finding()`, so this table is an independent statement of
/// which states are findings — not a restatement of the code under test.
fn every_basis_with_expected_finding() -> Vec<(TimeoutBasis, bool)> {
    vec![
        // Findings.
        (ceiling_clamped_basis(), true),
        (TimeoutBasis::Saturated { budget_ms: 20_000, attempted_samples: 1 }, true),
        (TimeoutBasis::Operator { band: PinBand::BelowFloor }, true),
        (TimeoutBasis::Operator { band: PinBand::AboveCeiling }, true),
        (TimeoutBasis::Unprobed { reason: UnprobedReason::Failed, attempted_samples: 3 }, true),
        // Routine.
        (TimeoutBasis::Operator { band: PinBand::InBand }, false),
        (contended_probed().basis, false),
        // A fast host: the derivation landed under `TIMEOUT_FLOOR_MS` and
        // was raised to it. Unremarkable, hence no finding -- but it is a
        // distinct `Clamped` state that `coverage_finding` enumerates by
        // name, and it was the one this table was missing.
        (
            TimeoutBasis::Probed {
                fastest_tok_per_s: 12_400.0,
                slowest_tok_per_s: 11_950.0,
                measured_samples: 3,
                attempted_samples: 3,
                derived_ms: 9_000,
                clamped: Clamped::ToFloor,
            },
            false,
        ),
        (
            TimeoutBasis::Unprobed { reason: UnprobedReason::NoTokenCount, attempted_samples: 3 },
            false,
        ),
        (
            TimeoutBasis::Unprobed {
                reason: UnprobedReason::TooFewUncachedTokens,
                attempted_samples: 3,
            },
            false,
        ),
        (
            TimeoutBasis::Unprobed { reason: UnprobedReason::Nonsensical, attempted_samples: 3 },
            false,
        ),
    ]
}

/// A distinct token per state a [`TimeoutBasis`] can be in.
///
/// Finer than `TimeoutBasis::kind()`, which folds all three `Clamped`
/// states into a bare `"probed"`. That fold is not a detail: a `kind()`
/// set cannot tell a table holding three `Probed` rows from one holding
/// a single `Probed` row and two duplicates elsewhere, and a mutant that
/// did exactly that **survived** a `kind()`-based assertion. This
/// function exists because of that surviving mutant.
///
/// Wildcard-free, so a new state is a build error here too. That half is
/// belt-and-braces rather than novel — production's
/// `TimeoutBasis::coverage_finding` is already wildcard-free and fails
/// first — but it is what drags whoever adds a state into *this* file,
/// where [`ALL_STATE_KEYS`] then tells them what else to update.
///
/// It constrains **coverage**, not the verdict: the `bool` in the table
/// stays an independent literal, so this does not turn the table into a
/// restatement of the code under test.
fn state_key(basis: &TimeoutBasis) -> &'static str {
    match basis {
        TimeoutBasis::Operator { band: PinBand::InBand } => "operator/in-band",
        TimeoutBasis::Operator { band: PinBand::BelowFloor } => "operator/below-floor",
        TimeoutBasis::Operator { band: PinBand::AboveCeiling } => "operator/above-ceiling",
        TimeoutBasis::Probed { clamped: Clamped::No, .. } => "probed/unclamped",
        TimeoutBasis::Probed { clamped: Clamped::ToFloor, .. } => "probed/to-floor",
        TimeoutBasis::Probed { clamped: Clamped::ToCeiling, .. } => "probed/to-ceiling",
        TimeoutBasis::Saturated { .. } => "saturated",
        TimeoutBasis::Unprobed { reason: UnprobedReason::Nonsensical, .. } => {
            "unprobed/nonsensical"
        }
        TimeoutBasis::Unprobed { reason: UnprobedReason::TooFewUncachedTokens, .. } => {
            "unprobed/too-few-uncached-tokens"
        }
        TimeoutBasis::Unprobed { reason: UnprobedReason::NoTokenCount, .. } => {
            "unprobed/no-token-count"
        }
        TimeoutBasis::Unprobed { reason: UnprobedReason::Failed, .. } => "unprobed/failed",
    }
}

/// Every token [`state_key`] can return, spelled out.
///
/// **Add a state to [`TimeoutBasis`] and you must touch this array and
/// the arm above it, which sit together on purpose.** A bare count would
/// not do the same work: a new variant whose `state_key` arm was added
/// but whose table row was forgotten leaves the table at its old length,
/// so `len() == 11` still passes and the state is covered by nothing.
/// Comparing SETS makes that omission name itself.
const ALL_STATE_KEYS: &[&str] = &[
    "operator/above-ceiling",
    "operator/below-floor",
    "operator/in-band",
    "probed/to-ceiling",
    "probed/to-floor",
    "probed/unclamped",
    "saturated",
    "unprobed/failed",
    "unprobed/no-token-count",
    "unprobed/nonsensical",
    "unprobed/too-few-uncached-tokens",
];

/// The table holds every state exactly once.
///
/// Two assertions, neither implying the other — a table can be
/// duplicate-free and short, or complete and padded:
///
/// * **no duplicates** — the key set is as large as the table. This is
///   the half a `kind()`-based set silently failed, because `kind()`
///   folds the three `Probed` states together and so reads a duplicate
///   as a fold.
/// * **no omissions** — the key set is exactly [`ALL_STATE_KEYS`], which
///   names the missing or unexpected state rather than reporting a
///   number that has to be decoded.
///
/// The literal 11 is asserted on [`ALL_STATE_KEYS`] rather than on the
/// table, and that is the point: without it, deleting a state's row *and*
/// its key together leaves both sides agreeing at 10 and the suite green.
/// Anchoring the count to the enumeration means the two can only shrink
/// by someone editing the number as well.
#[test]
fn the_basis_table_covers_every_state_exactly_once() {
    assert_eq!(
        ALL_STATE_KEYS.len(),
        11,
        "3 PinBand + 3 Clamped + 1 Saturated + 4 UnprobedReason; if a state was genuinely \
         removed from TimeoutBasis, `state_key` stopped compiling before you got here"
    );
    let table = every_basis_with_expected_finding();
    let keys: std::collections::BTreeSet<&str> = table.iter().map(|(b, _)| state_key(b)).collect();
    assert_eq!(
        keys.len(),
        table.len(),
        "the table lists a state twice: {} rows collapsed to {} distinct states {keys:?}",
        table.len(),
        keys.len()
    );
    let expected: std::collections::BTreeSet<&str> = ALL_STATE_KEYS.iter().copied().collect();
    assert_eq!(
        keys, expected,
        "the table and ALL_STATE_KEYS disagree; missing from the table: {:?}, unexpected \
         in it: {:?}",
        expected.difference(&keys).collect::<Vec<_>>(),
        keys.difference(&expected).collect::<Vec<_>>(),
    );
}

/// The finding reaches the DURABLE row for EVERY basis that has one,
/// and a routine boot leaves it null.
///
/// `kind()` folds `Clamped::ToCeiling` into a bare `"probed"`, so
/// without this key the row for a host that cannot adjudicate a
/// worst-case document is indistinguishable from a healthy one.
///
/// Table-driven over the whole enum rather than over the clamped basis
/// alone, because a payload that routed the finding only for `Probed`
/// passes a single-basis test while silencing the three LOUDEST states —
/// the probe never returned, the probe failed (which predicts a tier
/// that fails open on every dispatch), and both out-of-band operator
/// pins. `timeout_basis` still reads `"probe-failed"` in that mutant, so
/// nothing looks wrong; the documented query
/// `WHERE payload->>'coverage_finding' IS NOT NULL` just returns the
/// empty set for exactly the hosts it was written to find.
#[test]
fn every_basis_with_a_finding_reaches_the_row_and_the_quiet_ones_stay_null() {
    for (basis, expects_finding) in every_basis_with_expected_finding() {
        let budget = GuardTimeout { timeout: Duration::from_millis(120_000), basis };
        let kind = budget.basis.kind();
        let finding = boot_payload(0.795, 66_048, &budget)["coverage_finding"].clone();
        assert_eq!(
            !finding.is_null(),
            expects_finding,
            "{kind}: expected a finding? {expects_finding}; row carried {finding}"
        );
        // And it is the basis's OWN sentence, not a paraphrase — an
        // operator reading the row and an operator reading the `warn!`
        // line must be reading the same words.
        assert_eq!(
            finding.as_str(),
            budget.basis.coverage_finding(),
            "{kind} must carry the basis's own sentence verbatim"
        );
    }
}

/// The scalars are carried through unaltered, and `timeout_basis`
/// is the basis's own token rather than a second spelling of it.
///
/// Asserted at **two different `(tau, n_ctx)` pairs**, and that is the
/// point rather than thoroughness for its own sake: every other test in
/// this file passes `0.795` and `66_048`, so a payload that ignored its
/// arguments and emitted those two as constants would be green
/// everywhere else in the file. `n_ctx` in particular is host-varying —
/// `guard_tier_e2e` runs at 131 072 — and a frozen one would silently
/// misreport which context the D8 check verified.
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

    // The second pair: different tau, different n_ctx, same basis.
    let q = boot_payload(0.5, 131_072, &budget);
    assert_eq!(q["tau"].as_f64().unwrap() as f32, 0.5_f32, "tau is the argument, not a constant");
    assert_eq!(q["n_ctx"], 131_072, "n_ctx is the argument, not a constant");
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
    // ...and the token is pinned as a LITERAL, not only against the enum
    // that produces it. Comparing the row to `NotConfigured.as_str()`
    // alone moves both sides together under a rename: `tier.rs` holds
    // the only other occurrence in the tree, so the durable vocabulary
    // this row is documented to be the sole producer of could change
    // with nothing failing anywhere.
    assert_eq!(
        p["state"], "not_configured",
        "the durable token is `not_configured`; rows carrying it are already in audit_log"
    );
}

/// `BootRates` reads a probed basis without transposing it — the swap
/// detector at the struct, where the log sites read their values.
///
/// The payload-level detector is
/// `the_fastest_rate_is_tok_per_s_and_the_slowest_is_slowest_tok_per_s`;
/// these two are the only tests in the file that see a transposition,
/// and they see it at different layers on purpose. `from_basis` does no
/// arithmetic, so the exact float equality here is deliberate: it states
/// "carried verbatim", which is exactly the property under test.
#[test]
fn boot_rates_reads_a_probed_basis_without_transposing_it() {
    let rates = BootRates::from_basis(&contended_probed().basis);
    assert_eq!(
        rates,
        BootRates {
            fastest_tok_per_s: Some(6_090.0),
            slowest_tok_per_s: Some(269.6),
            measured_samples: Some(3),
            attempted_samples: Some(3),
        }
    );
}

/// No shape of this row can reach the audit truncation cap.
///
/// `guard_tier.boot` has **no key in `db::audit::PRESERVED_KEYS`**, so
/// an over-cap payload does not merely lose a field — the whole row
/// collapses to a `{_truncated, sha256, len}` fingerprint, taking the
/// timeout basis and the coverage finding with it. The tier would boot,
/// log correctly, and leave a durable record answering none of the
/// questions the row exists for.
///
/// Nothing is close to that today (the largest shape is well under a
/// kilobyte against a 4 KiB cap), and that is exactly why the check
/// belongs here rather than in a comment: what would breach it is a
/// future PR lengthening a `coverage_finding` sentence, which is a
/// change nobody would think to measure.
///
/// Measured with `serde_json::to_vec` — **the exact form
/// `truncate_payload` measures**, rather than the pretty-printed one,
/// which is larger and would make this pass for the wrong reason if it
/// ever started failing. (Not the form Postgres stores: `audit_log.payload`
/// is `jsonb`, a normalised binary encoding with its own size. The cap is
/// applied before the insert, so `to_vec` is the right ruler — but it is
/// the writer's ruler, not the column's.)
///
/// The inputs are chosen to inflate the row, not to be strictly maximal:
/// a `u64::MAX`-millisecond budget so `timeout_ms` renders at its full
/// 20 digits, `tau` at full f32 precision, and an `n_ctx` 8x above the
/// 131 072 the live DGX server reports. A genuinely maximal `n_ctx`
/// would be `u64::MAX` too; the 13 bytes that buys are noise against a
/// margin of roughly 3.4 KiB, and the fixed shape is easier to read.
#[test]
fn no_boot_payload_can_reach_the_audit_truncation_cap() {
    for (basis, _) in every_basis_with_expected_finding() {
        let budget = GuardTimeout { timeout: Duration::from_millis(u64::MAX), basis };
        let kind = budget.basis.kind();
        let len = serde_json::to_vec(&boot_payload(0.795_526_56, 1_048_576, &budget))
            .expect("serde_json::Value cannot fail to serialise")
            .len();
        assert!(
            len <= kastellan_db::audit::PAYLOAD_MAX_BYTES,
            "{kind}: a {len}-byte row would be truncated to a fingerprint by \
             db::audit::insert (cap {})",
            kastellan_db::audit::PAYLOAD_MAX_BYTES,
        );
    }
}

/// The unconfigured row is bounded too, and the loop above cannot see it.
///
/// [`not_configured_payload`] takes no arguments and shares no code with
/// [`boot_payload`], so no basis reaches it — and it is the row every
/// host *without* a guard tier stores, which is most of them.
#[test]
fn the_unconfigured_payload_is_bounded_too() {
    let len = serde_json::to_vec(&not_configured_payload())
        .expect("serde_json::Value cannot fail to serialise")
        .len();
    assert!(
        len <= kastellan_db::audit::PAYLOAD_MAX_BYTES,
        "{len} bytes against a cap of {}",
        kastellan_db::audit::PAYLOAD_MAX_BYTES,
    );
}
