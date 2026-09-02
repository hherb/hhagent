//! Where a test daemon's planner router dials (issue [#634]).
//!
//! Lifted out of [`super`] unchanged in [#641]'s branch, purely to keep
//! that file under the 500-line cap — `spec.rs` had reached 538. This is
//! a **movement**, not a rewrite: every item below is character-for-
//! character what it was, and `super`'s `pub use` keeps
//! `daemon::spec::LlmEndpoint` and `daemon::spec::COMPAT_SEGMENT`
//! resolving, so no caller and no test changed.
//!
//! The type earns a module of its own because it is the one parameter
//! in [`DaemonSpec::new`](super::DaemonSpec::new) whose wrong value
//! fails *silently* — see [`LlmEndpoint`]'s own docs.
//!
//! [#634]: https://github.com/hherb/kastellan/issues/634
//! [#641]: https://github.com/hherb/kastellan/issues/641

/// The OpenAI-compat path segment appended to an [`LlmEndpoint::Base`].
///
/// `pub` so that [`LlmEndpoint::Base`]'s own docs may link to it: a
/// public item linking to a private one is a `rustdoc` warning and a
/// dead link in the rendered page.
pub const COMPAT_SEGMENT: &str = "/v1";

/// Where the daemon's planner router should dial.
///
/// Two variants rather than one string **because the tree genuinely
/// holds both shapes, and they are not interchangeable**. The mock-LLM
/// callers own a bare `http://127.0.0.1:<port>` and want the on-wire
/// OpenAI-compat shape appended; the operator-driven callers
/// (`observation_capture`, `mail_daemon_e2e`'s live leg) read a URL out
/// of the environment that usually already ends in `/v1`. Appending to
/// the latter yields `/v1/v1` and a router that dials nothing;
/// *not* appending to the former yields a base with no compat segment
/// and a router that 404s. Both failures report a status code and
/// never the URL.
///
/// A single `&str` parameter cannot tell those apart, so the choice
/// would live in each caller's head. Here it is a type, and a call site
/// says which it means — or, when the value is the operator's and its
/// shape genuinely is not knowable, defers to
/// [`Self::from_operator_url`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmEndpoint {
    /// An OpenAI-compat **base**; [`COMPAT_SEGMENT`] is appended.
    Base(String),
    /// A **complete** URL that already carries its compat segment.
    /// Used verbatim.
    Verbatim(String),
}

impl LlmEndpoint {
    /// Classify an **operator-supplied** URL that may carry its compat
    /// segment or may not.
    ///
    /// The two variants above each demand that the caller already know
    /// which shape it holds. A caller reading a URL out of the
    /// *operator's* environment does not: `KASTELLAN_MAIL_LIVE_LLM_URL`
    /// has accepted both `http://127.0.0.1:11434` and
    /// `http://127.0.0.1:11434/v1` since that test was written, and the
    /// bare form is the one this tree's own installer treats as
    /// canonical (`OLLAMA_LLM_URL` in `core/src/install/plan.rs`).
    ///
    /// So this is a **third constructor, not a third variant** — it
    /// answers the question once, here, where a unit test can reach it,
    /// rather than in each caller's head. Both shapes normalise to
    /// exactly one [`COMPAT_SEGMENT`].
    ///
    /// Do **not** reach for this when the shape is known: a mock's
    /// `base_url` is a [`Self::Base`] and saying so is clearer than
    /// asking a function to work it out.
    ///
    /// Trailing slashes are trimmed before classifying *and* kept off
    /// the result, so `…:11434/` and `…:11434/v1/` reach the daemon as
    /// `…:11434/v1`. `ends_with` tests the whole `/v1` segment rather
    /// than a bare `v1`, so a base merely ending in those two characters
    /// (`…/apiv1`) is correctly read as a base — the same distinction
    /// `llm-router`'s `props_url` documents having needed.
    pub fn from_operator_url(url: impl Into<String>) -> Self {
        let url = url.into();
        let trimmed = url.trim_end_matches('/');
        if trimmed.ends_with(COMPAT_SEGMENT) {
            Self::Verbatim(trimmed.to_string())
        } else {
            Self::Base(trimmed.to_string())
        }
    }

    /// The value that reaches `KASTELLAN_LLM_LOCAL_URL`.
    ///
    /// `pub(super)` rather than private: the #641 split moved this type
    /// into its own module, and `DaemonSpec::service_spec` — its only
    /// caller — now lives one level up. Deliberately not `pub`: nothing
    /// outside the spec builder has any business rendering the URL.
    ///
    /// Asserts rather than silently appending when a [`Self::Base`]
    /// already carries its compat segment. That combination is never
    /// anything but a mistake, and it is the *first* half of the failure
    /// this type exists to prevent — `…/v1/v1`, a router that dials
    /// nothing, and an error naming a status code but never the URL
    /// (`RouterError::HttpStatus` carries the status and the body; the
    /// URL appears only in a `debug!` the daemon does not emit at
    /// `info`).
    ///
    /// The symmetric check on [`Self::Verbatim`] is deliberately absent:
    /// an OpenAI-compat server need not serve under `/v1`, so "does not
    /// end in `/v1`" is not evidence of a mistake there. Only the `Base`
    /// direction has a wrong answer that is knowable from the string.
    pub(super) fn url(&self) -> String {
        match self {
            Self::Base(base) => {
                assert!(
                    !base.trim_end_matches('/').ends_with(COMPAT_SEGMENT),
                    "LlmEndpoint::Base must not already carry {COMPAT_SEGMENT} \
                     (appending a second one dials nothing and reports no URL); \
                     use LlmEndpoint::Verbatim for a complete URL, or \
                     LlmEndpoint::from_operator_url when the shape is unknown. \
                     Got: {base}",
                );
                format!("{base}{COMPAT_SEGMENT}")
            }
            Self::Verbatim(url) => url.clone(),
        }
    }
}
