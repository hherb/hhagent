//! The guard tier's request timeout, measured rather than assumed
//! (issue [#586], wiring-spec D9).
//!
//! # Why a constant was wrong
//!
//! D2 derived 15 s from one host and one token count: measurement 1's
//! size sweep put a 64 KiB document at 10,062 prompt tokens and ~3.5 s
//! on the DGX, and 15 s is ~4x that. Measurement 3 broke both halves.
//! The token count was prose-shaped — the same 64 KiB of adversarial
//! text tokenises to **44,437** ([#604]) — and the host was not
//! representative: the Mac takes **~5.5 minutes** on a document D2's
//! arithmetic budgets at 15 s.
//!
//! The failure is one-directional and silent. **Too short a guard
//! timeout does not error; it fails open** — the tier is escalate-up
//! only, so a timeout means the document reaches the planner
//! unscreened. A constant that is wrong by 40x on a first-class host is
//! a security control that is off without saying so.
//!
//! # What is measured, and why the measurement is trustworthy
//!
//! Measurement 1 established the shape: the tier's cost is **entirely
//! prompt processing and linear in tokens** (decode is one token at
//! 0.00 ms). So one host's worst case follows from that host's
//! prompt-eval throughput, and throughput is cheap to measure — a
//! ~1 KiB probe stands in for a 64 KiB document at 1/64th the cost.
//!
//! Two things could corrupt that probe, and both are handled rather
//! than hoped away (M2, 2026-08-23, DGX):
//!
//! 1. **Prefix caching.** llama-server serves a repeated prompt from
//!    cache. Measured: a repeated 810-token document came back in 38 ms
//!    with `cached_tokens: 809`, which a naive `tokens / elapsed` reads
//!    as **21,094 tok/s** against the same server's true ~5,000 — a 4x
//!    over-estimate, deriving a timeout 4x too short. Two defences: the
//!    probe document carries a per-boot cache-busting **prefix** (measured to
//!    give `cached_tokens: 0` on consecutive cold runs, agreeing within
//!    3%), and throughput is computed over **uncached** tokens only, so
//!    a cache hit shrinks the sample rather than inflating the rate.
//! 2. **Tokenisation density.** A prose probe would measure bytes/token
//!    at ~6.5 and a worst-case document runs at ~1. The probe body is
//!    deliberately token-dense: measured at **1.26 bytes/token**, close
//!    to [#604]'s 1.47 on real jailbreak text.
//!
//! # The shape of this module
//!
//! Everything here is **pure**, and it is three files because it is
//! three questions:
//!
//! * [`sample`] — what ONE measurement of this backend is. The IO half
//!   produces a [`ProbeReading`]; [`probe_sample`] turns that into a
//!   [`ProbeOutcome`].
//! * this file — how an outcome becomes a budget
//!   ([`derive_guard_timeout`]), and what an operator's own number is
//!   allowed to be ([`validate_operator_timeout`]).
//! * [`basis`] — how that budget describes its own provenance.
//!
//! Every row of both tables in D9 is therefore a unit test with no
//! server. The two child modules are re-exported here, so
//! `timeout::probe_sample` and every other historic path still
//! resolves.
//!
//! [#586]: https://github.com/hherb/kastellan/issues/586
//! [#604]: https://github.com/hherb/kastellan/issues/604

use std::time::Duration;

use super::context_pin::REQUIRED_GUARD_N_CTX;

pub mod basis;
pub mod sample;

pub use basis::{classify_pin, Clamped, GuardTimeout, PinBand, TimeoutBasis, UnprobedReason};
pub use sample::{
    more_samples_wanted, probe_document, probe_error_outcome, probe_sample,
    sample_cache_buster, sample_tok_per_s, summarise, ProbeOutcome, ProbeReading,
    ProbeSummary, MIN_UNCACHED_PROBE_TOKENS, PROBE_BUDGET_MS, PROBE_BYTES, PROBE_SAMPLES,
    PROBE_TOTAL_BUDGET_MS,
};

/// Multiplier applied to the derived worst case.
///
/// Covers what measurement 1's open risk 3 left unmeasured: on a
/// single-host deployment the guard shares the GPU with the planner,
/// and under contention with a 26B model these numbers get worse by an
/// amount nobody has measured.
pub const PROBE_SAFETY_FACTOR: f32 = 2.0;

/// The shortest timeout that may be derived.
///
/// D2's number, kept as a floor because a *shorter* timeout is a
/// *weaker* control: it converts adjudications into fail-opens. A fast
/// host derives less than this and simply gets D2's value.
pub const TIMEOUT_FLOOR_MS: u64 = 15_000;

/// The longest timeout that may be derived.
///
/// Past this, stalling a dispatch is worse than degrading to
/// catalogue-only screening. Reaching it is a **finding about the
/// host**, not a routine clamp — see [`Clamped::ToCeiling`].
pub const TIMEOUT_CEILING_MS: u64 = 120_000;

/// The worst-case prompt the timeout must cover.
///
/// The same figure [`REQUIRED_GUARD_N_CTX`] pins, and deliberately the
/// same one: D8 refuses to boot a server that cannot *hold* this many
/// tokens, so budgeting for any smaller number here would leave a
/// document the server accepts and the timeout does not.
pub const WORST_CASE_TOKENS: u64 = REQUIRED_GUARD_N_CTX;

/// Clamp `derived_ms` into the band and say which bound it hit.
///
/// Pure, and separate from [`derive_guard_timeout`] so the band is one
/// thing rather than three inline comparisons.
fn clamp_derived(derived_ms: u64) -> (u64, Clamped) {
    if derived_ms < TIMEOUT_FLOOR_MS {
        (TIMEOUT_FLOOR_MS, Clamped::ToFloor)
    } else if derived_ms > TIMEOUT_CEILING_MS {
        (TIMEOUT_CEILING_MS, Clamped::ToCeiling)
    } else {
        (derived_ms, Clamped::No)
    }
}

/// Derive a guard timeout from a probe summary.
///
/// **The summary's `best` is the FASTEST of [`PROBE_SAMPLES`] samples,
/// not a single observation** (issue [#624]) — see [`summarise`] for why
/// the maximum is the right estimator and why correcting toward it is
/// worth moving the budget down. Everything below is unchanged by that:
/// the arithmetic acts on one sample either way, and the spread rides
/// along on the basis so a later reader can see how much the samples
/// disagreed.
///
/// ```text
/// tok_per_s  = uncached_tokens / (elapsed_ms / 1000)
/// derived_ms = WORST_CASE_TOKENS / tok_per_s * 1000 * PROBE_SAFETY_FACTOR
/// timeout    = clamp(derived_ms, TIMEOUT_FLOOR_MS, TIMEOUT_CEILING_MS)
/// ```
///
/// ⚠️ **This is a LINEAR extrapolation from a ~1 KiB sample, and on one
/// of the two supported platforms the linearity is false by 4.4x
/// ([#612](https://github.com/hherb/kastellan/issues/612)).**
///
/// Two *different* samples are involved, and conflating them is how these
/// numbers stop adding up. A **size sweep** with identical dense filler
/// (1.47 B/token) measures the *shape*: the DGX (CUDA) holds 3 177 tok/s
/// at 1 KiB, 6 327 at 8 KiB and 2 907 at 64 KiB; the Mac (Metal) holds
/// 1 137, 1 209, and **260**. Neither curve is flat — but the DGX's
/// 1 KiB reading sits *below* its 64 KiB one, so extrapolating from the
/// probe's sample errs in the **safe** direction there, which is the
/// property that matters and not flatness. The **boot probe** measures
/// the rate this formula is actually fed, on its own denser body
/// (1.26 B/token — see the module note above), and so reads higher on
/// both hosts: 6 073 tok/s on the DGX → a 21.8 s budget, and ~1 445 on
/// the Mac → 91 s. Do not try to derive one host's budget from the other
/// table's tok/s; they are not the same measurement.
///
/// The consequence is the Mac's alone. A worst-case 64 KiB document
/// really takes ~171 s there, against that derived 91 s, so the
/// adjudication times out — which, as this module's own note above says,
/// does not error but **fails open**. [`PROBE_SAFETY_FACTOR`]'s 2x does
/// not cover a 4.4x error, and the knee sits above the 8 KiB sample, so a
/// cheap second probe would not find it.
///
/// Multi-sampling does **not** close #612, and the two must not be
/// confused: #624 is that the *sample* was taken under load, #612 is
/// that extrapolating from a ~1 KiB sample is non-linear on Metal
/// whatever the load. A perfectly quiet Mac still reads ~1 137 tok/s at
/// 1 KiB and 260 at 64 KiB.
///
/// Until #612 is settled a Metal host should pin
/// `KASTELLAN_LLM_GUARD_TIMEOUT_MS` rather than trust the probe — at
/// **≥ ~350 s**. Where that comes from, since 171 s is the measured
/// number and neither figure is the other: the 171 s used 1.47 B/token
/// filler, i.e. ~44 400 tokens, while [`WORST_CASE_TOKENS`] (66 048)
/// budgets for the ~1 B/token adversarial ceiling
/// [`super::context_pin`] argues for. Scaling by tokens alone gives
/// 66 048 ÷ 260 tok/s ≈ **254 s** — and 254 s is the number that
/// *follows*. The recommendation is deliberately above it, because 260
/// tok/s was itself measured at 64 KiB and the curve is still falling
/// there: extrapolating a decaying rate linearly is the same mistake
/// this whole block is about. ~350 s is a floor with headroom for a knee
/// nobody has characterised, not a derivation — treat it as such, and
/// measure your own host with `live_boot_probe_derives_this_hosts_timeout`.
///
/// Note that pinning **skips the probe entirely**
/// ([`TimeoutBasis::Operator`]) and that `validate_operator_timeout` does
/// *not* clamp the pinned value to the range below — both deliberate,
/// both worth knowing before you read a boot line as a measurement, and
/// together the reason a pin is an operator decision rather than a new
/// default. A pin outside the range below is still honoured verbatim,
/// but since #615 it is no longer applied in *silence*: [`classify_pin`]
/// puts a [`PinBand`] on the basis, which earns a `warn!` and a
/// `coverage_finding` in the durable boot row. The ~350 s recommended
/// above is deliberately one of those — following this advice is a trade
/// (an unbounded per-dispatch stall in exchange for not failing open),
/// and it belongs on the record.
///
/// **[`ProbeOutcome::Saturated`] derives the CEILING, not the floor**,
/// and that is the one row a plausible implementation gets backwards. A
/// probe that overran its budget is an upper bound on throughput — the
/// only outcome that says *this host is slow*. Sending it to the floor
/// would give the slowest hosts the shortest timeout, which is exactly
/// inverted.
///
/// Every other non-measuring outcome takes the floor: nothing is known
/// about the host, and the floor is the value D2 shipped.
///
/// Pure.
pub fn derive_guard_timeout(summary: &ProbeSummary) -> GuardTimeout {
    let floor = |reason| GuardTimeout {
        timeout: Duration::from_millis(TIMEOUT_FLOOR_MS),
        basis: TimeoutBasis::Unprobed { reason },
    };
    match &summary.best {
        ProbeOutcome::Measured { uncached_tokens, elapsed_ms } => {
            let tok_per_s = f64::from(*uncached_tokens) / (*elapsed_ms as f64 / 1000.0);
            // `Measured` is only constructed with a positive token
            // count and a non-zero wall clock (see `probe_sample`), so
            // `tok_per_s` is finite and positive here. Guarded anyway:
            // this is a security control, and "unreachable" is a
            // property of another function.
            if !tok_per_s.is_finite() || tok_per_s <= 0.0 {
                return floor(UnprobedReason::Nonsensical);
            }
            let derived = WORST_CASE_TOKENS as f64 / tok_per_s
                * 1000.0
                * f64::from(PROBE_SAFETY_FACTOR);
            // Saturating: a pathologically slow probe can derive more
            // than u64::MAX ms, and that must land on the ceiling, not
            // wrap to a tiny number.
            let derived_ms = if derived >= u64::MAX as f64 {
                u64::MAX
            } else {
                derived.ceil() as u64
            };
            let (timeout_ms, clamped) = clamp_derived(derived_ms);
            GuardTimeout {
                timeout: Duration::from_millis(timeout_ms),
                basis: TimeoutBasis::Probed {
                    tok_per_s: tok_per_s as f32,
                    // `summarise` never reports a slowest without a
                    // rate to go with it, and never reports zero
                    // measuring samples beside a `Measured` best. Both
                    // are guarded rather than asserted: a hand-built
                    // summary must not be able to write a
                    // self-contradicting durable row, and the honest
                    // reading of "no recorded slowest" is that this is
                    // the only rate there is.
                    slowest_tok_per_s: summary
                        .slowest_tok_per_s
                        .unwrap_or(tok_per_s as f32),
                    measured_samples: summary.measured_samples.max(1),
                    derived_ms,
                    clamped,
                },
            }
        }
        // An overrun budget IS a measurement of slowness, so it takes the
        // CEILING and not the floor. But it measures no THROUGHPUT, so it
        // reports none: see `TimeoutBasis::Saturated`.
        ProbeOutcome::Saturated { budget_ms } => GuardTimeout {
            timeout: Duration::from_millis(TIMEOUT_CEILING_MS),
            basis: TimeoutBasis::Saturated { budget_ms: *budget_ms },
        },
        ProbeOutcome::TooFewUncachedTokens { .. } => floor(UnprobedReason::TooFewUncachedTokens),
        ProbeOutcome::NoTokenCount => floor(UnprobedReason::NoTokenCount),
        ProbeOutcome::Failed { .. } => floor(UnprobedReason::Failed),
    }
}

/// Why an operator-supplied guard timeout is unusable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutError {
    /// `KASTELLAN_LLM_GUARD_TIMEOUT_MS=0`.
    ///
    /// No HTTP request completes in zero milliseconds, so every
    /// adjudication would time out and take the fail-open door: the tier
    /// would look configured, log as configured, and be off. That is the
    /// same silent failure [`super::tier::validate_tau`] refuses at both
    /// ends of the threshold range, reached through the timeout instead.
    Zero,
}

impl std::fmt::Display for TimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Zero => write!(
                f,
                "KASTELLAN_LLM_GUARD_TIMEOUT_MS is 0. No request completes in zero \
                 milliseconds, so every adjudication would time out and fail OPEN -- the \
                 tier would be configured, logged as configured, and off. Unset it to \
                 derive a budget from a boot probe, or set a positive value."
            ),
        }
    }
}

impl std::error::Error for TimeoutError {}

/// Accept an operator-pinned timeout verbatim, refusing only the value
/// that cannot work — and **say so when it is out of band**.
///
/// **Deliberately NOT clamped to the derivation band.** The band
/// constrains what this module may *infer*; an operator who pinned a
/// number has already decided, and silently overriding them would make
/// the env var advisory. What is refused is zero — not because it is
/// unwise but because it is unusable, the same line
/// [`super::tier::validate_tau`] draws.
///
/// **Not clamping is not the same as not reporting** (issue [#615]).
/// Until it carried a [`PinBand`], this function applied a pin at either
/// extreme in silence, and each extreme is a real exposure: a pin below
/// [`TIMEOUT_FLOOR_MS`] turns adjudications into fail-opens, and one
/// above [`TIMEOUT_CEILING_MS`] buys an unbounded per-dispatch stall —
/// which is what issue #612 currently tells a Metal operator to do. The
/// band rides on the basis, so it reaches the `warn!` and the durable
/// `policy / guard_tier.boot` row through
/// [`TimeoutBasis::coverage_finding`] with no new plumbing.
///
/// Pure.
///
/// [#615]: https://github.com/hherb/kastellan/issues/615
pub fn validate_operator_timeout(ms: u64) -> Result<GuardTimeout, TimeoutError> {
    if ms == 0 {
        return Err(TimeoutError::Zero);
    }
    Ok(GuardTimeout {
        timeout: Duration::from_millis(ms),
        basis: TimeoutBasis::Operator { band: classify_pin(ms) },
    })
}

#[cfg(test)]
mod tests;
