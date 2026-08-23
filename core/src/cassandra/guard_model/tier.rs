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

use std::sync::Arc;
use std::time::Duration;

use kastellan_llm_router::{RouterConfig, RouterError};

use super::context_pin::{self, GuardContextError};
use super::timeout::{self, GuardTimeout, ProbeOutcome};
use super::{decide, GuardAdjudication, GuardClient};
use crate::cassandra::injection_guard::{decision_for_score, InjectionDecision};

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

/// Why the guard tier could not be built.
///
/// **Every variant stops the daemon** (wiring-spec D6). The
/// counter-argument — a down daemon protects nothing — was weighed and
/// rejected: "loud error at boot" is precisely the thing that gets
/// scrolled past, and the failure being guarded against is *silent
/// deactivation of a security control*. The concrete hazard has
/// happened: `kastellan-cli install` regenerates `kastellan.env` and
/// has been observed dropping hand-added keys.
#[derive(Debug)]
pub enum GuardTierError {
    /// The URL/model pair is half-configured, τ is missing while a
    /// guard is configured, or the HTTP client could not be built.
    Config(RouterError),
    /// τ was supplied and is not a usable threshold.
    Tau(TauError),
    /// `/props` could not be reached or parsed, so nothing about the
    /// backend could be verified.
    PropsUnavailable(RouterError),
    /// The backend's context cannot hold a worst-case document.
    Context(GuardContextError),
}

impl std::fmt::Display for GuardTierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "guard tier configuration: {e}"),
            Self::Tau(e) => write!(f, "{e}"),
            Self::PropsUnavailable(e) => write!(
                f,
                "the guard tier is configured but its backend's /props is unreachable: \
                 {e}\nThe tier cannot be verified, and starting without it would turn a \
                 security control off behind a correct-looking log line. Start the guard \
                 backend, or unset KASTELLAN_LLM_GUARD_URL and KASTELLAN_LLM_GUARD_MODEL \
                 to run without the tier deliberately."
            ),
            Self::Context(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for GuardTierError {}

/// What one adjudication produced, in the shape the audit row wants.
///
/// `p` is `None` on both fail-open doors — a failed call has no score,
/// and an `Unmeasured` one had no usable verdict pair. `tau` rides
/// along so the row is self-describing: a score without the threshold
/// it was compared against cannot be re-read months later.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuardReport {
    pub outcome: GuardOutcome,
    /// The raw probability, recorded on **cleared** documents as well
    /// as blocked ones (D5). It is a float and carries no document
    /// content.
    pub p: Option<f32>,
    pub tau: f32,
    pub ms: u64,
}

impl GuardReport {
    /// The `guard` sub-object for the per-dispatch tool audit row.
    ///
    /// Lives here rather than at the emission site so the field names
    /// are fixed in one place — a forensic query written against them
    /// should not depend on which chokepoint emitted the row.
    pub fn audit_value(&self) -> serde_json::Value {
        serde_json::json!({
            "state": self.outcome.as_str(),
            "p":     self.p,
            "tau":   self.tau,
            "ms":    self.ms,
        })
    }
}

/// A configured, verified guard tier.
///
/// Built once at boot and shared; holds no mutable state.
///
/// The manual [`std::fmt::Debug`] prints the tuning, not the client: a
/// `Router` holds a `reqwest::Client` whose debug output is noise, and the
/// three numbers below are the ones anyone debugging this actually wants.
pub struct GuardTier {
    client: GuardClient,
    tau: f32,
    /// The per-request budget and how it was arrived at. Kept whole
    /// rather than reduced to a `Duration` so the boot line can report
    /// the basis, and so a `Clamped::ToCeiling` finding survives to be
    /// logged.
    timeout: GuardTimeout,
    /// The context size the backend reported and D8 accepted. Logged so
    /// a boot line states what was verified rather than what was
    /// required.
    n_ctx: u64,
}

impl std::fmt::Debug for GuardTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardTier")
            .field("tau", &self.tau)
            .field("timeout", &self.timeout)
            .field("n_ctx", &self.n_ctx)
            .finish_non_exhaustive()
    }
}

impl GuardTier {
    /// Build the tier from the router config, verifying the backend.
    ///
    /// The boot sequence, in order, and the order is load-bearing:
    ///
    /// 1. **Tri-state on the URL/model pair.** Neither key set is a
    ///    deliberate opt-out and yields `Ok(None)`.
    /// 2. **τ is required.** A guard configured without one is a
    ///    misconfiguration, not an opt-out — the same argument
    ///    `for_guard` makes about the URL/model pair. There is
    ///    deliberately no default (D1).
    /// 3. **`/props`, then the D8 context check — fatal.** Cheap,
    ///    deterministic, and it is what makes the runtime fail-open on
    ///    HTTP 400 defensible.
    /// 4. **The D9 throughput probe — never fatal.** It picks a number;
    ///    it does not verify a control, so it must not undo a boot that
    ///    step 3 already allowed. Skipped entirely when the operator
    ///    pinned `KASTELLAN_LLM_GUARD_TIMEOUT_MS`.
    ///
    /// Two clients are built: the probe spends
    /// [`timeout::PROBE_BUDGET_MS`], which is also what makes
    /// [`ProbeOutcome::Saturated`] observable — a probe that overruns
    /// its budget arrives as a transport timeout, and that *is* the
    /// measurement.
    pub async fn from_router_config(
        cfg: &RouterConfig,
        nonce: &str,
    ) -> Result<Option<Self>, GuardTierError> {
        // Step 1 + 2. `for_guard` owns the URL/model tri-state; the
        // probe budget is a placeholder here and never reaches
        // production, since the client below is rebuilt at the real
        // timeout.
        let probe_budget = Duration::from_millis(timeout::PROBE_BUDGET_MS);
        let probe_client = match GuardClient::from_config(cfg, probe_budget)
            .map_err(GuardTierError::Config)?
        {
            None => return Ok(None),
            Some(c) => c,
        };
        let tau = match cfg.guard_tau {
            None => {
                return Err(GuardTierError::Config(RouterError::Config(
                    "KASTELLAN_LLM_GUARD_URL and KASTELLAN_LLM_GUARD_MODEL are set but \
                     KASTELLAN_LLM_GUARD_TAU is not. The guard tier has no default \
                     threshold on purpose: a provisional value promoted to a default is \
                     an unfitted number wearing the appearance of a sanctioned one. \
                     Measurement 3's fitted value is 0.79552656."
                        .to_string(),
                )))
            }
            Some(t) => validate_tau(t).map_err(GuardTierError::Tau)?,
        };

        // Step 3 — fatal.
        let props = probe_client.props().await.map_err(GuardTierError::PropsUnavailable)?;
        let n_ctx = context_pin::verify_guard_context(context_pin::n_ctx_from_props(&props))
            .map_err(GuardTierError::Context)?;

        // Step 4 — never fatal.
        let outcome = match cfg.guard_timeout_ms {
            Some(_) => ProbeOutcome::NoTokenCount, // ignored by the override arm
            None => run_probe(&probe_client, nonce).await,
        };
        let timeout = timeout::guard_timeout_from(cfg.guard_timeout_ms, &outcome);

        let client = GuardClient::from_config(cfg, timeout.timeout)
            .map_err(GuardTierError::Config)?
            .expect("for_guard already returned Some for this config");
        Ok(Some(Self { client, tau, timeout, n_ctx }))
    }

    /// The validated threshold.
    pub fn tau(&self) -> f32 {
        self.tau
    }

    /// The per-request budget and how it was arrived at.
    pub fn timeout(&self) -> &GuardTimeout {
        &self.timeout
    }

    /// The backend context size D8 verified.
    pub fn n_ctx(&self) -> u64 {
        self.n_ctx
    }

    /// Adjudicate one document.
    ///
    /// Returns the outcome, the raw probability when there was one, and
    /// the wall clock — all three of which the audit row carries (D5).
    ///
    /// **`p` is returned on cleared documents too**, and that is the
    /// decision with the most leverage in the slice: recording it makes
    /// production itself the source of a real-world score distribution,
    /// which is the only route out of measurement 3's catalogue-selected
    /// corpus. It is a float and carries no document content.
    ///
    /// Calls `probability()` + `decide()` rather than `adjudicate()`
    /// because the latter discards `p`. `probability` remains the only
    /// request-building path, which is the property that actually
    /// matters.
    pub async fn adjudicate_document(&self, body: &str) -> GuardReport {
        let started = std::time::Instant::now();
        let probability = self.client.probability(body).await;
        let ms = started.elapsed().as_millis() as u64;
        let (outcome, p) = match probability {
            Ok(p) => (resolve(GuardReading::Adjudicated(decide(p, self.tau))), p),
            Err(e) => {
                // Logged here and never carried into the verdict, so no
                // backend message can influence a containment decision.
                tracing::warn!(
                    target: "kastellan::guard_model",
                    error = %e,
                    ms,
                    "guard adjudication failed; failing OPEN to catalogue-only screening"
                );
                (resolve(GuardReading::Failed), None)
            }
        };
        GuardReport { outcome, p, tau: self.tau, ms }
    }
}

/// Run the boot probe and classify what came back.
///
/// The IO half of D9. A transport failure here is
/// [`ProbeOutcome::Failed`] — including the timeout that
/// [`timeout::PROBE_BUDGET_MS`] imposes, which is reported as
/// [`ProbeOutcome::Saturated`] because an overrun budget is an *upper
/// bound on throughput* rather than a missing measurement.
async fn run_probe(client: &GuardClient, nonce: &str) -> ProbeOutcome {
    let document = timeout::probe_document(nonce);
    match client.timed_probe(&document).await {
        Ok(reading) => timeout::probe_sample(reading),
        Err(e) if is_timeout(&e) => ProbeOutcome::Saturated { budget_ms: timeout::PROBE_BUDGET_MS },
        Err(e) => ProbeOutcome::Failed { why: e.to_string() },
    }
}

/// Did this router error come from the request budget running out?
///
/// `reqwest` surfaces a total-timeout as a `Transport` error whose
/// `Display` names it. Matching on the text is coarse, and the
/// consequence of getting it wrong is bounded: a misread timeout
/// becomes [`ProbeOutcome::Failed`] and takes the floor instead of the
/// ceiling, which is a *shorter* guard timeout on a slow host — worth
/// avoiding, not worth a boot failure over.
fn is_timeout(e: &RouterError) -> bool {
    let s = e.to_string().to_ascii_lowercase();
    s.contains("timed out") || s.contains("timeout")
}

/// Share one tier across the dispatcher and whatever else needs it.
pub type SharedGuardTier = Arc<GuardTier>;

#[cfg(test)]
mod tests;
