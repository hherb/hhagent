//! localmail REST client for the email channel's inbound half. Reuses
//! web-common's transport (`make_get`) so force-routing — proxy-CONNECT over
//! the egress UDS with the per-instance MITM CA, or the extra-CA seam for a
//! self-signed private origin (#492) — works unchanged; adds
//! `Authorization: Bearer` via `get_authed`/`post_authed`, exactly as
//! `workers/mail`'s client does. Three endpoints only: the tail-subscription
//! poll, one message's full-header detail, and the ack that advances the
//! server-side cursor.
//!
//! This module makes no security decisions either — see `main.rs`'s module
//! doc for why that boundary is drawn at the worker/core line, not here.

use kastellan_worker_web_common::http::{make_get, HttpGet, RawResponse};
use url::Url;

/// Cap for JSON responses (changes pages, message detail). No attachment
/// downloads happen in this worker, so unlike `workers/mail` there is only
/// one cap.
const JSON_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Failure modes surfaced to the handler, which maps them to JSON-RPC errors.
#[derive(Debug)]
pub enum EmailError {
    /// The worker built a bad request (unparsable path, bad JSON body, etc.).
    BadParams(String),
    /// localmail returned a non-2xx status.
    Upstream { status: u16, body: String },
    /// Transport or decode failure (no route, bad JSON, cap exceeded).
    Transport(String),
}

pub struct EmailClient {
    base: Url,
    token: String,
    transport: Box<dyn HttpGet>,
}

impl EmailClient {
    /// Build from the worker's environment: `KASTELLAN_EMAIL_ENDPOINT` (base
    /// URL) and `KASTELLAN_EMAIL_TOKEN_FILE` (0600 file holding the bearer
    /// token) — same env-var naming convention and token-file pattern as
    /// `workers/mail`. Transport is selected by `make_get` (proxy-CONNECT
    /// when force-routed, else direct).
    pub fn from_env() -> anyhow::Result<Self> {
        let base = std::env::var("KASTELLAN_EMAIL_ENDPOINT")
            .map_err(|_| anyhow::anyhow!("KASTELLAN_EMAIL_ENDPOINT unset"))?;
        let base = Url::parse(&base)
            .map_err(|e| anyhow::anyhow!("KASTELLAN_EMAIL_ENDPOINT invalid: {e}"))?;
        let token_file = std::env::var("KASTELLAN_EMAIL_TOKEN_FILE")
            .map_err(|_| anyhow::anyhow!("KASTELLAN_EMAIL_TOKEN_FILE unset"))?;
        let token = std::fs::read_to_string(&token_file)
            .map_err(|e| anyhow::anyhow!("read token file {token_file}: {e}"))?
            .trim()
            .to_string();
        if token.is_empty() {
            anyhow::bail!("token file {token_file} is empty");
        }
        let transport = make_get("kastellan-email-in/0")?;
        Ok(Self { base, token, transport })
    }

    #[cfg(test)]
    pub fn for_test(base: Url, token: String, transport: Box<dyn HttpGet>) -> Self {
        Self { base, token, transport }
    }

    fn url(&self, path: &str) -> Result<Url, EmailError> {
        self.base
            .join(path)
            .map_err(|e| EmailError::BadParams(format!("bad path {path}: {e}")))
    }

    /// Reject a non-2xx upstream response, clamping the echoed body — same
    /// truncate-before-lossy-decode reasoning as `workers/mail`'s `check`.
    fn check(resp: RawResponse) -> Result<RawResponse, EmailError> {
        if (200..300).contains(&resp.status) {
            Ok(resp)
        } else {
            let snippet = &resp.body[..resp.body.len().min(512)];
            Err(EmailError::Upstream {
                status: resp.status,
                body: String::from_utf8_lossy(snippet).into_owned(),
            })
        }
    }

    fn get_json_at(&self, url: Url) -> Result<serde_json::Value, EmailError> {
        let resp = self
            .transport
            .get_authed(&url, &self.token, JSON_MAX_BYTES)
            .map_err(EmailError::Transport)?;
        let resp = Self::check(resp)?;
        serde_json::from_slice(&resp.body)
            .map_err(|e| EmailError::Transport(format!("bad json: {e}")))
    }

    /// `GET /v1/changes?subscription=<name>` — messages newer than the
    /// server-side cursor for this subscription. A brand-new subscription
    /// returns `new_messages: []` deliberately (localmail starts it at the
    /// tip, not the backlog).
    pub fn changes(&self, subscription: &str) -> Result<serde_json::Value, EmailError> {
        let mut url = self.url("/v1/changes")?;
        url.query_pairs_mut().append_pair("subscription", subscription);
        self.get_json_at(url)
    }

    /// `GET /v1/messages/{id}?headers=full` — full headers, needed for
    /// `Authentication-Results` and `Message-ID`.
    pub fn message_detail(&self, id: &str) -> Result<serde_json::Value, EmailError> {
        let mut url = self.url(&format!("/v1/messages/{id}"))?;
        url.query_pairs_mut().append_pair("headers", "full");
        self.get_json_at(url)
    }

    /// `POST /v1/changes/ack {"subscription": …, "cursor": …}` — advances
    /// the server-side cursor. 204 empty body on success; never parsed as
    /// JSON (there is nothing to parse).
    pub fn ack(&self, subscription: &str, cursor: &str) -> Result<(), EmailError> {
        let url = self.url("/v1/changes/ack")?;
        let body = serde_json::json!({ "subscription": subscription, "cursor": cursor });
        let raw = serde_json::to_vec(&body).map_err(|e| EmailError::BadParams(e.to_string()))?;
        let resp = self
            .transport
            .post_authed(&url, &self.token, "application/json", &raw, JSON_MAX_BYTES)
            .map_err(EmailError::Transport)?;
        Self::check(resp)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_worker_web_common::http::RawResponse;

    struct FakeChanges;
    impl HttpGet for FakeChanges {
        fn get(&self, _u: &Url) -> Result<RawResponse, String> {
            unreachable!("client uses get_authed")
        }
        fn transport_kind(&self) -> &'static str {
            "fake"
        }
        fn get_authed(&self, url: &Url, bearer: &str, _max: usize) -> Result<RawResponse, String> {
            assert_eq!(bearer, "tok123");
            assert_eq!(url.path(), "/v1/changes");
            assert_eq!(url.query(), Some("subscription=agent-inbox"));
            Ok(RawResponse {
                status: 200,
                location: None,
                content_type: "application/json".into(),
                body: br#"{"new_messages":[],"next_cursor":"0"}"#.to_vec(),
            })
        }
    }

    #[test]
    fn changes_uses_bearer_and_subscription_query() {
        let c = EmailClient::for_test(
            Url::parse("http://127.0.0.1:8443").unwrap(),
            "tok123".into(),
            Box::new(FakeChanges),
        );
        let v = c.changes("agent-inbox").unwrap();
        assert_eq!(v["next_cursor"], "0");
    }

    struct FakeDetail;
    impl HttpGet for FakeDetail {
        fn get(&self, _u: &Url) -> Result<RawResponse, String> {
            unreachable!()
        }
        fn transport_kind(&self) -> &'static str {
            "fake"
        }
        fn get_authed(&self, url: &Url, _bearer: &str, _max: usize) -> Result<RawResponse, String> {
            assert_eq!(url.path(), "/v1/messages/42");
            assert_eq!(url.query(), Some("headers=full"));
            Ok(RawResponse {
                status: 200,
                location: None,
                content_type: "application/json".into(),
                body: br#"{"id":"42"}"#.to_vec(),
            })
        }
    }

    #[test]
    fn message_detail_requests_full_headers() {
        let c = EmailClient::for_test(
            Url::parse("http://127.0.0.1:8443").unwrap(),
            "t".into(),
            Box::new(FakeDetail),
        );
        let v = c.message_detail("42").unwrap();
        assert_eq!(v["id"], "42");
    }

    struct FakeAck;
    impl HttpGet for FakeAck {
        fn get(&self, _u: &Url) -> Result<RawResponse, String> {
            unreachable!()
        }
        fn transport_kind(&self) -> &'static str {
            "fake"
        }
        fn post_authed(
            &self,
            url: &Url,
            bearer: &str,
            content_type: &str,
            body: &[u8],
            _max: usize,
        ) -> Result<RawResponse, String> {
            assert_eq!(bearer, "tok123");
            assert_eq!(content_type, "application/json");
            assert_eq!(url.path(), "/v1/changes/ack");
            let v: serde_json::Value = serde_json::from_slice(body).unwrap();
            assert_eq!(v["subscription"], "agent-inbox");
            assert_eq!(v["cursor"], "7");
            Ok(RawResponse { status: 204, location: None, content_type: String::new(), body: Vec::new() })
        }
    }

    #[test]
    fn ack_posts_subscription_and_cursor() {
        let c = EmailClient::for_test(
            Url::parse("http://127.0.0.1:8443").unwrap(),
            "tok123".into(),
            Box::new(FakeAck),
        );
        c.ack("agent-inbox", "7").unwrap();
    }

    struct Fake403;
    impl HttpGet for Fake403 {
        fn get(&self, _u: &Url) -> Result<RawResponse, String> {
            unreachable!()
        }
        fn transport_kind(&self) -> &'static str {
            "fake"
        }
        fn get_authed(&self, _url: &Url, _bearer: &str, _max: usize) -> Result<RawResponse, String> {
            Ok(RawResponse { status: 403, location: None, content_type: "text/plain".into(), body: b"forbidden".to_vec() })
        }
    }

    #[test]
    fn non_2xx_is_upstream_error() {
        let c = EmailClient::for_test(
            Url::parse("http://127.0.0.1:8443").unwrap(),
            "t".into(),
            Box::new(Fake403),
        );
        match c.changes("sub") {
            Err(EmailError::Upstream { status: 403, .. }) => {}
            other => panic!("expected Upstream 403, got {other:?}"),
        }
    }
}
