//! OpenAI-compatible chat-completion request and response types.
//!
//! These are the wire shapes for `POST <base>/chat/completions` against
//! any OpenAI-compatible HTTP endpoint:
//!
//! * vLLM and SGLang on Linux (the canonical local-backend choices).
//! * llama.cpp's `--api` server and Ollama on macOS (Ollama's
//!   `/v1/chat/completions` endpoint follows the same shape).
//! * Any frontier backend with an OpenAI-compatible front door (which
//!   today includes every commercial provider that matters).
//!
//! We deliberately model **only** the subset of fields the router
//! actually reads or writes for Phase 0. Streaming SSE, tool-call
//! arguments, function definitions, response-format JSON schemas, and
//! image/audio modalities all live behind the same endpoint but slot
//! in later. Today's contract: a list of role-tagged text messages
//! goes out, a single completion text comes back.
//!
//! ## Why we use `serde(rename_all = "lowercase")` for [`ChatRole`]
//! OpenAI's spec serialises roles as the bare lowercase strings
//! `"user"`, `"system"`, `"assistant"`, `"tool"`. A future addition
//! (e.g. `"developer"`) will require an explicit enum variant — we'd
//! rather break the build at compile time than silently round-trip
//! an unknown role as a stringly-typed escape hatch. The `Tool`
//! variant is included now even though Phase 0 does not invoke
//! function calling: keeping the enum closed-but-complete makes the
//! eventual tool-call slice a pure-Rust addition rather than a wire-
//! shape change.

use serde::{Deserialize, Serialize};

/// Role of the speaker in a chat-completion message.
///
/// Closed enum on purpose — see module docstring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

/// A single role-tagged text message in a chat conversation.
///
/// We do not attempt to model multimodal `content` (the OpenAI spec
/// permits a list of `{type, text|image_url, ...}` parts). For
/// Phase 0 the router carries plain text only; widening this later
/// is a backwards-compatible enum swap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: ChatRole::System, content: content.into() }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: ChatRole::User, content: content.into() }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: ChatRole::Assistant, content: content.into() }
    }
}

/// Outgoing chat-completion request.
///
/// `max_tokens` and `temperature` are `Option` so callers can defer
/// to backend defaults; serde's `skip_serializing_if = Option::is_none`
/// keeps the wire payload minimal — some local backends choke on
/// nulls in optional fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    /// Extra keyword arguments forwarded to the backend's chat
    /// *template* (not to sampling). This is the de-facto OpenAI-compat
    /// extension both Ollama and vLLM honour; it is the only portable
    /// way to reach a reasoning model's `enable_thinking` switch.
    ///
    /// Left `None` the field is not serialised at all, so a backend
    /// that has never heard of it sees a byte-identical payload. Set it
    /// with [`ChatRequest::without_thinking`] rather than by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<serde_json::Value>,

    /// Ask the backend to return per-token log-probabilities.
    ///
    /// Needed by classifier models that are read from the *distribution*
    /// at the first output position rather than from their emitted text —
    /// a `yes`/`no` safety classifier renormalised into a calibrated score
    /// is the motivating case. Without it such a model degrades to a bare
    /// verdict token, i.e. a hard threshold with no confidence band.
    ///
    /// Set both this and [`ChatRequest::top_logprobs`] together via
    /// [`ChatRequest::with_logprobs`]: vLLM rejects a `top_logprobs`
    /// arriving without `logprobs: true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,

    /// How many alternatives to return at each position (OpenAI caps this
    /// at 20). Only meaningful with `logprobs: true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u8>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            max_tokens: None,
            temperature: None,
            chat_template_kwargs: None,
            logprobs: None,
            top_logprobs: None,
        }
    }

    /// Ask for `top_n` token alternatives at each output position.
    ///
    /// Sets `logprobs: true` as well, because the two are one decision on
    /// the wire — vLLM 4xxs on a `top_logprobs` that arrives without it,
    /// and that failure reaches core as a `RouterError::Transport`, which
    /// reads as "the backend is unreachable" rather than "your request was
    /// malformed" (the misdiagnosis that cost the whole #505 session).
    ///
    /// Measured safe to combine with [`ChatRequest::without_thinking`]:
    /// against Shieldstral on llama.cpp the `chat_template_kwargs` key is
    /// accepted and inert — both calls returned 200 with an identical
    /// 26-token prompt, the second reporting `cached_tokens: 25`, which is
    /// positive evidence the rendered prompt was byte-identical rather
    /// than merely "no error".
    pub fn with_logprobs(mut self, top_n: u8) -> Self {
        self.logprobs = Some(true);
        self.top_logprobs = Some(top_n);
        self
    }

    /// Ask the backend's chat template to skip the model's thinking
    /// block (`chat_template_kwargs: {"enable_thinking": false}`).
    ///
    /// A reasoning model that thinks freely can spend the whole
    /// generation budget — and far more wall-clock than any sane
    /// request timeout — on `reasoning` while emitting an empty
    /// `content`. Measured on the DGX with the 26B local planner and a
    /// ~16k-token prompt: 222 s and 15 094 chars of reasoning for
    /// 1 519 chars of plan, versus 51 s with this set. Both failure
    /// modes downstream (transport timeout, and a `content` so empty
    /// that plan decoding reports `expected value at line 1 column 1`)
    /// trace back to it.
    ///
    /// A backend that does not implement the switch ignores the key,
    /// which is why this is safe to set unconditionally on the local
    /// leg.
    pub fn without_thinking(mut self) -> Self {
        self.chat_template_kwargs =
            Some(serde_json::json!({ "enable_thinking": false }));
        self
    }
}

/// One alternative token the backend considered at a given position,
/// with its log-probability.
///
/// `bytes` is the token's raw UTF-8, which OpenAI-compatible backends
/// return alongside the display form. It is the *reliable* identity of a
/// token: tokenizers render the same word with family-specific markers
/// (`Ġyes` for byte-BPE, `▁yes` for SentencePiece) that no amount of
/// trimming removes, whereas the bytes decode to plain ` yes` on every
/// one of them. See [`crate::logprob_score`], which prefers it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopLogProb {
    pub token: String,
    pub logprob: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<Vec<u8>>,
}

/// The token actually chosen at one position, plus the alternatives that
/// were in contention there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenLogProbs {
    pub token: String,
    pub logprob: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<Vec<u8>>,
    /// Empty when the backend returned `logprobs: true` but no
    /// `top_logprobs` count — a shape that carries no distribution to
    /// renormalise, and which callers must treat as unmeasurable.
    #[serde(default)]
    pub top_logprobs: Vec<TopLogProb>,
}

/// Per-position log-probabilities for one choice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogProbs {
    #[serde(default)]
    pub content: Vec<TokenLogProbs>,
}

/// One completion choice returned by the backend.
///
/// We model `index` and `finish_reason` because they're load-bearing
/// for downstream callers (Phase 1's scheduler will branch on
/// `finish_reason == "length"` to retry with a higher `max_tokens`),
/// but we do *not* require them to be present — vLLM omits
/// `finish_reason` when streaming is disabled and the response is
/// truncated mid-token.
///
/// Note this is deliberately **not** `Eq`: `logprobs` carries floats, and
/// a derived `Eq` on a struct holding log-probabilities would invite
/// exact-equality comparisons on values that are the output of a softmax.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatChoice {
    #[serde(default)]
    pub index: u32,
    pub message: ChatMessage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// Present only when the request asked for it. Every call the planner
    /// makes leaves this `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<LogProbs>,
}

/// Token-accounting envelope returned by the backend.
///
/// Phase 0 forwards this through unchanged; Phase 1+ will read it for
/// budgeting decisions in the scheduler's context-manager. All three
/// fields are `Option` because Ollama and some llama.cpp builds omit
/// the `usage` block entirely when the request was a non-streaming
/// completion.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
}

/// Decoded `200 OK` response from a chat-completion call.
///
/// The OpenAI envelope also carries `id`, `object`, `created`, and
/// `model`; we keep the first three as opaque strings (or absent) and
/// echo `model` because operators want to see which model actually
/// served the call (some backends do model-fallback transparently).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub choices: Vec<ChatChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_role_serializes_as_lowercase() {
        // Wire-shape pin: any change here rotates the contract with
        // every OpenAI-compatible backend on the planet.
        assert_eq!(serde_json::to_string(&ChatRole::System).unwrap(), "\"system\"");
        assert_eq!(serde_json::to_string(&ChatRole::User).unwrap(), "\"user\"");
        assert_eq!(serde_json::to_string(&ChatRole::Assistant).unwrap(), "\"assistant\"");
        assert_eq!(serde_json::to_string(&ChatRole::Tool).unwrap(), "\"tool\"");
    }

    #[test]
    fn chat_role_rejects_unknown_string() {
        // Closed enum: deserialising "developer" must fail rather than
        // silently fall back. If we ever add Developer as a role this
        // test will fail at the right moment.
        let err = serde_json::from_str::<ChatRole>("\"developer\"").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown variant"), "expected 'unknown variant' in {msg:?}");
    }

    #[test]
    fn chat_message_constructors_set_the_right_role() {
        assert_eq!(ChatMessage::system("hi").role, ChatRole::System);
        assert_eq!(ChatMessage::user("hi").role, ChatRole::User);
        assert_eq!(ChatMessage::assistant("hi").role, ChatRole::Assistant);
    }

    #[test]
    fn chat_request_omits_none_fields_on_the_wire() {
        // Some local backends (older llama.cpp builds especially) reject
        // requests that include explicit nulls. The
        // `skip_serializing_if = Option::is_none` pin guards against a
        // refactor that drops it.
        let req = ChatRequest::new("local-model", vec![ChatMessage::user("hi")]);
        let s = serde_json::to_string(&req).unwrap();
        assert!(!s.contains("max_tokens"), "max_tokens leaked: {s}");
        assert!(!s.contains("temperature"), "temperature leaked: {s}");
        assert!(s.contains("\"model\":\"local-model\""), "model missing in {s}");
    }

    #[test]
    fn chat_request_includes_optional_fields_when_set() {
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![ChatMessage::user("hi")],
            max_tokens: Some(42),
            temperature: Some(0.7),
            chat_template_kwargs: None,
            logprobs: None,
            top_logprobs: None,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"max_tokens\":42"), "max_tokens missing: {s}");
        assert!(s.contains("\"temperature\":0.7"), "temperature missing: {s}");
    }

    /// An untouched request must stay byte-identical on the wire — a
    /// backend that has never heard of `chat_template_kwargs` must not
    /// start seeing it just because the field exists in the struct.
    #[test]
    fn chat_template_kwargs_is_absent_unless_asked_for() {
        let req = ChatRequest::new("m", vec![ChatMessage::user("hi")]);
        let s = serde_json::to_string(&req).unwrap();
        assert!(
            !s.contains("chat_template_kwargs"),
            "chat_template_kwargs leaked into an untouched request: {s}"
        );
    }

    /// The exact wire shape both Ollama and vLLM look for. Pinned
    /// because a typo here fails silently: the backend ignores the
    /// unknown key and the model thinks anyway.
    #[test]
    fn without_thinking_emits_the_enable_thinking_false_kwarg() {
        let req =
            ChatRequest::new("m", vec![ChatMessage::user("hi")]).without_thinking();
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(
            v["chat_template_kwargs"]["enable_thinking"],
            serde_json::Value::Bool(false),
            "unexpected wire shape: {v}"
        );
    }

    /// `without_thinking` must not disturb anything else the caller set.
    #[test]
    fn without_thinking_preserves_the_other_fields() {
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![ChatMessage::user("hi")],
            max_tokens: Some(8192),
            temperature: Some(0.2),
            chat_template_kwargs: None,
            logprobs: None,
            top_logprobs: None,
        }
        .without_thinking();
        assert_eq!(req.model, "m");
        assert_eq!(req.max_tokens, Some(8192));
        assert_eq!(req.temperature, Some(0.2));
        assert_eq!(req.messages.len(), 1);
    }

    #[test]
    fn chat_response_decodes_canonical_openai_envelope() {
        // Hand-crafted to match what a vLLM 0.5+ server returns; the
        // `system_fingerprint` field is absent on purpose to prove
        // `serde(default)` fields tolerate missing keys.
        let raw = json!({
            "id": "chatcmpl-abc",
            "object": "chat.completion",
            "created": 1_700_000_000_u64,
            "model": "Qwen/Qwen2.5-7B-Instruct",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hello back"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14}
        });
        let resp: ChatResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.id.as_deref(), Some("chatcmpl-abc"));
        assert_eq!(resp.model.as_deref(), Some("Qwen/Qwen2.5-7B-Instruct"));
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].message.role, ChatRole::Assistant);
        assert_eq!(resp.choices[0].message.content, "hello back");
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, Some(11));
        assert_eq!(usage.total_tokens, Some(14));
    }

    /// The planner path must stay byte-identical to what it sends today:
    /// neither logprobs field may appear on a request nobody asked to
    /// score. Same guarantee `chat_template_kwargs` carries, and the same
    /// reason — a backend that has never heard of the field must not start
    /// seeing it merely because the struct grew.
    #[test]
    fn logprobs_fields_are_absent_unless_asked_for() {
        let req = ChatRequest::new("m", vec![ChatMessage::user("hi")]);
        let s = serde_json::to_string(&req).unwrap();
        assert!(!s.contains("logprobs"), "logprobs leaked: {s}");
        assert!(!s.contains("top_logprobs"), "top_logprobs leaked: {s}");
    }

    /// The exact pair OpenAI-compatible backends look for. `logprobs` is a
    /// bool and `top_logprobs` a count, and sending only the count is a 4xx
    /// on vLLM — so the builder sets both or neither.
    #[test]
    fn with_logprobs_emits_the_openai_wire_shape() {
        let req =
            ChatRequest::new("m", vec![ChatMessage::user("hi")]).with_logprobs(20);
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(v["logprobs"], serde_json::Value::Bool(true), "shape: {v}");
        assert_eq!(v["top_logprobs"], serde_json::json!(20), "shape: {v}");
    }

    /// `with_logprobs` must not disturb anything else the caller set —
    /// notably `chat_template_kwargs`, which the local leg stamps on
    /// unconditionally.
    #[test]
    fn with_logprobs_preserves_the_other_fields() {
        let req = ChatRequest::new("m", vec![ChatMessage::user("hi")])
            .without_thinking()
            .with_logprobs(5);
        assert_eq!(req.model, "m");
        assert_eq!(req.messages.len(), 1);
        assert!(req.chat_template_kwargs.is_some());
        assert_eq!(req.top_logprobs, Some(5));
    }

    /// Decoded from a real response measured against DGX Ollama 0.22.0 on
    /// 2026-08-16 (`/v1/chat/completions`, `top_logprobs: 5`) rather than
    /// reconstructed by hand — a fixture written from what we believe the
    /// shape to be pins our belief, not the wire ([[#566's lesson]]).
    #[test]
    fn chat_response_decodes_the_logprobs_envelope() {
        let raw = json!({
            "id": "chatcmpl-800",
            "model": "gemma4:26b-a4b-it-q8_0-ctx64k",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": ""},
                "finish_reason": "length",
                "logprobs": {"content": [{
                    "token": "yes",
                    "logprob": -0.000000004074,
                    "bytes": [121, 101, 115],
                    "top_logprobs": [
                        {"token": "yes", "logprob": -0.000000004074, "bytes": [121, 101, 115]},
                        {"token": "no",  "logprob": -20.2255,        "bytes": [110, 111]}
                    ]
                }]}
            }],
            "usage": {"prompt_tokens": 28, "completion_tokens": 1, "total_tokens": 29}
        });
        let resp: ChatResponse = serde_json::from_value(raw).unwrap();
        let lp = resp.choices[0].logprobs.as_ref().expect("logprobs decoded");
        assert_eq!(lp.content.len(), 1);
        assert_eq!(lp.content[0].token, "yes");
        assert_eq!(lp.content[0].top_logprobs.len(), 2);
        assert_eq!(lp.content[0].top_logprobs[1].token, "no");
        assert_eq!(
            lp.content[0].top_logprobs[0].bytes.as_deref(),
            Some([121u8, 101, 115].as_slice())
        );
    }

    /// Every backend that returns no logprobs — which is every call the
    /// planner makes — must keep decoding exactly as before.
    #[test]
    fn chat_response_without_logprobs_decodes_with_none() {
        let raw = json!({
            "model": "m",
            "choices": [{"message": {"role": "assistant", "content": "ok"}}]
        });
        let resp: ChatResponse = serde_json::from_value(raw).unwrap();
        assert!(resp.choices[0].logprobs.is_none());
    }

    #[test]
    fn chat_response_decodes_minimal_ollama_envelope() {
        // Ollama's OpenAI-compat front door omits `usage` entirely when
        // the underlying GGUF runtime didn't surface it. This test pins
        // that the decoder accepts the absence rather than failing.
        let raw = json!({
            "model": "llama3.2:3b",
            "choices": [{
                "message": {"role": "assistant", "content": "ok"}
            }]
        });
        let resp: ChatResponse = serde_json::from_value(raw).unwrap();
        assert!(resp.id.is_none());
        assert!(resp.usage.is_none());
        assert!(resp.choices[0].finish_reason.is_none());
        assert_eq!(resp.choices[0].index, 0);
    }
}
