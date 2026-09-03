//! web-fetch: fetch a URL (HTTPS-only, against a host allowlist) and return
//! extracted readable text over JSON-RPC stdio. GET-only; no caller-supplied
//! headers/body. Design:
//! docs/superpowers/specs/2026-06-08-web-fetch-worker-design.md

mod handler;

use kastellan_worker_prelude::serve_stdio_with;

fn main() -> anyhow::Result<()> {
    // Lock down FIRST, then construct (security audit 2026-09-02, F4): the
    // handler's `from_env` builds the egress-proxy CONNECT client's tokio
    // runtime (or a `reqwest::blocking` runtime thread in legacy mode), and
    // Landlock covers only threads created *after* `restrict_self` — the
    // network-facing threads were the ones without it.
    serve_stdio_with(handler::WebFetchHandler::from_env)?;
    Ok(())
}
