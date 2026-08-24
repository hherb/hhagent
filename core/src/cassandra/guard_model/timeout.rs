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

pub mod basis;

pub use basis::{classify_pin, Clamped, GuardTimeout, PinBand, TimeoutBasis, UnprobedReason};

/// Bytes of dense text in the boot probe.
///
/// **Descriptive, not a parameter.** [`PROBE_BODY`] is a committed
/// literal and nothing resizes it from this constant; the two are tied
/// together by `probe_body_is_exactly_probe_bytes` instead. Changing
/// this number alone changes no behaviour.
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
/// See [`probe_document`], which prefixes a per-boot varying string
/// so the sample is cold.
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

/// The probe document: a per-boot `cache_buster` followed by
/// [`PROBE_BODY`].
///
/// **The cache-buster goes before the body, and that ordering is the
/// mechanism.** A prefix cache matches from position 0 forward, so a
/// varying string ahead of the body guarantees the *body* is never
/// served from cache; a varying *suffix* would leave it cached and
/// reproduce M2's 4x over-estimate. Measured: consecutive cold runs with
/// different busters both reported `cached_tokens: 0` and agreed within
/// 3%.
///
/// **It is not at position 0 of what is actually sent, and the
/// difference matters.** `GuardClient::timed_probe` wraps this string in
/// `policy::build_messages`, which prepends a system message and an
/// `"<Instruct>: … <Query>: … <Document>: "` preamble — roughly 140
/// tokens that are byte-identical on every boot and therefore genuinely
/// cacheable. M2 measured a bare 1024-byte body at 810 prompt tokens,
/// i.e. without that envelope, so production's probe reports more
/// tokens than M2 did and a slice of them may be cache hits. That is
/// handled by subtracting `cached_tokens`, not by this ordering — see
/// [`probe_sample`], and the backend caveat recorded there.
///
/// **Deliberately not called a "nonce".** It is not secret, not
/// authenticating anything, and not protecting against replay — it
/// exists only to make this boot's prompt differ from the last one's.
/// Naming it a nonce overstates its role to a reader, and CodeQL's
/// `rust/hard-coded-cryptographic-value` rule reads the name and flags
/// every caller that passes a literal.
///
/// Pure — the caller supplies the value, so this stays testable.
pub fn probe_document(cache_buster: &str) -> String {
    format!("{cache_buster}\n{PROBE_BODY}")
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
/// **`cached_tokens` is subtracted, not ignored.** Saturating
/// subtraction, so a backend reporting more cached than prompt tokens
/// yields zero rather than wrapping to four billion.
///
/// **An absent block is treated as zero cached, and the uncached-token
/// floor does NOT make that safe.** The floor only bites when the cache
/// *is* reported (M2's contaminated row: 810 − 809 = 1, far below 256).
/// A backend that serves from cache and reports no `cached_tokens` still
/// reports the full `prompt_tokens`, which sails past the floor and
/// inflates throughput — exactly M2's failure, deriving a timeout too
/// short, which is a fail-open. The only defence in that case is the
/// cache-buster in [`probe_document`]; the floor is a defence against a
/// *thin* sample, not against an unreported cache. (An earlier version
/// of this paragraph named the floor as the protection, which is worse
/// than naming none. Carrying the reported/absent distinction into the
/// basis so the boot row can be read later is issue #608.)
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

/// Map a *failed* probe call to an outcome.
///
/// Pure, so the floor/ceiling choice is pinned without a server. The
/// split matters because the two directions are not symmetric: a
/// timeout is an **upper bound on throughput** and must reach the
/// ceiling, while any other failure knows nothing about the host and
/// takes the floor. Sending a timeout to the floor would hand the
/// slowest hosts the shortest guard timeout — the inversion
/// [`derive_guard_timeout`] warns about, arriving one function earlier.
///
/// `timed_out` is supplied by the caller because only the transport can
/// answer it; that one-line boundary is exercised end to end by
/// `guard_tier_e2e::a_probe_that_overruns_its_budget_derives_the_ceiling`.
pub fn probe_error_outcome(timed_out: bool, why: String, budget_ms: u64) -> ProbeOutcome {
    if timed_out {
        ProbeOutcome::Saturated { budget_ms }
    } else {
        ProbeOutcome::Failed { why }
    }
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
pub fn derive_guard_timeout(outcome: &ProbeOutcome) -> GuardTimeout {
    let floor = |reason| GuardTimeout {
        timeout: Duration::from_millis(TIMEOUT_FLOOR_MS),
        basis: TimeoutBasis::Unprobed { reason },
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
                basis: TimeoutBasis::Probed { tok_per_s: tok_per_s as f32, derived_ms, clamped },
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
