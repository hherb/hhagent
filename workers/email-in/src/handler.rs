//! JSON-RPC dispatch for the three `email.*` methods. This worker makes NO
//! security decisions (spec D6, `docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`):
//! it never inspects `Authentication-Results` or any per-pairing token — it
//! only fetches raw material from localmail and hands it to core, which is
//! the only place a rejection is decided AND audited. Tests live in
//! `handler/tests.rs` (kept in a separate file so this one stays under the
//! project's 500-LOC guideline, mirroring core's `bus.rs`/`bus/tests.rs` split).

use kastellan_protocol::{codes, server::Handler, RpcError};

use crate::client::{EmailClient, EmailError};

pub struct EmailInHandler {
    client: EmailClient,
    /// Named localmail subscription this worker polls/acks — kastellan holds
    /// no inbound cursor state at all; localmail owns it (spec D7).
    subscription: String,
    /// The agent's own email address, echoed back by `email.init`.
    address: String,
}

impl EmailInHandler {
    pub fn from_env() -> anyhow::Result<Self> {
        let subscription = std::env::var("KASTELLAN_EMAIL_SUBSCRIPTION")
            .map_err(|_| anyhow::anyhow!("KASTELLAN_EMAIL_SUBSCRIPTION unset"))?;
        let address = std::env::var("KASTELLAN_EMAIL_ADDRESS")
            .map_err(|_| anyhow::anyhow!("KASTELLAN_EMAIL_ADDRESS unset"))?;
        Ok(Self { client: EmailClient::from_env()?, subscription, address })
    }

    #[cfg(test)]
    pub fn with_client(client: EmailClient, subscription: String, address: String) -> Self {
        Self { client, subscription, address }
    }

    /// `email.init` — identity/login-proof method the driver calls once at
    /// spawn (see `core/src/channel/polled_driver.rs::spawn`). No I/O: just
    /// echoes the worker's own configured identity.
    fn init(&self) -> Result<serde_json::Value, RpcError> {
        Ok(serde_json::json!({
            "address": self.address,
            "subscription": self.subscription,
        }))
    }

    /// `email.poll {timeout_ms}` — long-polls: call `changes`, and if it
    /// comes back empty, sleep in short slices (capped at 250ms, and further
    /// capped at whatever remains of the budget) until `timeout_ms` elapses,
    /// then return an empty event list. Every new message gets a
    /// `message_detail` fetch and is converted to a raw event via
    /// `build_event` — never filtered or judged here.
    fn poll(&self, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        #[derive(serde::Deserialize)]
        struct P {
            timeout_ms: u64,
        }
        let p: P = parse_params(params)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(p.timeout_ms);
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

        loop {
            let changes = self.client.changes(&self.subscription).map_err(email_err_to_rpc)?;
            let new_messages = changes
                .get("new_messages")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            if !new_messages.is_empty() {
                let mut events = Vec::with_capacity(new_messages.len());
                for m in &new_messages {
                    let Some(message_id) = m.get("message_id").and_then(|v| v.as_str()) else {
                        continue; // Malformed entry — skip rather than fail the whole batch.
                    };
                    let detail = self.client.message_detail(message_id).map_err(email_err_to_rpc)?;
                    if let Some(event) = build_event(&detail, message_id) {
                        events.push(event);
                    }
                }
                return Ok(serde_json::json!({ "events": events }));
            }

            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(serde_json::json!({ "events": [] }));
            }
            std::thread::sleep(POLL_INTERVAL.min(deadline - now));
        }
    }

    /// `email.ack {cursor}` — advances the localmail subscription cursor.
    /// Called by the driver once an event has been handed to the bus; see
    /// `PolledWorkerSpec::ack_method`.
    fn ack(&self, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        #[derive(serde::Deserialize)]
        struct P {
            cursor: String,
        }
        let p: P = parse_params(params)?;
        self.client.ack(&self.subscription, &p.cursor).map_err(email_err_to_rpc)?;
        Ok(serde_json::json!({ "ok": true }))
    }
}

impl Handler for EmailInHandler {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        match method {
            "email.init" => self.init(),
            "email.poll" => self.poll(params),
            "email.ack" => self.ack(params),
            _ => Err(RpcError::new(codes::METHOD_NOT_FOUND, format!("unknown method {method}"))),
        }
    }
}

fn parse_params<T: serde::de::DeserializeOwned>(params: serde_json::Value) -> Result<T, RpcError> {
    serde_json::from_value(params)
        .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))
}

fn email_err_to_rpc(e: EmailError) -> RpcError {
    match e {
        EmailError::BadParams(m) => RpcError::new(codes::INVALID_PARAMS, m),
        EmailError::Upstream { status: 401 | 403, .. } => RpcError::new(
            codes::POLICY_DENIED,
            "localmail auth/permission denied (check token / api-user grant)".to_string(),
        ),
        EmailError::Upstream { status, body } => {
            RpcError::new(codes::OPERATION_FAILED, format!("localmail {status}: {body}"))
        }
        EmailError::Transport(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("transport: {m}"))
        }
    }
}

/// Build one event from a localmail message detail
/// (`GET /v1/messages/{id}?headers=full`). Pure so it is unit-testable
/// without a transport. Returns `None` when the detail has no usable From
/// address — nothing to attribute the message to.
///
/// Field names are confirmed against localmail's own source, not guessed:
/// `from.address` and `body_text` are unconditional fields
/// (`localmail/src/localmail/api/messages.py` — `_address()` and the
/// `msg["body_text"]` assignment); `headers` is present only because this
/// worker's client always requests `?headers=full`, and — this is the part
/// the brief guessed wrong — every header's value is a JSON ARRAY of every
/// occurrence of that EXACT-cased header name, in wire order
/// (`localmail/src/localmail/parser.py`'s `_headers_dict`, which iterates
/// Python's `email.message.Message.items()`), never a bare string. See
/// `task-7-report.md` for the full confirmation trail.
///
/// `peer` is the From address, lowercased to match the normalization
/// `pair issue-token` applies at pairing time — a case mismatch would
/// silently never authorize. It always comes from the top-level
/// `from.address` field, never from any header.
///
/// `Reply-To` is deliberately never consulted for the peer: honouring it
/// would let a sender who passes the gate redirect the agent's reply to a
/// third party by simply setting a Reply-To header.
///
/// `auth_results` is every `Authentication-Results` header value, in wire
/// order — this worker never inspects them, because core's gate
/// (`core/src/channel/email/gate.rs`) decides which one counts, consulting
/// only the first, and needs the untouched wire order to do that safely.
pub fn build_event(detail: &serde_json::Value, message_id: &str) -> Option<serde_json::Value> {
    let from = detail
        .get("from")
        .and_then(|f| f.get("address"))
        .and_then(|a| a.as_str())?
        .trim()
        .to_ascii_lowercase();
    if from.is_empty() {
        return None;
    }

    let headers = detail.get("headers").and_then(|h| h.as_object());

    // Conversation = the RFC 5322 Message-ID, so a later slice's reply can
    // set In-Reply-To/References and thread. Fall back to a stable synthetic
    // value derived from the localmail id when the header is absent.
    let conversation = headers
        .map(|h| header_values(h, "Message-ID"))
        .unwrap_or_default()
        .into_iter()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("localmail:{message_id}"));

    let auth_results: Vec<String> = headers
        .map(|h| header_values(h, "Authentication-Results"))
        .unwrap_or_default()
        .into_iter()
        .map(String::from)
        .collect();

    let body = detail.get("body_text").and_then(|b| b.as_str()).unwrap_or("").to_string();

    Some(serde_json::json!({
        "peer": from,
        "conversation": conversation,
        "body": body,
        "ack_token": message_id,
        "auth_results": auth_results,
    }))
}

/// Every value under every header-map key that case-insensitively equals
/// `name`, concatenated in that key's own wire order.
///
/// localmail groups all wire occurrences of one EXACT-cased header name into
/// a single JSON array (see `build_event`'s doc), so the realistic case this
/// worker must get right — a two-milter Postfix emitting two
/// `Authentication-Results` headers with identical casing — comes back as
/// one key whose array preserves full wire order; that path is exercised by
/// `auth_results_are_returned_verbatim_and_in_order` in `handler/tests.rs`.
///
/// A header repeated with a DIFFERENT case becomes a second, distinct object
/// key in localmail's own JSON (confirmed by direct experiment against
/// Python's `email` package — see `task-7-report.md`). This workspace's
/// `serde_json` does not enable the `preserve_order` feature, so
/// `Value::Object` is backed by a `BTreeMap` (alphabetical key order) —
/// iterating across two *different-cased* keys is therefore NOT guaranteed
/// to reproduce true cross-key wire order. This function still returns every
/// value from every matching key (nothing is silently dropped), but that
/// specific cross-case-duplicate scenario cannot be given a wire-order
/// guarantee from the worker side; the loss of a unified ordering happens
/// upstream, in localmail's own parser, before this worker ever sees the
/// response.
fn header_values<'a>(headers: &'a serde_json::Map<String, serde_json::Value>, name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    for (k, v) in headers {
        if k.eq_ignore_ascii_case(name) {
            match v {
                serde_json::Value::Array(arr) => out.extend(arr.iter().filter_map(|x| x.as_str())),
                // Defensive only: real localmail responses always use an
                // array (`_headers_dict` groups every occurrence into a
                // list), even for a header seen exactly once.
                serde_json::Value::String(s) => out.push(s.as_str()),
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "handler/tests.rs"]
mod tests;
