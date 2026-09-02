//! web-research: one-call web research — SearxNG search, fetch the top-N
//! allowlisted result pages, extract readable text, and return the passages
//! most relevant to the query over JSON-RPC stdio. GET-only; the LLM supplies
//! only the query string. Design:
//! docs/superpowers/specs/2026-07-07-web-research-composite-worker-design.md

mod chunk;
mod embed;
mod handler;
mod rank;
mod research;

use kastellan_worker_prelude::serve_stdio_with;

fn main() -> anyhow::Result<()> {
    // Lock down FIRST, then construct (security audit 2026-09-02, F4): the
    // handler's `from_env` builds the egress-proxy CONNECT client's tokio
    // runtime (or a `reqwest::blocking` runtime thread in legacy mode), and
    // Landlock covers only threads created *after* `restrict_self` — the
    // network-facing threads were the ones without it.
    serve_stdio_with(handler::WebResearchHandler::from_env)?;
    Ok(())
}
