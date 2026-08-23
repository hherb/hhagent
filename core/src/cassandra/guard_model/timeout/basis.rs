//! How a guard timeout was arrived at, and how to report it
//! (wiring-spec D9).
//!
//! Split out of [`super`] to keep that file under the 500-LOC cap, and
//! because these four types are one coherent thing: the *provenance* of
//! a timeout, as distinct from the arithmetic that derives it. Nothing
//! here does IO or arithmetic — [`super::derive_guard_timeout`] owns
//! that and constructs these.

use std::time::Duration;

// Referenced from doc links in this module; not used in code here.
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
/// [`super::tier::Unadjudicated`] makes one module over. As a
/// `&'static str` the "short, stable, whitespace-free" promise
/// [`TimeoutBasis::kind`] makes was a promise about a *caller-supplied*
/// string, an external caller could collide with `Operator`'s own token,
/// and no consumer could match on a reason exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnprobedReason {
    /// A `Measured` sample whose arithmetic came out non-finite or
    /// non-positive. Unreachable via [`probe_sample`]; guarded anyway.
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

/// Where a guard timeout came from, in enough detail to log it.
#[derive(Debug, Clone, PartialEq)]
pub enum TimeoutBasis {
    /// `KASTELLAN_LLM_GUARD_TIMEOUT_MS`. No probe was run.
    Operator,
    /// Derived from a boot probe that produced a real sample.
    Probed { tok_per_s: f32, derived_ms: u64, clamped: Clamped },
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
    /// takes the ceiling — but the only honest number it carries is the
    /// budget it exceeded.
    Saturated { budget_ms: u64 },
    /// The probe could not produce a usable sample.
    Unprobed { reason: UnprobedReason },
}

impl TimeoutBasis {
    /// A short, stable, whitespace-free token for a log field.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Probed { .. } => "probed",
            Self::Saturated { .. } => "probe-saturated",
            Self::Unprobed { reason } => reason.as_str(),
        }
    }

    /// The operator-facing finding this basis reports, if any.
    ///
    /// `Some` marks a **reduction in coverage** and earns a `warn!`;
    /// `None` is routine and gets only the `info!` boot line, because
    /// warning about routine things is how the one that matters gets
    /// scrolled past.
    ///
    /// Three bases qualify, and they are not the same finding — which is
    /// why this returns the sentence rather than a `bool`:
    ///
    /// * [`Clamped::ToCeiling`] — large documents will time out here.
    /// * [`Self::Saturated`] — the probe itself never returned.
    /// * [`UnprobedReason::Failed`] — the probe call FAILED while
    ///   `/props` answered, which predicts that every adjudication will
    ///   fail the same way. That is the strongest predictor of a
    ///   totally fail-open tier in this enum, and it used to be reported
    ///   at `info!` alongside a "guard tier configured" line.
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
            Self::Unprobed { reason: UnprobedReason::Failed } => Some(
                "the guard boot probe FAILED while /props answered. The tier is \
                 configured and verified, but the call it will make on every dispatch \
                 is the call that just failed -- expect it to fail OPEN on all of them. \
                 The cause was logged by `guard boot probe failed` above.",
            ),
            _ => None,
        }
    }
}

/// A guard timeout together with how it was arrived at.
#[derive(Debug, Clone, PartialEq)]
pub struct GuardTimeout {
    pub timeout: Duration,
    pub basis: TimeoutBasis,
}
