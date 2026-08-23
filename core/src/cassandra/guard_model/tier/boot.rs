//! Building the guard tier: the boot sequence, and what one adjudication
//! produces (wiring-spec D6/D8/D9).
//!
//! Split from [`super`] so the decisions stay separable from the IO that
//! feeds them. Everything in the parent is pure and testable without a
//! server; everything here needs one, and the split is what lets the arm
//! logic be pinned exhaustively at the unit layer.

use std::sync::Arc;
use std::time::Duration;

use kastellan_llm_router::{RouterConfig, RouterError};

use super::super::context_pin::{self, GuardContextError};
use super::super::timeout::{self, GuardTimeout, ProbeOutcome};
use super::super::{decide, GuardClient};
use super::{resolve, validate_tau, GuardOutcome, GuardReading, TauError};

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
