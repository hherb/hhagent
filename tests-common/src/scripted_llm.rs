//! Queued multi-shot mock LLM for daemon e2e tests: an OpenAI-compatible HTTP
//! listener that dispatches canned responses from a per-endpoint FIFO (embed vs.
//! chat), chosen by the request path. Lifted from `core/tests/cli_ask_e2e.rs`
//! (mail-worker live-test coverage) so more than one daemon e2e can drive a
//! scripted planner. The `ScriptedLlm` name distinguishes it from the inert
//! `daemon::MockLlm` (which 503s every request).

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Hard cap on inbound request bytes the mock will buffer before
/// giving up. Real chat-completion requests are a few KiB; 1 MiB is
/// generous headroom that defends against a buggy client pinning the
/// mock task in an unbounded read.
const MOCK_MAX_REQUEST_BYTES: usize = 1 << 20;

/// The kind of OpenAI-compatible endpoint a captured request targets.
///
/// Used by the URL-routing mock to dispatch responses from the right
/// per-endpoint queue and to keep capture lists separate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointKind {
    Embedding,
    Chat,
}

/// Classify a request path into one of the two endpoint kinds the
/// daemon actually exercises.
///
/// Production paths look like `/v1/embeddings` and `/v1/chat/completions`,
/// but we deliberately match by substring rather than exact equality
/// — that way a future router refactor that changes the URL prefix
/// (or adds a trailing `?stream=false`) does not silently break this
/// classifier. Anything that contains `embeddings` is an embed request;
/// every other path is treated as a chat-completion. Pure: `&str → Kind`.
fn classify_endpoint(path: &str) -> EndpointKind {
    if path.contains("embeddings") {
        EndpointKind::Embedding
    } else {
        EndpointKind::Chat
    }
}

/// Extract the request-target (path) from an HTTP request-line string,
/// e.g. `"POST /v1/embeddings HTTP/1.1"` → `"/v1/embeddings"`.
///
/// Returns `None` if the line doesn't split into at least three
/// whitespace-separated tokens. Pure: `&str → Option<&str>`.
fn parse_request_path(headers: &str) -> Option<&str> {
    let first_line = headers.lines().next()?;
    let mut parts = first_line.split_whitespace();
    let _method = parts.next()?;
    parts.next()
}

/// Multi-shot HTTP mock for the LLM router, dispatching by URL path.
///
/// Serves canned 200-OK JSON bodies from one of two queues — embedding
/// or chat-completion — chosen by the request's URL path. Each queue
/// is FIFO; once a queue is exhausted, every subsequent request to that
/// endpoint gets a `503 Service Unavailable` so an unexpected extra
/// LLM call surfaces as `RouterError::HttpStatus` in the daemon log
/// AND as a `tasks.state = "failed"` row in the test's final assertion
/// — i.e. loud, not silent.
///
/// **Why per-endpoint queues, not a single FIFO** — the daemon's
/// `PgRecallBuilder::build` issues an embed before the chat-completion
/// today, but that ordering is not load-bearing on production behaviour.
/// A single shared FIFO would desync silently if a future refactor
/// parallelises embed+chat (the chat handler pops an embedding body or
/// vice-versa) or if any new caller adds an extra embed somewhere
/// upstream. Two queues fail loudly: an unexpected dial-count mismatch
/// surfaces as a 503 on the correct endpoint, not a misleading body-
/// shape error in the consumer.
///
/// The accept loop runs forever (one connection at a time) until the
/// `JoinHandle` is aborted. `Drop` aborts it for us so the mock cannot
/// leak past the test boundary.
pub struct ScriptedLlm {
    pub base_url: String,
    /// Captured embedding-request bodies in arrival order. Useful for
    /// asserting the daemon dialed the embed endpoint N times.
    pub embed_requests: Arc<Mutex<Vec<String>>>,
    /// Captured chat-completion request bodies in arrival order.
    pub chat_requests: Arc<Mutex<Vec<String>>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for ScriptedLlm {
    fn drop(&mut self) {
        if let Some(h) = self.join.take() {
            h.abort();
        }
    }
}

pub async fn spawn_scripted_llm(
    embed_responses: Vec<String>,
    chat_responses: Vec<String>,
) -> ScriptedLlm {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");

    let embed_queue = Arc::new(Mutex::new(embed_responses));
    let embed_queue_for_task = embed_queue.clone();
    let chat_queue = Arc::new(Mutex::new(chat_responses));
    let chat_queue_for_task = chat_queue.clone();
    let embed_requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let embed_requests_for_task = embed_requests.clone();
    let chat_requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let chat_requests_for_task = chat_requests.clone();

    let join = tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };

            let mut buf = Vec::with_capacity(4096);
            let mut tmp = [0u8; 1024];
            // Two outputs from the read loop: the request body string
            // (for capture) and the URL kind (for dispatch). `None` on
            // either means "malformed / truncated — serve 503 and move
            // on" rather than panicking.
            let parsed: Option<(EndpointKind, String)> = loop {
                let n = match sock.read(&mut tmp).await {
                    Ok(n) => n,
                    Err(_) => break None,
                };
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(headers_end) = find_double_crlf(&buf) {
                    let header_str = match std::str::from_utf8(&buf[..headers_end]) {
                        Ok(s) => s,
                        Err(_) => break None,
                    };
                    let kind = parse_request_path(header_str)
                        .map(classify_endpoint)
                        .unwrap_or(EndpointKind::Chat);
                    let content_length = header_content_length(header_str).unwrap_or(0);
                    let body_start = headers_end + 4;
                    let total_needed = body_start + content_length;
                    if buf.len() >= total_needed {
                        match String::from_utf8(buf[body_start..total_needed].to_vec()) {
                            Ok(b) => break Some((kind, b)),
                            Err(_) => break None,
                        }
                    }
                }
                if buf.len() > MOCK_MAX_REQUEST_BYTES {
                    break None;
                }
            };

            // Capture into the per-endpoint list and dequeue the next
            // canned response from the matching queue. Each endpoint
            // has its own FIFO so an unexpected extra dial to one side
            // surfaces as a 503 on that side, not a body-shape error
            // on the other.
            let next: Option<String> = if let Some((kind, body)) = parsed {
                match kind {
                    EndpointKind::Embedding => {
                        embed_requests_for_task.lock().unwrap().push(body);
                        let mut q = embed_queue_for_task.lock().unwrap();
                        if q.is_empty() { None } else { Some(q.remove(0)) }
                    }
                    EndpointKind::Chat => {
                        chat_requests_for_task.lock().unwrap().push(body);
                        let mut q = chat_queue_for_task.lock().unwrap();
                        if q.is_empty() { None } else { Some(q.remove(0)) }
                    }
                }
            } else {
                None
            };

            let resp = match next {
                Some(body) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                ),
                None => {
                    let empty = "{}";
                    format!(
                        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        empty.len(),
                        empty,
                    )
                }
            };

            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });

    ScriptedLlm {
        base_url,
        embed_requests,
        chat_requests,
        join: Some(join),
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    for i in 0..(buf.len() - 3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i);
        }
    }
    None
}

fn header_content_length(headers: &str) -> Option<usize> {
    for line in headers.lines() {
        let mut parts = line.splitn(2, ':');
        let Some(name) = parts.next() else { continue };
        let Some(value) = parts.next() else { continue };
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}

/// Wrap `plan_json` in an OpenAI-compatible chat-completion envelope.
pub fn envelope_for(plan_json_string: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": 1_700_000_000_u64,
        "model": "test-local-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": plan_json_string},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
    })
    .to_string()
}

/// Build an OpenAI-compatible embedding response envelope.
///
/// `PgRecallBuilder::build` calls `embed_query` (→ `router.embed`) once
/// per plan iteration, BEFORE the chat-completion call. The mock now
/// dispatches by URL path, so the embed queue holds one envelope per
/// expected embed dial and the chat queue holds one envelope per
/// expected plan-iteration — independent of call ordering.
///
/// `embed_query` Matryoshka-truncates the returned embedding to
/// `EMBEDDING_DIM` (256) elements; a vector at least that long succeeds
/// (this 768-long filler mirrors embeddinggemma's native width), while
/// a shorter one causes a `MemoryError::EmbeddingDimMismatch` that
/// triggers the degrade-and-warn path in `formulate_plan`. The byte
/// values don't matter for these tests: the `memories` table is never
/// seeded, so both recall lanes return 0 rows regardless of the query
/// vector. Using `0.001` (a small non-zero value) keeps the embedding
/// numerically well-defined for pgvector's cosine operator without
/// relying on any implementation-defined behaviour for the all-zeros
/// edge case.
pub fn embedding_envelope() -> String {
    let filler: Vec<f32> = vec![0.001f32; 768];
    serde_json::json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": filler}],
        "model": "test-local-model"
    })
    .to_string()
}

/// Convenience JSON builder for the plan body the planner emits as the
/// assistant message content.
pub fn plan_json(
    decision: &str,
    steps: serde_json::Value,
    result: Option<serde_json::Value>,
) -> String {
    let mut obj = serde_json::json!({
        "context":      "test context",
        "decision":     decision,
        "rationale":    "test rationale",
        "steps":        steps,
        "data_ceiling": "Public",
    });
    if let Some(r) = result {
        obj.as_object_mut().unwrap().insert("result".into(), r);
    } else {
        obj.as_object_mut()
            .unwrap()
            .insert("result".into(), serde_json::Value::Null);
    }
    obj.to_string()
}

// ---------------------------------------------------------------------------
// Unit tests for the URL-routing dispatcher helpers.
//
// The daemon e2e tests that consume this module skip on hosts without a
// supervisor / sandbox / Postgres toolchain, so the load-bearing classifier +
// path parser get their coverage from these in-module unit tests. Keep them
// here so the helpers and their pins live in one file.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod mock_router_unit_tests {
    use super::*;

    #[test]
    fn classify_endpoint_routes_embeddings_paths_to_embedding() {
        assert_eq!(classify_endpoint("/v1/embeddings"), EndpointKind::Embedding);
        assert_eq!(classify_endpoint("/embeddings"), EndpointKind::Embedding);
        // Query string / version drift defends against a future router
        // refactor that adds extra suffix bytes.
        assert_eq!(
            classify_endpoint("/v2/embeddings?stream=false"),
            EndpointKind::Embedding,
        );
    }

    #[test]
    fn classify_endpoint_defaults_unknown_paths_to_chat() {
        assert_eq!(
            classify_endpoint("/v1/chat/completions"),
            EndpointKind::Chat,
        );
        // No "embeddings" substring → falls through to Chat.
        assert_eq!(classify_endpoint("/v1/anything-else"), EndpointKind::Chat);
        assert_eq!(classify_endpoint("/"), EndpointKind::Chat);
    }

    #[test]
    fn parse_request_path_extracts_the_target_from_a_request_line() {
        let headers = "POST /v1/embeddings HTTP/1.1\r\nHost: localhost\r\n";
        assert_eq!(parse_request_path(headers), Some("/v1/embeddings"));
    }

    #[test]
    fn parse_request_path_handles_chat_completions_target() {
        let headers = "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\n";
        assert_eq!(parse_request_path(headers), Some("/v1/chat/completions"));
    }

    #[test]
    fn parse_request_path_returns_none_for_malformed_input() {
        // Single-token request line — no path field.
        assert_eq!(parse_request_path("GET"), None);
        // Empty input.
        assert_eq!(parse_request_path(""), None);
    }
}
