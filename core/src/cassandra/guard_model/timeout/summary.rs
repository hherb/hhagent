//! Folding a run of samples into one number (issue [#624]).
//!
//! Split out of [`super::sample`] when #625's review pushed that file
//! past the 500-LOC cap, along the seam it already carried as a section
//! divider — and for the same reason beyond the line count that
//! `timeout.rs` was split three ways: [`super::sample`] is **what ONE
//! measurement of this backend is**, and this is **how several of them
//! become the one the timeout is derived from**.
//!
//! Everything here is pure — no clock, no socket, no server. The IO half
//! (`tier::probe::run_probe`) contributes only a loop and an `Instant`;
//! which sample wins and when to stop are decided here and are unit
//! tests.
//!
//! Re-exported from [`super`], so `timeout::summarise` and every other
//! historic path still resolves; this split moved code, not names.
//!
//! [#624]: https://github.com/hherb/kastellan/issues/624

// `MIN_UNCACHED_PROBE_TOKENS`, `probe_document` and `probe_sample` are
// referenced from doc links only.
#[allow(unused_imports)]
use super::sample::{
    probe_document, probe_sample, ProbeOutcome, MIN_UNCACHED_PROBE_TOKENS, PROBE_BUDGET_MS,
};

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
/// **Two of [`PROBE_BUDGET_MS`], and the factor is the whole of issue
/// [#626].** It was *equal* to one sample's budget until then, which
/// meant a [`ProbeOutcome::Saturated`] first sample — produced only when
/// the per-request budget expired, so leaving `elapsed_ms` at exactly the
/// total — ended the probe at one measurement. See
/// [`more_samples_wanted`] for what that cost.
///
/// **A healthy boot still pays nothing extra**, which is what the old
/// equality was protecting and what the factor does not spend: the loop
/// stops at [`PROBE_SAMPLES`], never on this clock, unless a sample is
/// pathologically slow. A DGX sample is ~160 ms and a Mac one under a
/// second, so 3 x 160 ms against 40 000 is ~83x of headroom on the DGX
/// and 3 x ~560 ms (the Mac's ~1 445 tok/s on an 810-token body) is
/// ~24x.
///
/// **What the factor actually buys, per host:**
///
/// | host | before | after |
/// | --- | --- | --- |
/// | healthy | ~0.5 s | ~0.5 s — the clock is never reached |
/// | cold model, then fast | 20 s, ceiling, **false finding** | ~20.4 s, a real rate, no finding |
/// | genuinely slow | 20 s, `attempted_samples: 1` | 40 s, `attempted_samples: 2` |
/// | pathological | 40 s | 60 s |
///
/// So the added wall clock is not paid by the host this fixes — that one
/// pays ~0.4 s and gets a measurement instead of a warning. It is paid by
/// a host whose samples land just under the budget, and such a host
/// derives the ceiling and earns a coverage finding either way.
///
/// **The bound it gives is `PROBE_TOTAL_BUDGET_MS + PROBE_BUDGET_MS`
/// (60 s), and that is deliberate rather than sloppy.**
/// [`more_samples_wanted`] is consulted *before* a sample, so a sample
/// that starts just under the total may still run its own full budget.
/// Making the guarantee tight would mean shortening the per-sample budget
/// — which redefines [`ProbeOutcome::Saturated`] and would saturate hosts
/// the current budget measures fine — or refusing to start any sample
/// that could overrun, which is the equality this issue removed.
///
/// **The FULL overrun is reachable only when samples return just under
/// 20 s** — a host already deriving the ceiling and already emitting a
/// coverage finding. A *smaller* overrun is ordinary and carries no such
/// reassurance: two 100 ms samples followed by a saturating third spend
/// 20.2 s on a host whose `best` is a fast [`ProbeOutcome::Measured`],
/// with no clamp and no finding. Said exactly, because #625's review
/// found this paragraph offering the ceiling-finding reassurance for
/// every overrun rather than for the one it holds of.
///
/// The strict `>` relation to [`PROBE_BUDGET_MS`] is a **compile**-time
/// assertion in this module's tests, not a convention: re-equalising the
/// two is the defect, not a tunable.
///
/// [#626]: https://github.com/hherb/kastellan/issues/626
pub const PROBE_TOTAL_BUDGET_MS: u64 = 2 * PROBE_BUDGET_MS;

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
/// **One rule, not two, and since issue [#626] that is a property of the
/// budgets rather than a coincidence of them.** An earlier revision added
/// an explicit "stop as soon as a sample saturates".
/// [`ProbeOutcome::Saturated`] is produced only when the per-request
/// budget expired, so a saturating sample leaves `elapsed_ms` at exactly
/// [`PROBE_BUDGET_MS`] — and while [`PROBE_TOTAL_BUDGET_MS`] *equalled*
/// that, the check below already returned `false`, making the extra
/// clause unable to fire. The two rules were behaviourally identical, and
/// the shipped one had the rejected one's defect.
///
/// **That defect was the cold-model case of #624, and the budget change
/// is what removes it.** A cold `llama-server` paging in its weights
/// stalls its first call past 20 s; the probe stopped there, derived the
/// ceiling and fired [`super::basis::TimeoutBasis::Saturated`]'s finding
/// — "the guard boot probe never returned within its budget" — on a host
/// that adjudicates a document in ~19 s a moment later, with the fast
/// samples that would have contradicted it never taken. #624 removed the
/// *contention* case of that defect; this removes the *cold-model* one.
/// With the total at twice the per-sample budget, one saturating sample
/// leaves a whole budget unspent and the run gets a second look.
///
/// **Nothing here special-cases saturation, and it must not start to.**
/// The rule is still elapsed wall clock, which is why a sample that came
/// merely *close* to the budget buys another look on the same terms. What
/// decides the outcome afterwards is [`summarise`]'s ranking, where
/// `Measured` outranks `Saturated`: a fast follow-up sample replaces the
/// ceiling with a derived budget, and a host that really is slow
/// saturates twice and fires the same finding on `attempted_samples: 2`.
///
/// (Until #625's review the first paragraph said the explicit rule was
/// *rejected* because it "would end the probe at one unrepresentative
/// sample and fire the ceiling finding" — which is what the shipped rule
/// then did too. Corrected in place rather than quietly, because that
/// claim was the design record in three other documents; #626 is the
/// behaviour catching up with the correction.)
///
/// [#626]: https://github.com/hherb/kastellan/issues/626
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
    /// How many samples were **taken**, usable or not.
    ///
    /// The denominator [`Self::measured_samples`] is a numerator of.
    /// Without it a durable row saying `measured_samples: 1` has three
    /// readings that call for opposite actions — the probe took one
    /// sample and it worked; it took three and two were served from
    /// cache; it took three and two calls to the backend *failed* — and
    /// the reader cannot even recover [`PROBE_SAMPLES`], which is a
    /// tunable this file argues about. `attempted > measured` is the
    /// query that says "look at the boot log", and #625's review found
    /// it unwritable.
    ///
    /// It does not say *which* kind of unusable; the per-sample `warn!`
    /// in `tier::probe::run_one_sample` does, for every non-measuring
    /// outcome.
    pub attempted_samples: u32,
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
/// decides **whether a coverage finding fires** — and, secondarily,
/// which `timeout_basis` token the durable row carries
/// (`probe-failed` vs `probe-too-few-uncached-tokens` vs
/// `probe-no-token-count`). This said "one thing only" until #625's
/// review pointed out that
/// [`super::basis::PinBand::as_str`] spends a paragraph arguing that
/// token is the queryable thing.
///
/// * `Measured` over `Saturated`: contention and a cold model can stall
///   one sample to the budget on a host that is otherwise fast, and a
///   real rate is strictly more informative than an upper bound. **This
///   rung has a partial backstop, and its shape is worth knowing rather
///   than discovering:** [`summarise`]'s tie-break orders by
///   [`sample_tok_per_s`], which is `None` for every non-measuring
///   outcome, so a `Measured` sample beats a `Saturated` one even if
///   their ranks here were *equal*. Collapsing this line to
///   `Saturated`'s rank is therefore invisible to `summarise` —
///   **lowering it below `Saturated` is not**: `Saturated` then wins,
///   derives the ceiling and fires a false coverage finding. The
///   tie-break covers one direction only, which is why the rank is also
///   asked directly by
///   `the_informativeness_ranking_is_strictly_ordered`. (This said "held
///   twice … both mechanisms are one line each" until #625's review
///   counted the directions.)
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
/// `informativeness` (private, just below) for the ranking and why
/// `Failed` outranks a thin sample.
///
/// An empty slice is [`ProbeOutcome::NoTokenCount`] with nothing
/// measured, given a total answer rather than a panic because this is a
/// security control and "unreachable" is a property of another function.
/// Note *which* function: the first sample is guaranteed by
/// `tier::probe::run_probe` taking its `Instant` immediately before the
/// loop, **not** by [`more_samples_wanted`], which refuses a first
/// sample like any other once `elapsed_ms >= PROBE_TOTAL_BUDGET_MS`.
/// Hoisting that `Instant` earlier would make this arm reachable.
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
        attempted_samples: samples.len() as u32,
        measured_samples: rates.len() as u32,
        slowest_tok_per_s: rates.into_iter().reduce(f64::min).map(|r| r as f32),
    }
}

#[cfg(test)]
mod tests;
