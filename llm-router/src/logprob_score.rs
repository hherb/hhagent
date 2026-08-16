//! Renormalising a two-way token distribution into a calibrated score.
//!
//! ## What this is for
//!
//! Some classifier models are not read from the text they emit but from
//! the *distribution* at their first output position. The caller asks a
//! yes/no policy question, requests one token with
//! [`ChatRequest::with_logprobs`](crate::messages::ChatRequest::with_logprobs),
//! and renormalises the `yes` and `no` log-probabilities against each
//! other. The result is a continuous score in `0.0..=1.0` rather than a
//! bare verdict — which is what makes a *banded* decision possible at all
//! (escalate in the middle, act only at the extremes).
//!
//! This module is the pure arithmetic half of that. It knows nothing
//! about any particular policy, model, or hook point: it takes the
//! alternatives at one position and two sets of token spellings, and
//! returns a probability or nothing.
//!
//! ## `None` means UNMEASURED, and that distinction is the whole point
//!
//! [`binary_token_probability`] returns `Option<f32>`, and `None` is not
//! "probably safe" — it is "this call carried no distribution to read".
//! The two must never collapse, because in a security control they fail
//! in opposite directions.
//!
//! The collapse is easy to write by accident. Seed both logits with a
//! sentinel floor, renormalise, and a response containing *neither*
//! spelling yields `exp(f)/(exp(f)+exp(f))` = exactly `0.5` — which then
//! compares as "below threshold" against the conventional τ=0.5 and reads
//! as a clean pass. A response containing only *one* spelling is worse:
//! the floor manufactures a confident 0.9999 out of a single observation.
//! A sentinel floor is the right arithmetic for renormalising two real
//! logits; it is not a licence to emit a verdict when there is nothing to
//! renormalise. So both spellings must be observed, or there is no score.
//!
//! This is not hypothetical. The Python probe this module's semantics are
//! taken from shipped with exactly that defect, and it was found only
//! because a mock backend was pointed at it: "could not measure" and
//! "measured safe" were one output.
//!
//! ## Token identity is a tokenizer problem, not a string problem
//!
//! The same word arrives spelled differently from different tokenizer
//! families — `yes`, `Ġyes` (byte-BPE), `▁yes` (SentencePiece) — and no
//! amount of `trim()` removes those markers. A matcher that compares the
//! display string alone therefore does not merely miss occasionally: it
//! can fail to find *either* spelling on every call at once, turning a
//! whole run unmeasurable the moment the backend changes. The wire
//! carries `bytes` for precisely this reason, so [`token_text`] prefers
//! it and falls back to the display form only when it is absent.

use crate::messages::{ChatResponse, TopLogProb};

/// Conventional spellings of the affirmative verdict token, after
/// [`normalize_token`]. Callers may pass their own.
pub const YES_FORMS: &[&str] = &["yes", "true", "unsafe"];

/// Conventional spellings of the negative verdict token, after
/// [`normalize_token`].
pub const NO_FORMS: &[&str] = &["no", "false", "safe"];

/// The token's text, preferring the wire's raw `bytes` over the display
/// form.
///
/// `bytes` is the tokenizer-neutral identity: a byte-BPE `Ġyes` and a
/// SentencePiece `▁yes` both carry the bytes of `" yes"`, which
/// [`normalize_token`] then trims to `yes`. The display form is the
/// fallback for backends that omit `bytes`, and it is why
/// [`normalize_token`] strips the markers explicitly as well.
///
/// Invalid UTF-8 in `bytes` falls back to the display form rather than
/// being lossily decoded: a token that is half a multi-byte character is
/// a real thing at a truncation boundary, and inventing a replacement
/// character for it could only ever create a spurious match.
pub fn token_text(alt: &TopLogProb) -> &str {
    match alt.bytes.as_deref() {
        Some(raw) => std::str::from_utf8(raw).unwrap_or(&alt.token),
        None => &alt.token,
    }
}

/// Fold a raw token spelling to its comparable form.
///
/// Strips, in order: the byte-BPE (`Ġ`) and SentencePiece (`▁`) word-start
/// markers, surrounding whitespace, surrounding quotes, and trailing
/// sentence punctuation; then lowercases. So `" Yes."`, `Ġyes`, `▁YES` and
/// `"yes"` all fold to `yes`.
///
/// Deliberately *not* aggressive beyond that: folding more (stripping
/// interior punctuation, say) would start mapping unrelated tokens onto a
/// verdict spelling, and a false match here is a fabricated score.
pub fn normalize_token(raw: &str) -> String {
    raw.trim_start_matches(['\u{0120}', '\u{2581}'])
        .trim()
        .trim_matches(['"', '\'', '`'])
        .trim_end_matches(['.', ',', ':', ';', '!', '?'])
        .trim()
        .to_lowercase()
}

/// Renormalise the `yes` and `no` alternatives at one output position
/// into `P(yes)`.
///
/// Returns `None` — meaning **unmeasured**, never "no" — when either
/// spelling is absent from `alternatives`, including when the slice is
/// empty. See the module docs for why that distinction is load-bearing.
///
/// When a spelling appears more than once (tokenizers can offer several
/// encodings of one word) the highest log-probability wins, matching how
/// the model's own mass is distributed across them.
///
/// The arithmetic is `sigmoid(z_yes − z_no)`, which is algebraically
/// identical to `exp(z_yes) / (exp(z_yes) + exp(z_no))` but cannot
/// overflow: log-probabilities of −700 and below make the naive form
/// evaluate `0/0` and yield `NaN`, and a `NaN` score compares `false`
/// against every threshold — silently unreachable in whichever direction
/// the caller happens to test.
pub fn binary_token_probability(
    alternatives: &[TopLogProb],
    yes_forms: &[&str],
    no_forms: &[&str],
) -> Option<f32> {
    let mut z_yes: Option<f64> = None;
    let mut z_no: Option<f64> = None;

    for alt in alternatives {
        let folded = normalize_token(token_text(alt));
        let slot = if yes_forms.contains(&folded.as_str()) {
            &mut z_yes
        } else if no_forms.contains(&folded.as_str()) {
            &mut z_no
        } else {
            continue;
        };
        // Highest logprob wins when one spelling appears more than once.
        *slot = Some(slot.map_or(alt.logprob, |seen: f64| seen.max(alt.logprob)));
    }

    // Both spellings, or no score at all. This `?` pair is the whole
    // fail-safe: there is deliberately no sentinel-floor branch to fall
    // through to, because that branch is what turns "unmeasurable" into
    // "safe" (see the module docs).
    let (yes, no) = (z_yes?, z_no?);
    Some((1.0 / (1.0 + (no - yes).exp())) as f32)
}

/// The alternatives the backend offered at the **first** output position
/// of the first choice, if it returned any.
///
/// `None` covers every shape that carries no distribution: no `logprobs`
/// block, an empty `content` array, or a position with no `top_logprobs`
/// (which a backend returns when `logprobs: true` arrived without a
/// `top_logprobs` count). Callers get one unmeasurable case to handle
/// rather than four.
pub fn first_position_alternatives(resp: &ChatResponse) -> Option<&[TopLogProb]> {
    let alternatives = resp
        .choices
        .first()?
        .logprobs
        .as_ref()?
        .content
        .first()?
        .top_logprobs
        .as_slice();
    (!alternatives.is_empty()).then_some(alternatives)
}

#[cfg(test)]
mod tests;
