//! Test fixture (NOT a production binary): a fake email-in worker that speaks
//! the real `email.init` / `email.poll` / `email.ack` JSON-RPC surface over
//! stdio so `core/tests/email_channel_e2e.rs` can exercise the full
//! EmailChannel → ChannelBus loop against a real worker process — with no
//! localmail, no network, no sandbox. Modelled on
//! `workers/matrix/examples/fake_matrix_worker.rs`.
//!
//! Behaviour (env-configured):
//! - `email.poll` serves the canned `{"events": […], "skipped": […]}` result
//!   read from `KASTELLAN_FAKE_EMAIL_POLL_RESULT` (the exact shape
//!   `email-in`'s real `email.poll` returns — see
//!   `workers/email-in/src/handler.rs::build_event`) exactly ONCE, then an
//!   empty batch on every subsequent poll — so the driver's poll loop keeps
//!   running without redelivering;
//! - `email.ack` appends every acked cursor to `KASTELLAN_FAKE_EMAIL_ACK_LOG`
//!   (one per line) so the test can assert on acks, including acks of
//!   `skipped` ids that never became an event.

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

use kastellan_protocol::{codes, server::serve_stdio, server::Handler, RpcError};
use serde_json::Value;

/// Empty poll result shape (`events` + `skipped` both absent-safe as empty
/// arrays), served on every poll after the canned batch and as the fallback
/// when `KASTELLAN_FAKE_EMAIL_POLL_RESULT` is unset or unparsable.
fn empty_poll_result() -> Value {
    serde_json::json!({ "events": [], "skipped": [] })
}

struct FakeEmailWorker {
    /// The canned first-poll result, exactly as `KASTELLAN_FAKE_EMAIL_POLL_RESULT`
    /// supplied it (the test builds this as `{"events": […], "skipped": […]}`).
    poll_result: Value,
    /// Flips true after the canned batch is served once.
    served: AtomicBool,
    ack_log: Option<String>,
}

impl Handler for FakeEmailWorker {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "email.init" => Ok(serde_json::json!({
                "address": "kastellan@example.org",
                "subscription": "test",
            })),
            "email.poll" => {
                if self.served.swap(true, Ordering::SeqCst) {
                    Ok(empty_poll_result())
                } else {
                    Ok(self.poll_result.clone())
                }
            }
            "email.ack" => {
                if let (Some(path), Some(cursor)) =
                    (self.ack_log.as_deref(), params.get("cursor").and_then(Value::as_str))
                {
                    if let Ok(mut f) =
                        std::fs::OpenOptions::new().create(true).append(true).open(path)
                    {
                        let _ = writeln!(f, "{cursor}");
                    }
                }
                Ok(serde_json::json!({ "ok": true }))
            }
            other => Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method {other}"),
            )),
        }
    }
}

fn main() -> std::io::Result<()> {
    let poll_result = std::env::var("KASTELLAN_FAKE_EMAIL_POLL_RESULT")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(empty_poll_result);
    let mut h = FakeEmailWorker {
        poll_result,
        served: AtomicBool::new(false),
        ack_log: std::env::var("KASTELLAN_FAKE_EMAIL_ACK_LOG").ok(),
    };
    serve_stdio(&mut h)
}
