//! Split out of `handler.rs` to keep that file under the project's 500-LOC
//! guideline (mirrors core's `bus.rs`/`bus/tests.rs` split). `use super::*`
//! brings in everything from `handler.rs` (`EmailInHandler`, `build_event`,
//! `header_values`, …).

use super::*;

#[test]
fn poll_maps_changes_and_detail_into_events() {
    let mut h = handler_with_canned();
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    let ev = &out["events"][0];
    assert_eq!(ev["peer"], "me@example.org", "peer is the From address");
    assert_eq!(ev["conversation"], "<mid-1@example.org>", "conversation is the Message-ID");
    assert_eq!(ev["ack_token"], "7", "ack_token is the localmail message id");
    assert!(ev["body"].as_str().unwrap().contains("what is 17*23"));
}

#[test]
fn from_address_is_lowercased_so_it_matches_the_paired_peer() {
    let mut h = handler_with_from("Me@Example.ORG");
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    assert_eq!(out["events"][0]["peer"], "me@example.org");
}

#[test]
fn reply_to_is_never_used_as_the_peer() {
    // Honouring Reply-To would let a sender who passes the gate redirect
    // the agent's reply to a third party.
    let mut h = handler_with_reply_to("attacker@evil.example");
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    assert_eq!(out["events"][0]["peer"], "me@example.org");
}

#[test]
fn auth_results_are_returned_verbatim_and_in_order() {
    let mut h = handler_with_auth_results(vec![
        "mx.example.net; dmarc=pass".to_string(),
        "evil.example.com; dmarc=pass".to_string(),
    ]);
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    let ar = out["events"][0]["auth_results"].as_array().unwrap();
    assert_eq!(ar.len(), 2, "every header is surfaced; core decides which counts");
    assert_eq!(ar[0], "mx.example.net; dmarc=pass", "wire order must be preserved");
    assert_eq!(ar[1], "evil.example.com; dmarc=pass", "wire order must be preserved");
}

#[test]
fn empty_changes_yields_no_events() {
    let mut h = handler_with_empty_changes();
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    assert_eq!(out["events"].as_array().unwrap().len(), 0);
}

#[test]
fn ack_posts_the_cursor_upstream() {
    let (mut h, recorder) = handler_recording_requests();
    h.call("email.ack", serde_json::json!({"cursor": "7"})).unwrap();
    assert!(recorder.lock().unwrap().iter().any(|r| r.contains("/v1/changes/ack")));
}

#[test]
fn unknown_method_is_rejected() {
    let mut h = handler_with_canned();
    assert!(h.call("email.nope", serde_json::json!({})).is_err());
}

#[test]
fn init_returns_configured_address_and_subscription() {
    let mut h = handler_with_canned();
    let out = h.call("email.init", serde_json::json!({})).unwrap();
    assert_eq!(out["address"], "agent@example.org");
    assert_eq!(out["subscription"], "sub");
}

// --- test helpers (fakes over kastellan_worker_web_common::http::HttpGet) ---

use kastellan_worker_web_common::http::{HttpGet, RawResponse};
use std::sync::{Arc, Mutex};
use url::Url;

fn json_resp(body: &serde_json::Value) -> RawResponse {
    RawResponse {
        status: 200,
        location: None,
        content_type: "application/json".into(),
        body: serde_json::to_vec(body).unwrap(),
    }
}

fn client_with(transport: Box<dyn HttpGet>) -> crate::client::EmailClient {
    crate::client::EmailClient::for_test(
        Url::parse("http://127.0.0.1:8443").unwrap(),
        "tok".into(),
        transport,
    )
}

fn changes_resp(ids: &[&str]) -> RawResponse {
    let msgs: Vec<serde_json::Value> = ids
        .iter()
        .map(|id| {
            serde_json::json!({
                "message_id": id,
                "subject": "s",
                "from": {"address": "ignored@example.org", "name": serde_json::Value::Null},
                "date": serde_json::Value::Null,
                "account": {"id": "1", "name": "acct"},
            })
        })
        .collect();
    json_resp(&serde_json::json!({
        "new_messages": msgs,
        "next_cursor": ids.last().unwrap_or(&"0"),
    }))
}

fn empty_changes_resp() -> RawResponse {
    json_resp(&serde_json::json!({"new_messages": [], "next_cursor": "0"}))
}

/// One localmail `GET /v1/messages/{id}?headers=full` response.
fn detail_resp(
    from: &str,
    message_id_header: Option<&str>,
    reply_to: Option<&str>,
    auth_results: &[&str],
) -> RawResponse {
    let mut headers = serde_json::Map::new();
    headers.insert("From".to_string(), serde_json::json!([from]));
    if let Some(mid) = message_id_header {
        headers.insert("Message-ID".to_string(), serde_json::json!([mid]));
    }
    if let Some(rt) = reply_to {
        headers.insert("Reply-To".to_string(), serde_json::json!([rt]));
    }
    if !auth_results.is_empty() {
        headers.insert("Authentication-Results".to_string(), serde_json::json!(auth_results));
    }
    json_resp(&serde_json::json!({
        "id": "7",
        "subject": "s",
        "from": {"address": from, "name": serde_json::Value::Null},
        "body_text": "what is 17*23",
        "body_html": serde_json::Value::Null,
        "headers": headers,
    }))
}

fn path_and_query(url: &Url) -> String {
    match url.query() {
        Some(q) => format!("{}?{}", url.path(), q),
        None => url.path().to_string(),
    }
}

/// Fake transport: GETs to a `/v1/changes` path get `changes`, anything
/// else (the message detail fetch) gets `detail`. Not FIFO — safe to call
/// any number of times, which the long-poll retry loop needs.
struct PathFake {
    changes: RawResponse,
    detail: RawResponse,
}
impl HttpGet for PathFake {
    fn get(&self, _u: &Url) -> Result<RawResponse, String> {
        unreachable!("client uses get_authed")
    }
    fn transport_kind(&self) -> &'static str {
        "fake"
    }
    fn get_authed(&self, url: &Url, _bearer: &str, _max: usize) -> Result<RawResponse, String> {
        if url.path().starts_with("/v1/changes") {
            Ok(self.changes.clone())
        } else {
            Ok(self.detail.clone())
        }
    }
}

fn fake_with(changes: RawResponse, detail: RawResponse) -> Box<dyn HttpGet> {
    Box::new(PathFake { changes, detail })
}

fn handler_with_canned() -> crate::handler::EmailInHandler {
    let transport = fake_with(
        changes_resp(&["7"]),
        detail_resp("me@example.org", Some("<mid-1@example.org>"), None, &[]),
    );
    crate::handler::EmailInHandler::with_client(
        client_with(transport),
        "sub".to_string(),
        "agent@example.org".to_string(),
    )
}

fn handler_with_from(addr: &str) -> crate::handler::EmailInHandler {
    let transport = fake_with(
        changes_resp(&["7"]),
        detail_resp(addr, Some("<mid-1@example.org>"), None, &[]),
    );
    crate::handler::EmailInHandler::with_client(
        client_with(transport),
        "sub".to_string(),
        "agent@example.org".to_string(),
    )
}

fn handler_with_reply_to(reply_to: &str) -> crate::handler::EmailInHandler {
    let transport = fake_with(
        changes_resp(&["7"]),
        detail_resp("me@example.org", Some("<mid-1@example.org>"), Some(reply_to), &[]),
    );
    crate::handler::EmailInHandler::with_client(
        client_with(transport),
        "sub".to_string(),
        "agent@example.org".to_string(),
    )
}

fn handler_with_auth_results(v: Vec<String>) -> crate::handler::EmailInHandler {
    let refs: Vec<&str> = v.iter().map(String::as_str).collect();
    let transport = fake_with(
        changes_resp(&["7"]),
        detail_resp("me@example.org", Some("<mid-1@example.org>"), None, &refs),
    );
    crate::handler::EmailInHandler::with_client(
        client_with(transport),
        "sub".to_string(),
        "agent@example.org".to_string(),
    )
}

fn handler_with_empty_changes() -> crate::handler::EmailInHandler {
    let transport = fake_with(
        empty_changes_resp(),
        detail_resp("me@example.org", None, None, &[]),
    );
    crate::handler::EmailInHandler::with_client(
        client_with(transport),
        "sub".to_string(),
        "agent@example.org".to_string(),
    )
}

/// Fake transport recording every request's method + path+query, so the
/// ack test can assert what was actually sent upstream.
struct RecordingFake {
    log: Arc<Mutex<Vec<String>>>,
}
impl HttpGet for RecordingFake {
    fn get(&self, _u: &Url) -> Result<RawResponse, String> {
        unreachable!("client uses get_authed/post_authed")
    }
    fn transport_kind(&self) -> &'static str {
        "fake"
    }
    fn get_authed(&self, url: &Url, _bearer: &str, _max: usize) -> Result<RawResponse, String> {
        self.log.lock().unwrap().push(format!("GET {}", path_and_query(url)));
        Ok(empty_changes_resp())
    }
    fn post_authed(
        &self,
        url: &Url,
        _bearer: &str,
        _content_type: &str,
        _body: &[u8],
        _max: usize,
    ) -> Result<RawResponse, String> {
        self.log.lock().unwrap().push(format!("POST {}", path_and_query(url)));
        Ok(RawResponse { status: 204, location: None, content_type: String::new(), body: Vec::new() })
    }
}

fn handler_recording_requests() -> (crate::handler::EmailInHandler, Arc<Mutex<Vec<String>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let transport: Box<dyn HttpGet> = Box::new(RecordingFake { log: log.clone() });
    let h = crate::handler::EmailInHandler::with_client(
        client_with(transport),
        "sub".to_string(),
        "agent@example.org".to_string(),
    );
    (h, log)
}
