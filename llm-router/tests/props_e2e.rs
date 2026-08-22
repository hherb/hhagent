//! `Router::props()` against a hand-rolled GET mock.
//!
//! `/props` is how the guard-weights pin (issue #592) learns which file
//! `llama-server` actually opened: llama.cpp's `/v1/models` reports an
//! **empty** `digest`, so the only thing the endpoint can tell us is the
//! path, and we hash that ourselves.
//!
//! Deliberately a separate file from `local_backend_e2e.rs` rather than
//! more tests appended to it: that file was already at the 500-line cap,
//! and this fixture is a much simpler animal — a GET with no request
//! body to parse. Per the repo's own rule, split *before* the change
//! that grows a file.

use std::time::Duration;

use kastellan_llm_router::{Router, RouterConfig, RouterError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// Bind a one-shot HTTP/1.1 GET mock on an ephemeral port.
///
/// Returns the server's base origin and a receiver that fires with the
/// whole **request line** the mock actually saw — method included.
///
/// The method matters and was once dropped here: capturing only the
/// path let `self.http.get(..)` become `.post(..)` while every test in
/// this file still passed, against a real llama-server that answers
/// `/props` with 405 for anything but GET. The mock answers whatever
/// verb it is sent, so nothing else in this file would notice.
async fn spawn_get_mock(status_line: &'static str, body: String) -> (String, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    let origin = format!("http://127.0.0.1:{port}");

    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let Ok((mut sock, _peer)) = listener.accept().await else { return };
        // A GET carries no body, so the request is complete at the
        // first CRLFCRLF — no Content-Length handling needed.
        let mut buf = Vec::with_capacity(2048);
        let mut tmp = [0u8; 1024];
        loop {
            let Ok(n) = sock.read(&mut tmp).await else { return };
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf).to_string();
        let request_line = head.lines().next().unwrap_or("").trim().to_string();
        let _ = tx.send(request_line);

        let resp = format!(
            "{status_line}\r\nContent-Type: application/json\r\n\
             Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
            len = body.len(),
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        let _ = sock.flush().await;
    });

    (origin, rx)
}

/// A router pointed at `local_url`, with a short timeout so a broken
/// test fails fast instead of hanging the suite.
fn router_at(local_url: String) -> Router {
    let cfg = RouterConfig {
        local_url,
        // Short, so a broken test fails fast instead of sitting on the
        // production default and hanging the suite.
        timeout: Duration::from_secs(5),
        ..RouterConfig::default()
    };
    Router::new(cfg).expect("build router")
}

/// Abridged from the DGX's live llama-server, 2026-08-22.
fn props_body() -> String {
    serde_json::json!({
        "model_alias": "shieldstral",
        "model_ftype": "Q8_0",
        "model_path": "/home/hherb/models/shieldstral/upstream/Shieldstral-1.0-3B-Q8_0.gguf",
    })
    .to_string()
}

#[tokio::test]
async fn props_returns_the_parsed_body() {
    let (origin, _served) = spawn_get_mock("HTTP/1.1 200 OK", props_body()).await;
    let router = router_at(format!("{origin}/v1"));

    let props = router.props().await.expect("props must succeed");

    assert_eq!(
        props.get("model_path").and_then(|v| v.as_str()),
        Some("/home/hherb/models/shieldstral/upstream/Shieldstral-1.0-3B-Q8_0.gguf")
    );
}

/// The load-bearing routing fact: `/props` lives at the server **root**,
/// while `/chat/completions` lives under the compat prefix. Addressing
/// `/v1/props` gets a 404 from a real llama-server, which would surface
/// as "cannot verify the weights" on a server that is serving fine.
#[tokio::test]
async fn props_addresses_the_server_root_not_the_compat_prefix() {
    let (origin, served) = spawn_get_mock("HTTP/1.1 200 OK", props_body()).await;
    let router = router_at(format!("{origin}/v1"));

    router.props().await.expect("props must succeed");

    assert_eq!(served.await.expect("mock served a request"), "GET /props HTTP/1.1");
}

#[tokio::test]
async fn props_reports_a_non_success_status() {
    let (origin, _served) =
        spawn_get_mock("HTTP/1.1 404 Not Found", "no such endpoint".to_string()).await;
    let router = router_at(format!("{origin}/v1"));

    match router.props().await {
        Err(RouterError::HttpStatus { status, .. }) => assert_eq!(status, 404),
        other => panic!("expected HttpStatus 404, got {other:?}"),
    }
}

/// A non-llama.cpp backend answering 200 with HTML is the realistic
/// shape here, and it must not be mistaken for a chat-decode failure —
/// the diagnosis is "this endpoint is not a llama.cpp server".
#[tokio::test]
async fn props_reports_a_non_json_body_as_its_own_failure() {
    let (origin, _served) =
        spawn_get_mock("HTTP/1.1 200 OK", "<html>hello</html>".to_string()).await;
    let router = router_at(format!("{origin}/v1"));

    match router.props().await {
        Err(RouterError::DecodeProps { body, .. }) => {
            assert!(body.contains("<html>"), "raw body must be carried: {body}");
        }
        other => panic!("expected DecodeProps, got {other:?}"),
    }
}

#[tokio::test]
async fn props_reports_a_dead_backend_as_transport() {
    // Port 1 on loopback: nothing listens, so this is a connect failure.
    let router = router_at("http://127.0.0.1:1/v1".to_string());

    match router.props().await {
        Err(RouterError::Transport(_)) => {}
        other => panic!("expected Transport, got {other:?}"),
    }
}
