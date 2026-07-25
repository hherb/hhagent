//! Plain-HTTP canned-response mock of localmail's `/v1` REST API, serving the
//! six endpoints the mail worker hits in localmail's REAL response shapes (as
//! #487 corrected them: search → `results`, attachment text → `application/json
//! {"text": …}`). Plain HTTP is deliberate: it sidesteps the webpki-only TLS
//! wall entirely (that only bites TLS), so the mail worker's DIRECT transport
//! round-trips hermetically against it. It is NOT reachable via the force-routed
//! transport (HTTPS-only) — see the mail e2e egress tier. Response SHAPES are
//! pinned against real localmail by the Mac-only contract test in
//! `core/tests/mail_daemon_e2e.rs`.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 64 lowercase hex — the attachment sha the canned message advertises and the
/// attachment endpoints key on (the worker validates sha256 shape).
pub const CANNED_SHA256: &str =
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// Extracted text surfaced by `mail.get_attachment_text`.
pub const CANNED_ATTACHMENT_TEXT: &str = "NORTH COAST AREA HEALTH SERVICE invoice total 42.00";
/// Original-format bytes delivered by `mail.get_attachment`.
pub const CANNED_ATTACHMENT_BYTES: &[u8] = b"%PDF-1.4 canned attachment bytes";
/// The message id the canned search/list hits reference.
pub const CANNED_MESSAGE_ID: i64 = 7;

/// A live plain-HTTP localmail mock. Aborts its listener task on drop.
pub struct MockLocalmail {
    pub base_url: String,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for MockLocalmail {
    fn drop(&mut self) {
        if let Some(h) = self.join.take() {
            h.abort();
        }
    }
}

/// Bind an ephemeral loopback port and serve the six `/v1` endpoints. Every
/// request must carry a non-empty `Authorization: Bearer` header (asserted).
pub async fn spawn_mock_localmail() -> MockLocalmail {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");

    let join = tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            serve_localmail_conn(&mut sock).await;
        }
    });

    MockLocalmail { base_url, join: Some(join) }
}

/// Serve one localmail connection: read the request head (draining the declared
/// body so the close is a clean FIN, not an RST that truncates the client's
/// read), route it via [`route`], and write the response. Generic over the
/// stream so the plain-TCP and TLS spawns share exactly one implementation.
async fn serve_localmail_conn<S>(sock: &mut S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Read until end-of-headers, THEN drain the declared Content-Length body.
    // localmail's search is a POST; its body doesn't change the canned page,
    // but we must consume it before responding+closing — a socket closed with
    // unread inbound bytes is RST'd by the kernel, which can truncate the
    // response the client is mid-read on (the sibling `scripted_llm` drains
    // its body for exactly this reason). The request line + headers are all
    // we route on.
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 512];
    let head = loop {
        let n = match sock.read(&mut tmp).await {
            Ok(0) | Err(_) => break None,
            Ok(n) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let header_str = match std::str::from_utf8(&buf[..i]) {
                Ok(s) => s.to_owned(),
                Err(_) => break None,
            };
            // Consume the body so the close is a clean FIN, not an RST.
            let want = (i + 4) + content_length(&header_str);
            while buf.len() < want && buf.len() <= 64 * 1024 {
                match sock.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                }
            }
            break Some(header_str);
        }
        if buf.len() > 64 * 1024 {
            break None;
        }
    };
    let (status, ctype, body): (&str, &str, Vec<u8>) = match head.as_deref() {
        Some(h) => route(h),
        None => ("400 Bad Request", "text/plain", b"bad request".to_vec()),
    };
    let resp_head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = sock.write_all(resp_head.as_bytes()).await;
    let _ = sock.write_all(&body).await;
    let _ = sock.flush().await;
    let _ = sock.shutdown().await;
}

/// A live **self-signed-HTTPS** localmail mock at `https://127.0.0.1:<port>`.
/// Returns the mock (aborts its listener on drop) and the cert PEM (the caller
/// writes it wherever the egress proxy's upstream extra CA must live). Serves the
/// identical `/v1` shapes as [`spawn_mock_localmail`] — the force-routed MITM path
/// can reach it once the proxy is given this cert as its upstream extra CA (#491).
pub async fn spawn_mock_localmail_tls() -> (MockLocalmail, String) {
    let (cert_der, key_der, cert_pem) = crate::tls_origin::generate_loopback_cert();
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("build localmail tls server config");
    let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("https://127.0.0.1:{port}");

    let join = tokio::spawn(async move {
        loop {
            let (tcp, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let mut tls = match acceptor.accept(tcp).await {
                    Ok(t) => t,
                    Err(_) => return,
                };
                serve_localmail_conn(&mut tls).await;
            });
        }
    });

    (MockLocalmail { base_url, join: Some(join) }, cert_pem)
}

/// The request's `Content-Length` (0 when absent or unparseable). Used only to
/// know how many body bytes to drain before closing the connection.
fn content_length(head: &str) -> usize {
    for line in head.lines() {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                return value.trim().parse().unwrap_or(0);
            }
        }
    }
    0
}

/// Pure request-line/headers → (status, content-type, body). Asserts a
/// non-empty bearer so the auth wiring is exercised, then routes by path.
fn route(head: &str) -> (&'static str, &'static str, Vec<u8>) {
    // Bearer presence (auth wiring). A request with no non-empty bearer is a 401.
    let has_bearer = head.lines().any(|l| {
        let mut p = l.splitn(2, ':');
        matches!((p.next(), p.next()), (Some(n), Some(v))
            if n.trim().eq_ignore_ascii_case("authorization")
               && v.trim().strip_prefix("Bearer ").map(|t| !t.trim().is_empty()).unwrap_or(false))
    });
    if !has_bearer {
        return ("401 Unauthorized", "text/plain", b"no bearer".to_vec());
    }
    let path = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/");

    let json = |s: String| ("200 OK", "application/json", s.into_bytes());

    // /v1/messages/<id> exactly or with a query string — NOT a numeric prefix
    // (so id 7 does not also match 70..=79). Bound here so the if-chain below
    // stays a simple condition (clippy::blocks_in_conditions).
    let is_message_by_id = {
        let m = format!("/v1/messages/{CANNED_MESSAGE_ID}");
        path == m || path.starts_with(&format!("{m}?"))
    };

    // Order matters: the more specific attachment paths before /v1/messages.
    if path.starts_with("/v1/search") {
        json(serde_json::json!({
            "results": [{"message_id": CANNED_MESSAGE_ID, "subject": "invoice", "snippet": "…"}],
            "next_cursor": serde_json::Value::Null
        }).to_string())
    } else if path.starts_with("/v1/accounts") {
        json(serde_json::json!([{"id": 1, "name": "horst-gmail"}]).to_string())
    } else if path.contains("/text") && path.starts_with("/v1/attachments/") {
        json(serde_json::json!({"text": CANNED_ATTACHMENT_TEXT}).to_string())
    } else if path.starts_with("/v1/attachments/") {
        ("200 OK", "application/pdf", CANNED_ATTACHMENT_BYTES.to_vec())
    } else if is_message_by_id {
        json(serde_json::json!({
            "id": CANNED_MESSAGE_ID,
            "subject": "invoice",
            "from": "billing@example.test",
            "body": "please find the invoice attached",
            "attachments": [{
                "filename": "invoice.pdf",
                "sha256": CANNED_SHA256,
                "content_type": "application/pdf",
                "size": CANNED_ATTACHMENT_BYTES.len()
            }]
        }).to_string())
    } else if path.starts_with("/v1/messages") {
        json(serde_json::json!({
            "results": [{"message_id": CANNED_MESSAGE_ID, "subject": "invoice"}],
            "next_cursor": serde_json::Value::Null
        }).to_string())
    } else {
        ("404 Not Found", "text/plain", b"no such endpoint".to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    /// Drive one raw GET /v1/accounts against the mock and confirm it answers
    /// with the localmail accounts array shape (a JSON list).
    #[test]
    fn serves_accounts_as_a_json_array() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mock = rt.block_on(spawn_mock_localmail());
        let addr = mock.base_url.strip_prefix("http://").unwrap().to_string();
        let mut s = TcpStream::connect(&addr).unwrap();
        write!(
            s,
            "GET /v1/accounts HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200"), "resp: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap();
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert!(v.is_array(), "accounts must be a JSON array, got {v}");
    }

    /// The attachment-text endpoint must return the localmail envelope shape
    /// `application/json {"text": …}` (the #487 contract), NOT plain text.
    #[test]
    fn attachment_text_is_json_text_envelope() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mock = rt.block_on(spawn_mock_localmail());
        let addr = mock.base_url.strip_prefix("http://").unwrap().to_string();
        let mut s = TcpStream::connect(&addr).unwrap();
        write!(
            s,
            "GET /v1/attachments/{CANNED_SHA256}/text HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        assert!(resp.contains("application/json"), "content-type: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap();
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["text"], CANNED_ATTACHMENT_TEXT);
    }

    /// A request with no bearer is refused (auth wiring is exercised).
    #[test]
    fn missing_bearer_is_401() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mock = rt.block_on(spawn_mock_localmail());
        let addr = mock.base_url.strip_prefix("http://").unwrap().to_string();
        let mut s = TcpStream::connect(&addr).unwrap();
        write!(s, "GET /v1/accounts HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 401"), "no-bearer must 401, got: {resp}");
    }

    /// The TLS mock serves the same `/v1/search` `results` shape as the plain mock,
    /// over TLS, to a client trusting only the returned cert — the exact trust path
    /// the force-routed MITM e2e relies on (proxy upstream extra CA), without a sandbox.
    #[test]
    fn tls_mock_serves_search_results_over_tls() {
        use rustls_pki_types::pem::PemObject;
        use rustls_pki_types::{CertificateDer, ServerName};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;
        use tokio_rustls::TlsConnector;

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (mock, cert_pem) = spawn_mock_localmail_tls().await;
            let port: u16 = mock.base_url.rsplit(':').next().unwrap().parse().unwrap();

            let mut roots = rustls::RootCertStore::empty();
            roots.add(CertificateDer::from_pem_slice(cert_pem.as_bytes()).unwrap()).unwrap();
            let cfg = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let connector = TlsConnector::from(std::sync::Arc::new(cfg));

            let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let sni = ServerName::IpAddress(std::net::Ipv4Addr::LOCALHOST.into());
            let mut tls = connector.connect(sni, tcp).await.expect("tls handshake");
            tls.write_all(
                b"POST /v1/search HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer t\r\n\
                  Content-Length: 0\r\nConnection: close\r\n\r\n",
            ).await.unwrap();
            let mut resp = Vec::new();
            tls.read_to_end(&mut resp).await.unwrap();
            let resp = String::from_utf8_lossy(&resp);
            assert!(resp.starts_with("HTTP/1.1 200"), "resp: {resp}");
            let body = resp.split("\r\n\r\n").nth(1).unwrap();
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            assert!(v["results"].is_array(), "expected results array, got {v}");
        });
    }
}
