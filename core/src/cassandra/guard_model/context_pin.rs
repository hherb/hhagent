//! The guard backend's context must hold a worst-case document
//! (issue [#604], wiring-spec D8).
//!
//! # Why this exists
//!
//! `SCAN_BYTE_CAP` bounds the **bytes** handed to the injection guard.
//! It bounds nothing about **tokens**, and tokens are what a model
//! server's context is measured in. Measurement 3 hit the gap on its
//! first over-cap attack case: a document truncated to exactly 65,536
//! bytes tokenised to **44,437 tokens** and the adjudication died with
//!
//! ```text
//! HTTP 400 {"type":"exceed_context_size_error","n_prompt_tokens":44437,"n_ctx":32768}
//! ```
//!
//! The ratio is **attacker-chosen**, because the attacker writes the
//! document. Measurement 1's material was ordinary prose at ~6.5
//! bytes/token; the failing case was a dense jailbreak collection —
//! leetspeak, symbol runs, base64-ish blobs — at **1.47**.
//!
//! # What this module decides, and what it leaves to the wiring
//!
//! In production the same 400 arrives at the dispatch chokepoint, where
//! the tier fails **open** (wiring-spec D4/D8: escalate-up-only means
//! every failure mode is at worst today's catalogue-only behaviour).
//! Fail-open is only defensible if the 400 is *rare*, so this module
//! makes it impossible on a correctly deployed host: the tier refuses to
//! **boot** unless the server's per-request context can hold a
//! worst-case document.
//!
//! Fail-**closed** at runtime was considered and rejected: an attacker
//! who can force the 400 can force it on any document by padding it,
//! which is a denial of service on the whole tool path reachable by
//! anyone who can serve the agent a web page.
//!
//! # Why one token per byte is a bound and not a guess
//!
//! Shieldstral's tokeniser is byte-level BPE — its base vocabulary
//! contains the individual bytes — so no input can produce *more* than
//! one token per byte. 1 token/byte is therefore the adversarial
//! ceiling rather than an estimate, and an attacker choosing maximally
//! unmergeable bytes converges on it. The two measurements bracket it
//! from below: 1.47 bytes/token on real jailbreak text (#604) and 1.26
//! on synthetic dense text (M2).
//!
//! # Where the number comes from
//!
//! `/props` is llama.cpp's own endpoint and already the guard's source
//! of truth for *which weights* the server opened
//! ([`super::weights_pin`]). It also reports the context size, and this
//! module reads it from the same place for the same reason: the server
//! is the only thing that knows.
//!
//! [#604]: https://github.com/hherb/kastellan/issues/604

use std::fmt;

use crate::cassandra::injection_guard::SCAN_BYTE_CAP;

/// Tokens reserved for the tuned policy prompt and the chat template,
/// on top of the document itself.
///
/// **A constant with a comment, not a measurement**, and deliberately
/// generous. The asymmetry is the whole reason to be generous: too
/// large and a marginally-sized server refuses to boot, which is loud
/// and one flag away from fixed; too small and the 400 becomes
/// reachable again at runtime, which fails open silently. See open risk
/// 8 in the wiring spec.
pub const GUARD_PROMPT_OVERHEAD_TOKENS: u64 = 512;

/// The smallest per-request context a guard backend may serve.
///
/// **Derived from [`SCAN_BYTE_CAP`], never written as a literal.** If
/// the cap ever moves, this must move with it — a literal here would
/// leave the check silently sized for the old cap, which is a
/// fail-open. `context_pin_required_tracks_the_scan_cap` pins the
/// derivation itself.
pub const REQUIRED_GUARD_N_CTX: u64 = SCAN_BYTE_CAP as u64 + GUARD_PROMPT_OVERHEAD_TOKENS;

/// Why the guard backend's context could not be accepted.
///
/// Kept as separate variants rather than one "context bad" string
/// because they call for different actions — the same reasoning
/// [`super::weights_pin::WeightsPinError`] documents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardContextError {
    /// `/props` parsed but carried no context size in either place we
    /// look.
    ///
    /// Refused rather than assumed. An assumed size fails open at
    /// runtime, which is precisely what this module exists to prevent.
    NoContextSize,
    /// The server's per-request context is smaller than a worst-case
    /// document needs.
    TooSmall { reported: u64, required: u64 },
}

impl GuardContextError {
    /// A short, stable, whitespace-free token naming which refusal this
    /// is — for a log field or a report header, where the multi-line
    /// [`fmt::Display`] paragraph does not fit.
    ///
    /// Same split, and the same rationale, as
    /// [`super::weights_pin::WeightsPinError::kind`].
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NoContextSize => "no-context-size",
            Self::TooSmall { .. } => "context-too-small",
        }
    }
}

impl fmt::Display for GuardContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoContextSize => write!(
                f,
                "the guard backend's /props reported no context size, so it cannot be \
                 shown to hold a worst-case document.\n\
                 Looked for `default_generation_settings.n_ctx` and a top-level `n_ctx`. \
                 A backend that reports neither cannot be verified; the guard tier \
                 refuses rather than assuming a size, because a wrong assumption fails \
                 OPEN at runtime (issue #604)."
            ),
            Self::TooSmall { reported, required } => write!(
                f,
                "the guard backend serves {reported} tokens of context per request, and \
                 a worst-case document needs {required}.\n  \
                 {SCAN_BYTE_CAP} bytes at the adversarial floor of 1 token/byte, plus \
                 {GUARD_PROMPT_OVERHEAD_TOKENS} tokens for the policy prompt.\n  \
                 Restart llama-server with `-c {required}` or higher. Until then a \
                 sufficiently dense document fails the adjudication with HTTP 400 and \
                 the tier fails OPEN on it -- which is the class of document most \
                 likely to be an attack. See issue #604."
            ),
        }
    }
}

impl std::error::Error for GuardContextError {}

/// Extract the per-request context size from a llama.cpp `/props` body.
///
/// Pure — no IO, no HTTP.
///
/// **`default_generation_settings.n_ctx` is read first**, because it is
/// the number a request is actually compared against. Measured on the
/// DGX guard server 2026-08-23: launched with `-c 131072` and no `-np`,
/// it reports `total_slots: 4` while *each slot* reports the full
/// `n_ctx: 131072`, and there is **no top-level `n_ctx`** — so on that
/// build `-c` is per-slot and the nested field is the per-request
/// limit.
///
/// A top-level `n_ctx` is accepted as a fallback for builds that report
/// it there. `None` for every shape that is not a positive integer, so
/// a server reporting the field as `null`, a string, an object, zero or
/// a negative number is treated as "did not tell us" — which
/// [`context_verdict`] turns into a refusal, never into a guess.
pub fn n_ctx_from_props(props: &serde_json::Value) -> Option<u64> {
    let positive = |v: &serde_json::Value| v.as_u64().filter(|n| *n > 0);
    props
        .get("default_generation_settings")
        .and_then(|d| d.get("n_ctx"))
        .and_then(positive)
        .or_else(|| props.get("n_ctx").and_then(positive))
}

/// Is `reported` a context big enough for a worst-case document?
///
/// `Ok(n)` is the single success and returns the accepted size, so a
/// caller can log what it verified rather than what it wanted.
///
/// **`required` is a parameter, not a read of [`REQUIRED_GUARD_N_CTX`],**
/// for exactly the reason [`super::weights_pin::hash_matches`] takes
/// one: with the constant hard-wired, the accepting arm would only be
/// reachable from a fixture representing a 66,048-token server, and an
/// implementation that refused unconditionally would pass every test
/// that could be written. See the `unreachable-success-path-proves-nothing`
/// note.
///
/// The comparison is `<`, so a server reporting **exactly** `required`
/// passes: `required` is already the worst case plus its overhead, and
/// demanding one more token would refuse a correctly-sized host.
///
/// Pure.
pub fn context_verdict(reported: Option<u64>, required: u64) -> Result<u64, GuardContextError> {
    match reported {
        None => Err(GuardContextError::NoContextSize),
        Some(n) if n < required => Err(GuardContextError::TooSmall { reported: n, required }),
        Some(n) => Ok(n),
    }
}

/// [`context_verdict`] against the in-repo requirement. The thin
/// wrapper production uses; the logic lives in the parameterised form
/// above.
pub fn verify_guard_context(reported: Option<u64>) -> Result<u64, GuardContextError> {
    context_verdict(reported, REQUIRED_GUARD_N_CTX)
}

#[cfg(test)]
mod tests;
