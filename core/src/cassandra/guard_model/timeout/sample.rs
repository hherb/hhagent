//! One probe sample: what the IO half observes, and what that means
//! (wiring-spec D9).
//!
//! Split out of [`super`] to keep that file under the 500-LOC cap, and
//! because these types are one coherent thing: **what a single
//! measurement of this backend is**, as distinct from the arithmetic
//! that turns one into a budget ([`super::derive_guard_timeout`]) and
//! from how that budget describes its own provenance
//! ([`super::basis`]).
//!
//! Everything here is pure. The IO half produces a [`ProbeReading`];
//! [`probe_sample`] turns that into a [`ProbeOutcome`]. No clock, no
//! socket, no server — so every row of D9's outcome table is a unit
//! test.
//!
//! Re-exported from [`super`], so `timeout::probe_sample` and every
//! other historic path still resolves; this split moved code, not
//! names.

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
/// [`super::derive_guard_timeout`] warns about, arriving one function earlier.
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


// ── Many samples, one number (issue #624) ────────────────────────────

/// How many probe samples are taken before one throughput is chosen.
///
/// **One sample was not a measurement of the host** (issue [#624]). The
/// probe runs ~3 s into daemon startup, while Postgres, 15 workers, the
/// Matrix channel and the audit mirror are all still coming up, so it
/// measures the host *under startup contention*. Three consecutive boots
/// on one unchanged DGX backend derived 21 752 / 120 000 / 83 489 ms
/// from 6 073 / 269.6 / 1 582 tok/s, while that same backend measured a
/// reproducible ~7 000 tok/s uncontended minutes later — a 26x
/// under-measurement, and the 269.6 run clamped to the ceiling and fired
/// a **false** "this host cannot adjudicate a worst-case document"
/// finding.
///
/// Three rather than two because the middle sample is what makes a
/// *spread* visible: with two, a reader cannot tell a quiet host from
/// one that happened to be quiet once. Three rather than five because
/// each sample costs real boot time on the host that needs the budget
/// most — see [`PROBE_TOTAL_BUDGET_MS`].
///
/// [#624]: https://github.com/hherb/kastellan/issues/624
pub const PROBE_SAMPLES: usize = 3;

/// Wall clock the whole probe may spend, across **all** samples.
///
/// Deliberately equal to [`PROBE_BUDGET_MS`], the budget one sample used
/// to cost, so that going multi-sample does not lengthen a *healthy*
/// boot at all: a DGX sample is ~160 ms and a Mac one under a second, so
/// all [`PROBE_SAMPLES`] fit inside this with two orders of magnitude to
/// spare.
///
/// **The bound it actually gives is `PROBE_TOTAL_BUDGET_MS +
/// PROBE_BUDGET_MS`, and that is deliberate rather than sloppy.**
/// [`more_samples_wanted`] is consulted *before* a sample, so a sample
/// that starts just under the total may still run its own full budget.
/// Making the guarantee tight would mean either shortening the
/// per-sample budget (which redefines [`ProbeOutcome::Saturated`] and
/// would saturate hosts the current budget measures fine) or refusing to
/// start any sample that could overrun (which, since the two budgets are
/// equal, means never taking a second one). The overrun is reachable
/// only when a sample returns just under 20 s — a host already deriving
/// the ceiling and already emitting a coverage finding.
///
/// **A probe whose FIRST sample saturates costs exactly one budget and
/// stops**, because the elapsed check then fails on its own, with no
/// special case needed for it.
pub const PROBE_TOTAL_BUDGET_MS: u64 = PROBE_BUDGET_MS;

/// The cache-buster for sample `index` of this boot's probe.
///
/// **Per-sample, and that is load-bearing rather than tidy.** The
/// cache-buster exists to make a sample cold ([`probe_document`]); N
/// samples sharing one buster would send N byte-identical prompts, so
/// samples 2..N are served from the prefix cache. On a backend that
/// reports `cached_tokens` they collapse to
/// [`ProbeOutcome::TooFewUncachedTokens`] and the multi-sample probe
/// silently degenerates to a single-sample one. On a backend that does
/// **not** report it (Ollama's OpenAI front door omits `usage`
/// entirely), they instead read as enormous throughputs — and
/// [`summarise`] takes the FASTEST sample, so it would pick the most
/// cache-contaminated one and derive a timeout several times too short.
/// That is a fail-open, manufactured by the very change meant to make
/// the measurement trustworthy.
///
/// **The index leads.** A prefix cache matches from position 0 forward,
/// so putting the varying part first makes consecutive samples diverge
/// as early as the prompt allows; a shared leading base with the index
/// appended would leave everything before it cacheable. The fixed
/// `build_messages` envelope still precedes both and is still cacheable
/// — that is handled by subtracting `cached_tokens`, not by this
/// ordering.
///
/// Pure, so the property is a unit test rather than a live observation.
pub fn sample_cache_buster(boot_cache_buster: &str, index: usize) -> String {
    format!("{index}-{boot_cache_buster}")
}

/// Should the probe take another sample?
///
/// Pure, so the loop's whole stopping rule is a unit test with no clock:
/// stop at [`PROBE_SAMPLES`], or when [`PROBE_TOTAL_BUDGET_MS`] of wall
/// clock has already gone, whichever comes first.
///
/// **One rule, not two.** An earlier revision added an explicit "stop as
/// soon as a sample saturates", which reintroduces exactly the defect
/// #624 is about: a single 20 s stall — a cold `llama-server` warming
/// its weights, say — would end the probe at one unrepresentative sample
/// and fire the ceiling finding, with the fast samples that would have
/// contradicted it never taken. The elapsed check already stops a
/// genuinely saturating first sample, because saturating means spending
/// the whole budget; a sample that merely came *close* buys one more
/// look, which is the behaviour worth having.
pub fn more_samples_wanted(taken: usize, elapsed_ms: u64) -> bool {
    taken < PROBE_SAMPLES && elapsed_ms < PROBE_TOTAL_BUDGET_MS
}

/// Throughput of one sample, if it measured one.
///
/// `None` for every non-measuring outcome — including
/// [`ProbeOutcome::Saturated`], which bounds throughput from above but
/// measures none (the same distinction
/// [`super::basis::TimeoutBasis::Saturated`] exists to keep).
///
/// `Measured` is only ever built with a non-zero `elapsed_ms` and at
/// least [`MIN_UNCACHED_PROBE_TOKENS`] tokens (see [`probe_sample`]), so
/// the division is finite and positive. Pure.
pub fn sample_tok_per_s(outcome: &ProbeOutcome) -> Option<f64> {
    match outcome {
        ProbeOutcome::Measured { uncached_tokens, elapsed_ms } if *elapsed_ms > 0 => {
            Some(f64::from(*uncached_tokens) / (*elapsed_ms as f64 / 1000.0))
        }
        _ => None,
    }
}

/// What a run of samples together says about this backend.
///
/// [`Self::best`] is the outcome [`super::derive_guard_timeout`] acts
/// on; the other two exist so a reader of the durable
/// `policy / guard_tier.boot` row can tell a reproducible number from a
/// noisy one, which is the whole of issue #624's complaint about
/// `timeout_basis: "probed"`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeSummary {
    /// The sample the timeout is derived from — see [`summarise`].
    pub best: ProbeOutcome,
    /// How many samples produced a usable throughput.
    ///
    /// Zero when none did, in which case [`Self::best`] is one of the
    /// non-measuring outcomes and no rate is reported anywhere.
    pub measured_samples: u32,
    /// The **lowest** throughput among the measuring samples.
    ///
    /// Beside `tok_per_s` (the highest) this is the contention spread,
    /// and it is the number that would have made #624 visible from a
    /// single boot row rather than from three of them: 6 994 against
    /// 269.6 says "this host was busy", where 6 994 alone says nothing.
    ///
    /// Equal to the highest when only one sample measured — honestly so:
    /// one sample observed one rate.
    pub slowest_tok_per_s: Option<f32>,
}

/// How informative an outcome is, for [`summarise`]'s ranking.
///
/// Higher wins. The two upper rungs are about the *timeout*; the three
/// lower ones all derive the same floor, so between them the ranking
/// decides one thing only — **whether a coverage finding fires**.
///
/// * `Measured` over `Saturated`: contention and a cold model can stall
///   one sample to the budget on a host that is otherwise fast, and a
///   real rate is strictly more informative than an upper bound.
/// * `Saturated` over the failures: it is the only non-measuring outcome
///   that says something about throughput, and it takes the ceiling.
/// * `Failed` over the two thin outcomes, which is the one genuinely
///   arguable rung. A failure means a call to the backend did not
///   complete — a fact about the *backend*, and real evidence for the
///   finding's prediction that every dispatch will fail the same way. A
///   thin sample means the call completed and only the *measurement* was
///   unusable. Ranking them the other way would let two thin samples
///   bury a real failure; ranking them this way lets one transient
///   failure fire a finding on a backend that answered twice. The
///   second is the better error: `run_probe` logs every failing sample
///   at `warn!` either way, so the loud path keeps the evidence, and
///   under-warning about a backend whose calls fail is worse than
///   over-warning about one whose calls are slow.
fn informativeness(outcome: &ProbeOutcome) -> u8 {
    match outcome {
        ProbeOutcome::Measured { .. } => 4,
        ProbeOutcome::Saturated { .. } => 3,
        ProbeOutcome::Failed { .. } => 2,
        ProbeOutcome::TooFewUncachedTokens { .. } => 1,
        ProbeOutcome::NoTokenCount => 0,
    }
}

/// Fold a run of samples into the one number the timeout is derived from.
///
/// **The fastest measuring sample wins, and the direction is the whole
/// point** (issue #624). Prompt processing has a hardware ceiling and no
/// floor: contention, a cold model and a busy daemon can only make an
/// observation *slower* than the host is capable of, never faster. So
/// the maximum over N samples is the best available estimate of the
/// host, and every sample below it is measuring something other than the
/// host.
///
/// **This moves the derived timeout DOWN, toward the fail-open edge, on
/// purpose.** A contended sample derives a *longer* budget, which is the
/// safe direction — so why correct it? Because
/// [`super::PROBE_SAFETY_FACTOR`]'s 2x is *already* the designed margin
/// for runtime contention (M1's open risk 3: the guard shares the GPU
/// with the planner). A probe that folds startup contention into the
/// measured rate spends that margin twice, and pays for it with a
/// `timeout_basis: "probed"` that is not reproducible across boots of
/// one unchanged host and a ceiling finding that cries wolf. Note that
/// the cache-buster still guards the genuinely dangerous direction: an
/// *over*-measured rate can only come from a cache hit, not from a quiet
/// moment.
///
/// With no measuring sample, the most informative failure wins — see
/// [`informativeness`] for the ranking and why `Failed` outranks a thin
/// sample.
///
/// An empty slice is [`ProbeOutcome::NoTokenCount`] with nothing
/// measured: unreachable through [`more_samples_wanted`], which always
/// grants a first sample, and given a total answer rather than a panic
/// because this is a security control and "unreachable" is a property of
/// another function.
///
/// Pure.
pub fn summarise(samples: &[ProbeOutcome]) -> ProbeSummary {
    let rates: Vec<f64> = samples.iter().filter_map(sample_tok_per_s).collect();
    let best = samples
        .iter()
        .max_by(|a, b| {
            informativeness(a).cmp(&informativeness(b)).then_with(|| {
                // Only reached when both rank alike, and only
                // `Measured` carries a rate — so this orders the
                // measuring samples and leaves every other tie to
                // `max_by`, which keeps the LAST of equal elements.
                sample_tok_per_s(a)
                    .unwrap_or(f64::NEG_INFINITY)
                    .total_cmp(&sample_tok_per_s(b).unwrap_or(f64::NEG_INFINITY))
            })
        })
        .cloned()
        .unwrap_or(ProbeOutcome::NoTokenCount);
    ProbeSummary {
        best,
        measured_samples: rates.len() as u32,
        slowest_tok_per_s: rates.into_iter().reduce(f64::min).map(|r| r as f32),
    }
}

#[cfg(test)]
mod tests;
