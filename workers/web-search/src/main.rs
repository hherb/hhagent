//! web-search: query an operator-configured SearxNG instance and return ranked
//! structured hits over JSON-RPC stdio. GET-only; the LLM supplies only the
//! query string. Design:
//! docs/superpowers/specs/2026-06-09-web-search-worker-design.md

mod batch;
mod handler;

use kastellan_worker_prelude::serve_stdio_with;

fn main() -> anyhow::Result<()> {
    // Lock down FIRST, then construct (security audit 2026-09-02, F4): the
    // handler's `from_env` builds the egress-proxy CONNECT client's tokio
    // runtime (or a `reqwest::blocking` runtime thread in legacy mode), and
    // Landlock covers only threads created *after* `restrict_self` — the
    // network-facing threads were the ones without it.
    serve_stdio_with(handler::WebSearchHandler::from_env)?;
    Ok(())
}
