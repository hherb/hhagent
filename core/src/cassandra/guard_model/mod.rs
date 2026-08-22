//! Model-based adjudication tier for the injection guard.
//!
//! Escalate-up only: this tier may turn an `Allow` into a `Block` and
//! never the reverse, so a guard-model failure can only ever be as
//! permissive as today's catalogue-only behaviour.
//!
//! **This module reports; it never decides to allow.** Fail-open on a
//! router error is the documented posture (the sandbox and the egress
//! allowlist are the boundary, not this), but it is applied at the
//! wiring site so the whole security posture is legible in one place.
//!
//! Not wired into the chokepoint yet — see
//! `docs/superpowers/specs/2026-08-21-shieldstral-guard-slice-1-design.md`.

pub mod decide;
pub mod policy;
pub mod weights_pin;

pub use decide::{decide, GuardAdjudication, DEFAULT_TAU};

use kastellan_llm_router::logprob_score::{
    binary_token_probability, first_position_alternatives, NO_FORMS, YES_FORMS,
};
use kastellan_llm_router::{ChatRequest, Router, RouterConfig, RouterError};

/// How many alternatives to request at position 0. Verbatim from the
/// measured harness; 20 is what both hosts were measured with.
const TOP_LOGPROBS: u8 = 20;

/// A client bound to the guard endpoint.
///
/// Holds its own [`Router`] because `Router::dispatch_local` reads
/// `config.local_url` — so "reach the guard" is expressed as a router
/// whose config came from [`RouterConfig::for_guard`].
pub struct GuardClient {
    router: Router,
}

impl GuardClient {
    /// Build a guard client.
    ///
    /// `Ok(None)` means the operator configured no guard — expected, not
    /// an error. `Err(..)` means they configured one and it is not
    /// usable: either only one of the two keys is set (a
    /// misconfiguration, see [`RouterConfig::for_guard`]) or the HTTP
    /// client could not be built. Collapsing those into one `None` would
    /// make an unconfigured guard indistinguishable from a broken one,
    /// which is how a security tier ends up silently off.
    pub fn from_config(cfg: &RouterConfig) -> Result<Option<Self>, RouterError> {
        match cfg.for_guard()? {
            None => Ok(None),
            Some(guard_cfg) => Router::new(guard_cfg).map(|router| Some(Self { router })),
        }
    }

    /// llama.cpp's `/props` for the guard backend.
    ///
    /// Delegates to [`Router::props`] so there is one HTTP path to the
    /// guard, not two. Its consumer is [`weights_pin`], which reads
    /// `model_path` to learn which file the server opened — the only
    /// thing the endpoint can tell us, since llama.cpp reports an empty
    /// `digest` (issue #592).
    pub async fn props(&self) -> Result<serde_json::Value, RouterError> {
        self.router.props().await
    }

    /// The raw probability that the document is unsafe, before any
    /// threshold is applied.
    ///
    /// `Ok(None)` means unmeasurable — the response carried no usable
    /// verdict pair. Used by the calibration harness, which must *fit* a
    /// threshold and therefore cannot be handed one.
    pub async fn probability(&self, document: &str) -> Result<Option<f32>, RouterError> {
        let mut req = ChatRequest::new(
            self.router.config().local_model.clone(),
            policy::build_messages(document),
        )
        .with_logprobs(TOP_LOGPROBS);
        // Verbatim from the measured harness: one token is all that is
        // read (the position-0 alternatives), and temperature 0 keeps
        // the logit pair reproducible.
        req.max_tokens = Some(1);
        req.temperature = Some(0.0);

        let resp = self.router.send(&req).await?;
        Ok(first_position_alternatives(&resp)
            .and_then(|alts| binary_token_probability(alts, YES_FORMS, NO_FORMS)))
    }

    /// Screen one document against `tau`.
    ///
    /// Returns [`GuardAdjudication::Unmeasured`] — never an error and
    /// never `Clear` — when the response carries no usable verdict
    /// pair. An `Err` means the call itself failed.
    ///
    /// Delegates to [`GuardClient::probability`] so there is exactly one
    /// request-building path; two would drift.
    pub async fn adjudicate(
        &self,
        document: &str,
        tau: f32,
    ) -> Result<GuardAdjudication, RouterError> {
        Ok(decide(self.probability(document).await?, tau))
    }
}
