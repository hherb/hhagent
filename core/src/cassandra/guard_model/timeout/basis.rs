//! How a guard timeout was arrived at, and how to report it
//! (wiring-spec D9).
//!
//! Split out of [`super`] to keep that file under the 500-LOC cap, and
//! because these types are one coherent thing: the *provenance* of a
//! timeout, as distinct from the arithmetic that derives it. No IO
//! happens here and no timeout is derived here —
//! [`super::derive_guard_timeout`] owns that and constructs these.
//!
//! The one function that does live here is [`classify_pin`], which is
//! not a derivation: it reads an operator's already-decided number and
//! says which [`PinBand`] it falls in. It sits beside the type it
//! constructs rather than beside the arithmetic it is not part of.

use std::time::Duration;

// `TIMEOUT_FLOOR_MS`/`TIMEOUT_CEILING_MS` are used by `classify_pin`; the
// other two are referenced from doc links only.
#[allow(unused_imports)]
use super::{
    MIN_UNCACHED_PROBE_TOKENS, PROBE_BUDGET_MS, TIMEOUT_CEILING_MS, TIMEOUT_FLOOR_MS,
};

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

/// Why a probe produced no usable sample.
///
/// A closed set, so an enum — the same argument
/// [`super::super::tier::Unadjudicated`] makes one module over. As a
/// `&'static str` the "short, stable, whitespace-free" promise
/// [`TimeoutBasis::kind`] makes was a promise about a *caller-supplied*
/// string, an external caller could collide with `Operator`'s own token,
/// and no consumer could match on a reason exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnprobedReason {
    /// A `Measured` sample whose arithmetic came out non-finite or
    /// non-positive. Unreachable via [`super::probe_sample`]; guarded
    /// anyway.
    Nonsensical,
    /// Too little genuinely-processed work to divide by.
    TooFewUncachedTokens,
    /// The backend reported no `usage.prompt_tokens`.
    NoTokenCount,
    /// The probe call itself failed.
    Failed,
}

impl UnprobedReason {
    /// A short, stable, whitespace-free token for a log field.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Nonsensical => "probe-nonsensical",
            Self::TooFewUncachedTokens => "probe-too-few-uncached-tokens",
            Self::NoTokenCount => "probe-no-token-count",
            Self::Failed => "probe-failed",
        }
    }
}

/// Where an operator's pinned timeout sits relative to the band this
/// module would derive within (issue [#615]).
///
/// **Not a clamp, and must not become one.** The band constrains what
/// [`super::derive_guard_timeout`] may *infer*; an operator who pinned a
/// number has decided, and silently overriding them would make
/// `KASTELLAN_LLM_GUARD_TIMEOUT_MS` advisory. What was missing is that
/// the pin was applied in *silence*, so both ends of the band — each a
/// real, opposite exposure — arrived looking like a routine boot.
///
/// Three states, so an enum rather than two `bool`s: `below_floor` and
/// `above_ceiling` can both be `true` in a struct and cannot both be
/// true in reality, and a reader then has to work out which one wins.
/// Same argument [`UnprobedReason`] makes one type up.
///
/// [#615]: https://github.com/hherb/kastellan/issues/615
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinBand {
    /// Inside `[TIMEOUT_FLOOR_MS, TIMEOUT_CEILING_MS]`. Routine.
    InBand,
    /// Below [`TIMEOUT_FLOOR_MS`]. A *shorter* timeout is a *weaker*
    /// control: the tier is escalate-up only, so an adjudication that
    /// runs out of budget does not error — it fails OPEN to
    /// catalogue-only screening.
    BelowFloor,
    /// Above [`TIMEOUT_CEILING_MS`], the point past which stalling a
    /// dispatch is judged worse than degrading to catalogue-only
    /// screening. Reachable by *following this project's own advice*:
    /// issue #612's mitigation for a Metal host is a pin of roughly 3x
    /// the ceiling.
    AboveCeiling,
}

impl PinBand {
    /// The `timeout_basis` token for an operator pin in this band.
    ///
    /// Encoded into the token rather than left to a separate field,
    /// following [`UnprobedReason`]: `Unprobed` reports
    /// `"probe-failed"`, not a bare `"unprobed"` with the reason
    /// elsewhere. So `SELECT ... WHERE payload->>'timeout_basis' =
    /// 'operator-below-floor'` counts the exposed hosts directly.
    ///
    /// **An in-band pin keeps the historic `"operator"` token
    /// unchanged**, so this is additive for every healthy deployment.
    ///
    /// It is additive for *rows*, not for *questions*: before this,
    /// every operator pin emitted `"operator"`, so a pre-existing
    /// `WHERE payload->>'timeout_basis' = 'operator'` meaning "which
    /// hosts pin their guard timeout?" still runs, still returns rows,
    /// and now silently omits exactly the out-of-band ones — the hosts
    /// worth counting. Use `LIKE 'operator%'` to count all pins.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InBand => "operator",
            Self::BelowFloor => "operator-below-floor",
            Self::AboveCeiling => "operator-above-ceiling",
        }
    }
}

/// Where a pinned timeout sits relative to `[TIMEOUT_FLOOR_MS,
/// TIMEOUT_CEILING_MS]`.
///
/// Split from [`super::validate_operator_timeout`] so the classification is a
/// total function of one number and every boundary is a unit test — the
/// two comparisons are the whole of issue [#615], and an off-by-one on
/// either would make the reporting wrong in the direction that stays
/// quiet. Both bounds are **inclusive**: a pin exactly at the floor or
/// exactly at the ceiling is a value this module would itself derive,
/// so it is not a finding.
///
/// Pure.
///
/// [#615]: https://github.com/hherb/kastellan/issues/615
pub fn classify_pin(ms: u64) -> PinBand {
    if ms < TIMEOUT_FLOOR_MS {
        PinBand::BelowFloor
    } else if ms > TIMEOUT_CEILING_MS {
        PinBand::AboveCeiling
    } else {
        PinBand::InBand
    }
}

/// Where a guard timeout came from, in enough detail to log it.
#[derive(Debug, Clone, PartialEq)]
pub enum TimeoutBasis {
    /// `KASTELLAN_LLM_GUARD_TIMEOUT_MS`. No probe was run.
    ///
    /// Carries where the pin sits relative to the derivation band so
    /// that an out-of-band value reaches the `warn!` and the durable
    /// `policy / guard_tier.boot` row instead of only the routine
    /// `info!` line — see [`PinBand`] and issue #615. The value itself
    /// is still honoured verbatim.
    Operator { band: PinBand },
    /// Derived from a boot probe that produced at least one real
    /// sample.
    ///
    /// `tok_per_s` is the **fastest** of [`super::PROBE_SAMPLES`]
    /// samples, and `slowest_tok_per_s` the slowest of the ones that
    /// measured — together the contention spread (issue [#624]). Before
    /// the probe took more than one sample, a single figure here was not
    /// reproducible across boots of one unchanged host: the DGX derived
    /// 6 073 / 269.6 / 1 582 tok/s on three consecutive boots of the
    /// same backend, and a reader treating `probed` as a property of the
    /// host was reading noise. A row whose two rates are close is a
    /// measurement; one whose rates differ by 22x is a busy host, and
    /// now says so.
    ///
    /// (22x — 6 073 / 269.6 — because these are the two rates *this
    /// row* carries. The 26x quoted in [`super::summarise`] is a
    /// different ratio: the ~7 000 tok/s measured directly minutes later
    /// against the 269.6 boot, and the 7 000 never appears in any row.
    /// Conflating them was corrected here and in
    /// `boot_report::BootRates` together, so the two stay in step.)
    ///
    /// `measured_samples` is at least 1 whenever this variant exists,
    /// and `slowest_tok_per_s == tok_per_s` when it is exactly 1 — one
    /// sample observed one rate, which is honest rather than a
    /// fabricated spread. **Both hold of a summary built by
    /// [`super::summarise`]; neither is enforced by this type**, whose
    /// fields are `pub`, so read them as a description of the producer
    /// rather than as a guarantee of the row.
    ///
    /// `attempted_samples` is the denominator, without which
    /// `measured_samples: 1` reads three ways at once — see
    /// [`super::ProbeSummary::attempted_samples`]. A row with
    /// `attempted_samples > measured_samples` and no coverage finding is
    /// a backend that failed or stalled on some of its boot calls and
    /// measured on the rest; the reason is in that boot's `warn!` lines.
    ///
    /// [#624]: https://github.com/hherb/kastellan/issues/624
    Probed {
        tok_per_s: f32,
        slowest_tok_per_s: f32,
        measured_samples: u32,
        attempted_samples: u32,
        derived_ms: u64,
        clamped: Clamped,
    },
    /// The probe overran [`PROBE_BUDGET_MS`] without answering.
    ///
    /// **A basis of its own, carrying no throughput and no derivation.**
    /// It used to be reported as `Probed`, which forced two fabrications:
    /// a `tok_per_s` computed from [`MIN_UNCACHED_PROBE_TOKENS`] — a
    /// *sample-rejection floor*, not a count of anything this probe
    /// processed — and a `derived_ms` set to the post-clamp
    /// [`TIMEOUT_CEILING_MS`] while the `Probed` arm's `derived_ms` is
    /// the *pre*-clamp derivation. So one field meant two things, and
    /// `kind()` answered `"probed"` for a probe that produced no sample.
    ///
    /// The overrun is still a measurement of *slowness* — that is why it
    /// takes the ceiling — but the only throughput-shaped number it
    /// carries is the budget it exceeded.
    ///
    /// **`budget_ms` is ONE sample's budget, not the probe's.** It was
    /// the same thing until #624 made the probe multi-sample; now the
    /// probe as a whole may have spent up to `PROBE_TOTAL_BUDGET_MS +
    /// PROBE_BUDGET_MS` (60 s) across `attempted_samples` calls. The
    /// count is carried so the row says how much evidence the ceiling
    /// rests on: one call that stalled is weaker than three.
    ///
    /// **Since issue [#626], the current code cannot write
    /// `attempted_samples: 1` on this variant at all.** The total budget
    /// is twice one sample's, so a saturating first sample leaves a whole
    /// budget unspent and the probe takes another. Refusing that second
    /// call would need a saturating sample to overshoot its 20 s deadline
    /// by a further 20 s, and `tier::probe::run_probe` takes its `Instant`
    /// immediately before the loop, with one `format!` between them. So a
    /// row saying `1` is a **pre-#626 row, or a bug** — not, as this said
    /// until #637's review, a run that had spent the total before the
    /// second call could start.
    ///
    /// **What the count does NOT say is that every sample stalled.** This
    /// variant is what [`super::summarise`] returns whenever *no* sample
    /// measured and *at least one* saturated, because its
    /// `informativeness` ranking puts `Saturated` above every failure. So
    /// `[Saturated, Failed, Failed]` reaches this row as
    /// `attempted_samples: 3` off **one** stall — and that mixed shape is
    /// also the only way to reach the 60 s bound above, since two
    /// saturating samples spend the whole total and stop at 40 s. The
    /// count is how many calls the probe made, not how many stalled; the
    /// per-sample `warn!` in `tier::probe::run_one_sample` says which was
    /// which.
    ///
    /// [#626]: https://github.com/hherb/kastellan/issues/626
    Saturated { budget_ms: u64, attempted_samples: u32 },
    /// The probe could not produce a usable sample.
    ///
    /// `attempted_samples` matters most on this variant: the
    /// [`UnprobedReason::Failed`] finding predicts that *every* dispatch
    /// will fail the same way, and three failed calls are much stronger
    /// evidence for that than one.
    Unprobed { reason: UnprobedReason, attempted_samples: u32 },
}

impl TimeoutBasis {
    /// A short, stable, whitespace-free token for a log field.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Operator { band } => band.as_str(),
            Self::Probed { .. } => "probed",
            Self::Saturated { .. } => "probe-saturated",
            Self::Unprobed { reason, .. } => reason.as_str(),
        }
    }

    /// The operator-facing finding this basis reports, if any.
    ///
    /// `Some` marks a **reduction in coverage** and earns a `warn!`;
    /// `None` is routine and gets only the `info!` boot line, because
    /// warning about routine things is how the one that matters gets
    /// scrolled past.
    ///
    /// Five bases qualify, and they are not the same finding — which is
    /// why this returns the sentence rather than a `bool`:
    ///
    /// * [`Clamped::ToCeiling`] — large documents will time out here.
    /// * [`Self::Saturated`] — the probe itself never returned.
    /// * [`UnprobedReason::Failed`] — the probe call FAILED while
    ///   `/props` answered, which predicts that every adjudication will
    ///   fail the same way. That is the strongest predictor of a
    ///   totally fail-open tier in this enum, and it used to be reported
    ///   at `info!` alongside a "guard tier configured" line.
    /// * [`PinBand::BelowFloor`] — an operator pin shorter than the
    ///   shortest value this module would derive.
    /// * [`PinBand::AboveCeiling`] — an operator pin longer than the
    ///   longest.
    ///
    /// **The last two are findings about the CONFIGURATION, not about
    /// the host** (issue #615), and they are reported through the same
    /// channel deliberately: an operator reading a boot line wants one
    /// place that says "this deployment screens less than it looks like
    /// it does", regardless of whether a probe or a pin got it there.
    /// An *in-band* pin stays silent — it is the operator's own number,
    /// inside the range this module would have chosen anyway.
    pub fn coverage_finding(&self) -> Option<&'static str> {
        match self {
            Self::Probed { clamped: Clamped::ToCeiling, .. } => Some(
                "this host cannot adjudicate a worst-case document within the guard \
                 timeout budget: large, token-dense documents WILL time out and fail \
                 open to catalogue-only screening. Set KASTELLAN_LLM_GUARD_TIMEOUT_MS \
                 deliberately if a longer per-dispatch stall is acceptable.",
            ),
            Self::Saturated { .. } => Some(
                "the guard boot probe never returned within its budget. The timeout was \
                 set to the ceiling, but nothing about this backend's throughput was \
                 measured -- large documents will very likely time out and fail open.",
            ),
            Self::Operator { band: PinBand::BelowFloor } => Some(
                "KASTELLAN_LLM_GUARD_TIMEOUT_MS is pinned BELOW the shortest timeout \
                 this module will ever derive. The pin is honoured, but an adjudication \
                 that runs out of budget does not error -- it fails OPEN to \
                 catalogue-only screening, so this converts documents the tier could \
                 otherwise have judged into unscreened ones.",
            ),
            Self::Operator { band: PinBand::AboveCeiling } => Some(
                "KASTELLAN_LLM_GUARD_TIMEOUT_MS is pinned ABOVE the longest timeout this \
                 module will derive, past the point where stalling a dispatch is judged \
                 worse than degrading to catalogue-only screening. The pin is honoured: \
                 a single dispatch may now block for the whole pinned budget. That is \
                 the intended trade on a host whose throughput the boot probe cannot \
                 measure (issue #612) -- recorded here so it is a decision on the \
                 record rather than a silent one.",
            ),
            Self::Unprobed { reason: UnprobedReason::Failed, .. } => Some(
                "the guard boot probe FAILED while /props answered. The tier is \
                 configured and verified, but the call it will make on every dispatch \
                 is the call that just failed -- expect it to fail OPEN on all of them. \
                 The cause was logged by `guard boot probe failed` above.",
            ),
            // The quiet half, enumerated rather than caught by a
            // wildcard. `error_kind.rs` argues this exact point for
            // `GuardErrorKind::Other` -- "a wildcard would silently file
            // it under `other`" -- and #619's review pointed out that the
            // same PR left the wildcard standing here, in the one match
            // whose default is "nothing to report". A new `PinBand` arm or
            // a fourth `Clamped` state would have compiled straight into
            // `coverage_finding: null`: a host that screens less than it
            // looks like it does, reported as routine. Now it is a build
            // error and whoever adds the state has to decide.
            Self::Operator { band: PinBand::InBand } => None,
            Self::Probed { clamped: Clamped::No | Clamped::ToFloor, .. } => None,
            Self::Unprobed {
                reason:
                    UnprobedReason::Nonsensical
                    | UnprobedReason::TooFewUncachedTokens
                    | UnprobedReason::NoTokenCount,
                ..
            } => None,
        }
    }
}

/// A guard timeout together with how it was arrived at.
#[derive(Debug, Clone, PartialEq)]
pub struct GuardTimeout {
    pub timeout: Duration,
    pub basis: TimeoutBasis,
}

#[cfg(test)]
mod tests;
