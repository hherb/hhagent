//! What the daemon records about the guard tier at boot (issue [#627]).
//!
//! One boot produces one `info!` line, at most one `warn!`, and one
//! durable `policy / guard_tier.boot` audit row. All three report the
//! same facts, and until this module existed all three were built inline
//! in `core/src/main.rs` — a binary crate with no `#[cfg(test)]` module,
//! so the only tests naming that row asserted its **count**.
//!
//! That gap mattered because half of [#624]'s fix lives in the *report*
//! rather than in the probe. Taking the fastest of several samples is
//! worth nothing if the row cannot then distinguish a quiet host from a
//! busy one — and swapping [`BootRates::tok_per_s`] with
//! [`BootRates::slowest_tok_per_s`] inverts the documented operator
//! query `slowest_tok_per_s < tok_per_s / 2` in perfect silence: a
//! contended host stops reporting as contended, and a quiet one starts.
//! Every key set, type and non-null survives that mutation, so only a
//! test that reads the two values back can catch it.
//!
//! **Pure throughout** — no tier, no pool, no clock, no backend. The
//! three scalars [`boot_payload`] takes are exactly what the payload
//! reads, and taking them rather than a `&GuardTier` is the whole point:
//! constructing a tier needs a [`super::GuardClient`], which needs a
//! reachable guard endpoint, which is why this code was unreachable from
//! a unit test in the first place.
//!
//! [#624]: https://github.com/hherb/kastellan/issues/624
//! [#627]: https://github.com/hherb/kastellan/issues/627

use serde_json::{json, Value};

use super::policy::policy_digest;
use super::tier::Unadjudicated;
use super::timeout::{GuardTimeout, TimeoutBasis};

/// The four probe-derived numbers a boot report carries.
///
/// Derived **once** and shared by the `info!` line, the `warn!` finding
/// and the durable row, so the three cannot drift apart. Before this
/// type they were an inline four-tuple destructured in the binary, which
/// is exactly the shape a transposition hides in.
///
/// Every field is an `Option` because every one of them is genuinely
/// absent on some basis, and absence must reach the row as JSON `null`
/// rather than as a fabricated `0`: a wedged backend really can measure
/// `0.0` tok/s, so a zero standing in for "never measured" would be
/// indistinguishable from an observation.
///
/// The fields are `pub` and nothing here enforces `slowest <= fastest` —
/// that invariant belongs to [`super::timeout::summarise`], which
/// produces the basis. This type reports what the basis says; it does
/// not re-decide it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BootRates {
    /// The **fastest** of the probe's measuring samples, which is the
    /// one that measures the *host* — prompt processing has a hardware
    /// ceiling and no floor, so contention can only ever make an
    /// observation slower ([#624]'s D11).
    ///
    /// [#624]: https://github.com/hherb/kastellan/issues/624
    pub tok_per_s: Option<f32>,
    /// The **slowest** of the samples that measured. Together with
    /// [`Self::tok_per_s`] this is the contention spread: two rates that
    /// agree are a measurement of the host, two that differ by 26x are a
    /// measurement of how busy it was at boot.
    pub slowest_tok_per_s: Option<f32>,
    /// How many samples produced a usable rate — the numerator.
    pub measured_samples: Option<u32>,
    /// How many probe calls were actually made — the denominator
    /// [`Self::measured_samples`] is a numerator of.
    ///
    /// Present on every basis a probe produced, not only the measuring
    /// one: on `Saturated`/`Unprobed` it is the *strength of the
    /// evidence* behind the finding, and one failed call predicts a
    /// wholly fail-open tier far more weakly than three.
    ///
    /// Without it, `measured_samples: 1` reads three ways at once — one
    /// sample that worked, or three of which two were served from cache,
    /// or three of which two failed outright — and a reader cannot even
    /// recover `PROBE_SAMPLES`, which is a tunable. A row with
    /// `attempted_samples > measured_samples` and no `coverage_finding`
    /// is the one that says "read that boot's `warn!` lines".
    pub attempted_samples: Option<u32>,
}

impl BootRates {
    /// Read the reportable numbers out of a timeout's provenance.
    ///
    /// Pure and total: every basis maps, and the ones that measured
    /// nothing map to `None` rather than to a stand-in value.
    pub fn from_basis(basis: &TimeoutBasis) -> Self {
        // Two matches rather than one, because the two questions have
        // different answers: "did this probe MEASURE anything?" is true
        // only of `Probed`, while "did a probe RUN at all?" is true of
        // every basis except an operator pin.
        let (tok_per_s, slowest_tok_per_s, measured_samples) = match basis {
            TimeoutBasis::Probed { tok_per_s, slowest_tok_per_s, measured_samples, .. } => {
                (Some(*tok_per_s), Some(*slowest_tok_per_s), Some(*measured_samples))
            }
            TimeoutBasis::Operator { .. }
            | TimeoutBasis::Saturated { .. }
            | TimeoutBasis::Unprobed { .. } => (None, None, None),
        };
        let attempted_samples = match basis {
            TimeoutBasis::Probed { attempted_samples, .. }
            | TimeoutBasis::Saturated { attempted_samples, .. }
            | TimeoutBasis::Unprobed { attempted_samples, .. } => Some(*attempted_samples),
            TimeoutBasis::Operator { .. } => None,
        };
        Self { tok_per_s, slowest_tok_per_s, measured_samples, attempted_samples }
    }
}

/// The budget in milliseconds, as every boot report spells it.
///
/// A one-line derivation, given its own name for one reason: the
/// `info!` line, the `warn!` finding and the durable row all report it,
/// and until this existed the log sites computed it from a local while
/// the payload computed it again. Two copies of `as_millis() as u64`
/// agree today and are a silent divergence the day one of them changes.
pub fn timeout_ms(budget: &GuardTimeout) -> u64 {
    budget.timeout.as_millis() as u64
}

/// The durable `policy / guard_tier.boot` payload for a **configured**
/// tier.
///
/// Takes `tau` and `n_ctx` as plain scalars, and the budget for its
/// provenance, rather than the tier that holds all three — see the
/// module docs for why that is the design and not a convenience.
///
/// Key notes for a reader of the stored rows:
///
/// * `timeout_basis` is [`TimeoutBasis::kind`], which folds
///   [`super::timeout::Clamped::ToCeiling`] into a bare `"probed"`. That
///   is why `coverage_finding` is a key of its own: without it, the row
///   for a host that cannot adjudicate a worst-case document looks
///   exactly like a healthy one.
/// * `coverage_finding` is `null` when the boot was routine, so the
///   query for affected hosts is
///   `WHERE payload->>'coverage_finding' IS NOT NULL`. Tracing logs
///   rotate; `audit_log` does not.
/// * The key set never shrinks. A basis that measured nothing still
///   carries `tok_per_s`, `slowest_tok_per_s` and `measured_samples`,
///   holding `null` — so a reader querying one of them finds the key on
///   every row rather than only on the hosts that happened to probe.
pub fn boot_payload(tau: f32, n_ctx: u64, budget: &GuardTimeout) -> Value {
    let rates = BootRates::from_basis(&budget.basis);
    json!({
        "configured":        true,
        "tau":               tau,
        "timeout_ms":        timeout_ms(budget),
        "timeout_basis":     budget.basis.kind(),
        "tok_per_s":         rates.tok_per_s,
        "slowest_tok_per_s": rates.slowest_tok_per_s,
        "measured_samples":  rates.measured_samples,
        "attempted_samples": rates.attempted_samples,
        "n_ctx":             n_ctx,
        "policy_digest":     policy_digest(),
        "coverage_finding":  budget.basis.coverage_finding(),
    })
}

/// The durable payload for a host with **no** guard tier configured.
///
/// Spells "no tier ran" with the same token the per-dispatch
/// `guard.state` vocabulary uses ([`Unadjudicated::NotConfigured`]), so
/// the question has one spelling across the audit log rather than a live
/// half and an orphaned half. This boot row is deliberately the ONLY
/// producer of it: a per-dispatch `not_configured` field would be a
/// constant on every row of an unconfigured host.
pub fn not_configured_payload() -> Value {
    json!({
        "configured": false,
        "state":      Unadjudicated::NotConfigured.as_str(),
    })
}

#[cfg(test)]
mod tests;
