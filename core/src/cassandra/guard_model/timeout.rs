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
//!    probe document carries a per-boot nonce **prefix** (measured to
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
//! Everything here is **pure**. The IO half produces a
//! [`ProbeReading`]; [`probe_sample`] turns that into a
//! [`ProbeOutcome`], and [`derive_guard_timeout`] turns the outcome
//! into a [`GuardTimeout`]. Every row of both tables in D9 is therefore
//! a unit test with no server.
//!
//! [#586]: https://github.com/hherb/kastellan/issues/586
//! [#604]: https://github.com/hherb/kastellan/issues/604

use std::time::Duration;

use super::context_pin::REQUIRED_GUARD_N_CTX;

/// Bytes of dense text in the boot probe.
///
/// Measured (M2): 1024 dense bytes tokenise to **810 tokens**, which
/// takes ~160 ms on the DGX and would take ~8 s on a 100 tok/s host —
/// comfortably inside [`PROBE_BUDGET_MS`] either way, and well above
/// [`MIN_UNCACHED_PROBE_TOKENS`].
pub const PROBE_BYTES: usize = 1024;

/// Multiplier applied to the derived worst case.
///
/// Covers what measurement 1's open risk 3 left unmeasured: on a
/// single-host deployment the guard shares the GPU with the planner,
/// and under contention with a 26B model these numbers get worse by an
/// amount nobody has measured.
pub const PROBE_SAFETY_FACTOR: f32 = 2.0;

/// Below this many *uncached* prompt tokens, a sample is rejected.
///
/// Small prompts are dominated by fixed per-request overhead in one
/// direction and by cache hits in the other; M2's contaminated row read
/// **one** uncached token. A sample that thin is noise, and dividing by
/// it produces a throughput that is not about this host at all.
pub const MIN_UNCACHED_PROBE_TOKENS: u32 = 256;

/// How long the boot probe may take before it is abandoned.
///
/// Bounds what a slow host adds to daemon startup. Exceeding it is not
/// a missing measurement — see [`ProbeOutcome::Saturated`].
pub const PROBE_BUDGET_MS: u64 = 20_000;

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

/// The fixed body of the boot probe: token-dense text, no prose.
///
/// Deterministic and committed so a measurement is comparable across
/// boots and across hosts. Density is what matters, not content — the
/// guard's verdict on it is discarded; only the timing is read.
///
/// See [`probe_document`], which prefixes a per-boot nonce so the
/// sample is cold.
const PROBE_BODY: &str = concat!(
    "PtYgj}mU~h=Bel31iEl]2h>pC~h|~YgCf<rL1s[p|N<xn~|yVm]i>hA/}2O7~6UM",
    "FxFk|M{/R5Kjp_1vRt+1fj<|ORS/~6ilI8ihN|5KXSc7Tvo/hBKqFYY/kv5Z]Jr3",
    "]J1TWDtkwtDDb+^xHKas1}V>Oq_g6<YYZYn9ZhyiA4uoRgna>t}mUdjAWtGSU8po",
    "+799NksnRH9u-cA{Us[d{MlH-UvTC}[=QCyEZDz-/TddJ8HyS5SUkCnD8zRA9a9S",
    "kpXz9w3QlY7Zkuvqdt^7s8St]]qcbn{r3yBdGBL=E^PH[1qhT6~-1=q}t{_c4xat",
    "ws8p<hP-{<9n<hFyJfm=5<di4P=_zJ5_}9=F-H<z5r1pY4OjE2jBMptUsGr7CmY+",
    "uCu3_ZR1zTOlUcR]64cXQ-L_ioDnkHIfxIq2HZt}_|/PlJhx2jIclHkCiHp6bR]1",
    "Iqf{EouHgxzNN{AL5=wIScGebc=]y_8F5n3/[Y=NBDRzrZSgqbjG3uhkW=KFLf6x",
    "uI5aHUQ]PFeNBTxaQWk8J=zF=alHlsZ^fYcMMDk~{tXP/tKsf_2=r{=>c~DkdfrU",
    "nW5<gc}F+Ha6i=}l{i8GjHEAD6/Wj9KfzjsQGM>rb9h+ImB+L-K777p]zNk8cL6j",
    "=5IXAAj~ls{HUq_JoUD/+Ydua+5ZMs1SWOpQaPRYpzbLGViYX^jU2JgJngKtFI3_",
    "OyV2dZ]]Akg05rK+g]qv81RKMGHZEM9<YpvujA=/]C5Q52r]yFlwR<lOEVH>zc0X",
    "0{AWIRh/J|Uq={BlIFXZ53Ncqe28^+ajY{75FnCtt-n6k]faqD>eMqG{3omjM{~y",
    "XHCab}M6JOF8{E]Fd0Nhcy/1kGD2VD/eR1UYzaL=iA/zNyD7CHLn/xC+1hsYgBds",
    "1ghxY5OokvQyx{7eNWVQ4vnakJkS1p<AWTN3lg8zV[5yPU8d0FZfWe7ihGyiRUIQ",
    "fHOJMaidDn87XG3/q/xbMtEPO6Uk_zYuF0ie9][Pu2njHkAm1/5wDr16E}pLLJ>I",
);

/// What the IO half of the probe observed.
///
/// The two token counts are `Option` because a backend may report the
/// `usage` block, part of it, or none of it — Ollama's OpenAI front
/// door omits it entirely. **Absence of `cached_tokens` means "this
/// backend does not report a cache", never "nothing was cached"**, and
/// [`probe_sample`] is careful about the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeReading {
    pub prompt_tokens: Option<u32>,
    pub cached_tokens: Option<u32>,
    pub elapsed_ms: u64,
}

/// What the boot probe concluded.
///
/// **Every variant is a value, never an error.** The probe picks a
/// number; it does not verify a control. D8's context check is what
/// may stop a boot, and a probe that could not measure must not undo
/// that decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// A usable sample: this many tokens were genuinely processed, in
    /// this much wall clock.
    Measured { uncached_tokens: u32, elapsed_ms: u64 },
    /// The call succeeded but too little of it was real work to divide
    /// by — a cache hit, or a backend that tokenised the probe far
    /// smaller than expected.
    TooFewUncachedTokens { uncached_tokens: u32, elapsed_ms: u64 },
    /// The backend reported no `usage.prompt_tokens`, so there is a
    /// wall clock and nothing to divide it into.
    NoTokenCount,
    /// The probe exceeded [`PROBE_BUDGET_MS`].
    ///
    /// **This is a measurement, not its absence** — an upper bound on
    /// throughput, and the only outcome that says the host is slow.
    Saturated { budget_ms: u64 },
    /// Transport or HTTP failure.
    Failed { why: String },
}

/// Whether the derived value hit a bound, and which one.
///
/// The two are not symmetric and must not be reported as if they were.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clamped {
    /// The derivation landed inside the band.
    No,
    /// A fast host derived less than [`TIMEOUT_FLOOR_MS`]. Unremarkable.
    ToFloor,
    /// The host cannot adjudicate a worst-case document inside
    /// [`TIMEOUT_CEILING_MS`]. **A finding**: large dense documents on
    /// this host will time out and fail open to catalogue-only
    /// screening.
    ToCeiling,
}

/// Where a guard timeout came from, in enough detail to log it.
#[derive(Debug, Clone, PartialEq)]
pub enum TimeoutBasis {
    /// `KASTELLAN_LLM_GUARD_TIMEOUT_MS`. No probe was run.
    Operator,
    /// Derived from a boot probe.
    Probed { tok_per_s: f32, derived_ms: u64, clamped: Clamped },
    /// The probe could not produce a usable sample. Carries the short
    /// reason so a boot line can say which.
    Unprobed { why: &'static str },
}

impl TimeoutBasis {
    /// A short, stable, whitespace-free token for a log field.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Probed { .. } => "probed",
            Self::Unprobed { why } => why,
        }
    }

    /// Does this basis warrant a `warn!` rather than an `info!`?
    ///
    /// True only for [`Clamped::ToCeiling`], which is the one basis
    /// that reports a **reduction in coverage**: on this host large
    /// documents will not be adjudicated at all. Every other basis is
    /// routine, and warning about routine things is how the one that
    /// matters gets scrolled past.
    pub fn is_coverage_finding(&self) -> bool {
        matches!(self, Self::Probed { clamped: Clamped::ToCeiling, .. })
    }
}

/// A guard timeout together with how it was arrived at.
#[derive(Debug, Clone, PartialEq)]
pub struct GuardTimeout {
    pub timeout: Duration,
    pub basis: TimeoutBasis,
}

/// The probe document: a per-boot `nonce` followed by [`PROBE_BODY`].
///
/// **The nonce goes first, and that is the whole mechanism.** A prefix
/// cache matches from position 0, so a varying prefix guarantees the
/// sample is cold; a varying *suffix* would leave the body cached and
/// reproduce M2's 4x over-estimate. Measured: consecutive cold runs
/// with different nonces both reported `cached_tokens: 0` and agreed
/// within 3%.
///
/// Pure — the caller supplies the nonce, so this stays testable.
pub fn probe_document(nonce: &str) -> String {
    format!("{nonce}\n{PROBE_BODY}")
}

/// Turn one raw reading into an outcome.
///
/// Pure. Three refusals, in the order they can arise:
///
/// * no `prompt_tokens` at all -> [`ProbeOutcome::NoTokenCount`];
/// * fewer than [`MIN_UNCACHED_PROBE_TOKENS`] genuinely processed ->
///   [`ProbeOutcome::TooFewUncachedTokens`];
/// * a non-positive wall clock -> also `TooFewUncachedTokens`, because
///   dividing by it is the same mistake wearing a different hat.
///
/// **`cached_tokens` is subtracted, not ignored.** An absent block
/// means the backend reports no cache and is treated as zero cached —
/// which is safe only because the uncached-token floor still applies to
/// the result, and because the nonce prefix makes a cache hit unlikely
/// in the first place. Saturating subtraction, so a backend reporting
/// more cached than prompt tokens yields zero rather than wrapping to
/// four billion.
pub fn probe_sample(reading: ProbeReading) -> ProbeOutcome {
    let Some(prompt_tokens) = reading.prompt_tokens else {
        return ProbeOutcome::NoTokenCount;
    };
    let uncached_tokens = prompt_tokens.saturating_sub(reading.cached_tokens.unwrap_or(0));
    if uncached_tokens < MIN_UNCACHED_PROBE_TOKENS || reading.elapsed_ms == 0 {
        return ProbeOutcome::TooFewUncachedTokens {
            uncached_tokens,
            elapsed_ms: reading.elapsed_ms,
        };
    }
    ProbeOutcome::Measured { uncached_tokens, elapsed_ms: reading.elapsed_ms }
}

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

/// Derive a guard timeout from a probe outcome.
///
/// ```text
/// tok_per_s  = uncached_tokens / (elapsed_ms / 1000)
/// derived_ms = WORST_CASE_TOKENS / tok_per_s * 1000 * PROBE_SAFETY_FACTOR
/// timeout    = clamp(derived_ms, TIMEOUT_FLOOR_MS, TIMEOUT_CEILING_MS)
/// ```
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
pub fn derive_guard_timeout(outcome: &ProbeOutcome) -> GuardTimeout {
    let floor = |why| GuardTimeout {
        timeout: Duration::from_millis(TIMEOUT_FLOOR_MS),
        basis: TimeoutBasis::Unprobed { why },
    };
    match outcome {
        ProbeOutcome::Measured { uncached_tokens, elapsed_ms } => {
            let tok_per_s = f64::from(*uncached_tokens) / (*elapsed_ms as f64 / 1000.0);
            // `Measured` is only constructed with a positive token
            // count and a non-zero wall clock (see `probe_sample`), so
            // `tok_per_s` is finite and positive here. Guarded anyway:
            // this is a security control, and "unreachable" is a
            // property of another function.
            if !tok_per_s.is_finite() || tok_per_s <= 0.0 {
                return floor("probe-nonsensical");
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
                basis: TimeoutBasis::Probed { tok_per_s: tok_per_s as f32, derived_ms, clamped },
            }
        }
        // An upper bound on throughput IS a measurement of slowness.
        ProbeOutcome::Saturated { budget_ms } => {
            let tok_per_s = f64::from(MIN_UNCACHED_PROBE_TOKENS) / (*budget_ms as f64 / 1000.0);
            GuardTimeout {
                timeout: Duration::from_millis(TIMEOUT_CEILING_MS),
                basis: TimeoutBasis::Probed {
                    tok_per_s: tok_per_s as f32,
                    derived_ms: TIMEOUT_CEILING_MS,
                    clamped: Clamped::ToCeiling,
                },
            }
        }
        ProbeOutcome::TooFewUncachedTokens { .. } => floor("probe-too-few-uncached-tokens"),
        ProbeOutcome::NoTokenCount => floor("probe-no-token-count"),
        ProbeOutcome::Failed { .. } => floor("probe-failed"),
    }
}

/// The operator's override, or the derivation.
///
/// **The override wins without consulting the outcome at all**, which
/// is why it takes the outcome by reference and may ignore it: an
/// operator who pinned a number has already decided, and re-deriving
/// underneath them would make the env var advisory.
///
/// Pure.
pub fn guard_timeout_from(override_ms: Option<u64>, outcome: &ProbeOutcome) -> GuardTimeout {
    match override_ms {
        Some(ms) => {
            GuardTimeout { timeout: Duration::from_millis(ms), basis: TimeoutBasis::Operator }
        }
        None => derive_guard_timeout(outcome),
    }
}

#[cfg(test)]
mod tests;
