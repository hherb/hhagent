//! The production guard tier: the arm logic, the threshold validation,
//! and the boot sequence that assembles them (wiring-spec D1/D4/D6/D8/D9).
//!
//! # The shape, and what each door costs
//!
//! ```text
//! catalogue >= BLOCK_THRESHOLD  ->  Block, model NOT consulted
//! catalogue <  BLOCK_THRESHOLD  ->  guard configured?
//!                                     no  -> Allow, audited (NotConfigured)
//!                                     yes -> probability()
//!                                              Err(..)     -> Allow, audited (RouterError)
//!                                              Unmeasured  -> Allow, audited (Unmeasured)
//!                                              Flagged     -> Block
//!                                              Clear       -> Allow
//! ```
//!
//! **Escalate-up only.** The model can turn an `Allow` into a `Block`
//! and never the reverse, so every failure mode of this tier is at
//! worst today's catalogue-only behaviour. That is the whole safety
//! argument, and it is why the `Err(..)` door fails *open* even though
//! an attacker can reach it (wiring-spec D8: fail-closed would let
//! anyone who can serve the agent a web page deny it every document by
//! padding one).
//!
//! # What this tier is, and is not
//!
//! Measurement 3 fitted τ = 0.79552656 on 133 cases, 109 of them
//! captured through the real `web.fetch` path. At that threshold the
//! tier catches **36 of 55 attacks — 65% recall** at zero false
//! positives, and the misses concentrate exactly where its rationale is
//! strongest: bare imperative payloads are caught 6/6 at a median
//! 0.9955, while the same intent wrapped in a plausible document runs a
//! median 0.0797 with 5 of 8 missed.
//!
//! **So this is advisory defence-in-depth, not a gate**, and nothing
//! downstream may relax on it: no catalogue weight is lowered because
//! the model is watching, no allowlist widened, no sandbox constraint
//! loosened. See D10.

use super::GuardAdjudication;
use crate::cassandra::injection_guard::{decision_for_score, InjectionDecision};

pub mod boot;

pub use boot::{GuardReport, GuardTier, GuardTierError, SharedGuardTier};

/// Why a document was allowed through **without** the model having
/// judged it.
///
/// A named enum rather than a `bool` plus a log line, for three
/// reasons. It keeps [`super::decide`]'s ban on an `escalates() -> bool`
/// helper intact — a caller consuming a bool structurally cannot audit
/// a distinction it has already erased. It makes the three fail-open
/// doors **countable** in the audit log. And it makes the mapping
/// exhaustively testable without a server, which is where the security
/// decisions actually get pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unadjudicated {
    /// No guard endpoint is configured. The expected state on a host
    /// that has not opted in.
    ///
    /// **Its only producer is the `policy / guard_tier.boot` audit row**, not
    /// a dispatch: an unconfigured tier is reported once at boot rather than
    /// as a constant field on every row a host ever writes (slice-1 D1 — a
    /// per-call line on the chokepoint hot path is its own denial of
    /// service). The variant exists so both places spell that state the same
    /// way; deleting it would split one fact across two vocabularies.
    NotConfigured,
    /// The call succeeded but carried no usable verdict pair, or a
    /// non-finite score. **Not a pass** — see [`super::decide`].
    Unmeasured,
    /// The call itself failed: transport error, timeout, or an HTTP
    /// status. Includes the attacker-reachable HTTP 400 of issue #604
    /// and the timeout of issue #586.
    RouterError,
}

impl Unadjudicated {
    /// A short, stable token for the `guard.state` audit field.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::Unmeasured => "unmeasured",
            Self::RouterError => "router_error",
        }
    }
}

/// What the tier concluded about one document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardOutcome {
    /// Withhold the document from the planner.
    Block,
    /// Pass it through; the model judged it clear.
    Allow,
    /// Pass it through; the model did **not** judge it. Carries which
    /// door was taken so the fail-open is countable rather than
    /// invisible.
    AllowUnadjudicated { reason: Unadjudicated },
}

impl GuardOutcome {
    /// The `guard.state` audit field.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Block => "flagged",
            Self::Allow => "clear",
            Self::AllowUnadjudicated { reason } => reason.as_str(),
        }
    }

    /// Does this outcome withhold the document?
    pub fn blocks(&self) -> bool {
        matches!(self, Self::Block)
    }
}

/// What the tier managed to learn about one document.
///
/// The input to [`resolve`]. An explicit enum rather than a nested
/// `Option<Result<..>>` so every door has a name and the compiler
/// forces each to be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardReading {
    /// No tier is configured on this host.
    ///
    /// Never constructed by the chokepoint — `screen_result` returns no
    /// guard report at all in that case. It is kept because [`resolve`] is a
    /// **total** function over a public enum, and a caller that does hold
    /// this reading (a future call site that wants the door audited
    /// per-dispatch) must get the escalate-up-only answer rather than have
    /// to invent one.
    NotConfigured,
    /// The call failed. The error text is *not* carried: it is logged
    /// at the call site and never enters the verdict, so no backend
    /// message can influence the decision.
    Failed,
    /// The call succeeded and produced this adjudication.
    Adjudicated(GuardAdjudication),
}

/// Should the model be consulted at all for this catalogue score?
///
/// **Delegates to [`decision_for_score`]** rather than re-testing
/// `>= BLOCK_THRESHOLD`, so the tier and the catalogue cannot disagree
/// about where the threshold is. A second inline copy is a second thing
/// to keep in step.
///
/// `false` means the catalogue already Blocked, and the model is not
/// asked — which saves ~3.5 s per document and, more importantly, means
/// a catalogue Block cannot be undone by a model that says "clear".
///
/// Pure.
pub fn consults_model(catalogue_score: f32) -> bool {
    matches!(decision_for_score(catalogue_score), InjectionDecision::Allow)
}

/// Map a reading to an outcome.
///
/// Total, pure, and the only place the arm logic lives.
/// [`GuardAdjudication::Unmeasured`] maps to
/// [`Unadjudicated::Unmeasured`] and **never** to [`GuardOutcome::Allow`]:
/// both pass the document through, but only one of them claims the
/// model cleared it, and conflating them makes a silently dead tier
/// indistinguishable from a working one.
pub fn resolve(reading: GuardReading) -> GuardOutcome {
    match reading {
        GuardReading::NotConfigured => {
            GuardOutcome::AllowUnadjudicated { reason: Unadjudicated::NotConfigured }
        }
        GuardReading::Failed => {
            GuardOutcome::AllowUnadjudicated { reason: Unadjudicated::RouterError }
        }
        GuardReading::Adjudicated(GuardAdjudication::Flagged) => GuardOutcome::Block,
        GuardReading::Adjudicated(GuardAdjudication::Clear) => GuardOutcome::Allow,
        GuardReading::Adjudicated(GuardAdjudication::Unmeasured) => {
            GuardOutcome::AllowUnadjudicated { reason: Unadjudicated::Unmeasured }
        }
    }
}

/// Why an operator-supplied τ is unusable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TauError {
    /// `tau <= 0.0`. `p >= 0.0` holds for every probability, so the
    /// tier would Block **every** document the catalogue allowed —
    /// a denial of service on the whole tool path, arriving as "the
    /// agent stopped being able to read anything".
    NotPositive(f32),
    /// `tau > 1.0`. No probability can reach it, so the tier never
    /// flags: it looks configured, logs as configured, costs seconds
    /// per document, and is off.
    AboveOne(f32),
    /// `NaN` or an infinity. Every `NaN` comparison is false, so a
    /// `NaN` τ is the `> 1.0` failure wearing a different hat — the
    /// same reason [`super::decide`] routes a non-finite `p` to
    /// `Unmeasured`.
    NotFinite(f32),
}

impl std::fmt::Display for TauError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPositive(t) => write!(
                f,
                "KASTELLAN_LLM_GUARD_TAU is {t}, which is not positive. Every probability \
                 satisfies `p >= {t}`, so the tier would withhold EVERY document the \
                 catalogue allowed -- a denial of service on the whole tool path. \
                 Measurement 3's fitted value is 0.79552656."
            ),
            Self::AboveOne(t) => write!(
                f,
                "KASTELLAN_LLM_GUARD_TAU is {t}, which is above 1.0. No probability can \
                 reach it, so the tier would never flag: configured, logged as \
                 configured, paying seconds per document, and off. \
                 Measurement 3's fitted value is 0.79552656."
            ),
            Self::NotFinite(t) => write!(
                f,
                "KASTELLAN_LLM_GUARD_TAU is {t}, which is not a finite number. Every \
                 comparison against it is false, so the tier would never flag. \
                 Measurement 3's fitted value is 0.79552656."
            ),
        }
    }
}

impl std::error::Error for TauError {}

/// Is `tau` a usable threshold?
///
/// Accepts `(0.0, 1.0]`. **Both ends are refused because both are
/// silent failures** — one blocks everything, the other blocks nothing,
/// and neither reports itself as broken at runtime. The upper bound is
/// inclusive: `tau == 1.0` is a legitimate (if extreme) setting, since
/// `p == 1.0` can occur and would still flag.
///
/// Non-finite is checked **first**, because `NaN <= 0.0` and
/// `NaN > 1.0` are both false and a NaN would otherwise fall through
/// the range arms into acceptance.
///
/// Pure.
pub fn validate_tau(tau: f32) -> Result<f32, TauError> {
    if !tau.is_finite() {
        Err(TauError::NotFinite(tau))
    } else if tau <= 0.0 {
        Err(TauError::NotPositive(tau))
    } else if tau > 1.0 {
        Err(TauError::AboveOne(tau))
    } else {
        Ok(tau)
    }
}


#[cfg(test)]
mod tests;
