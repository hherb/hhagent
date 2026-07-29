//! email-in: polls a localmail subscription and surfaces new messages as
//! channel events.
//!
//! ## Read this before touching anything in this crate
//!
//! **This worker makes no security decisions.** It does not check DMARC, it
//! does not check the per-pairing token, it does not decide whether a sender
//! is allowed to talk to the agent. It fetches messages from localmail and
//! hands them, unmodified, to core as raw material. The actual gate lives in
//! `core/src/channel/email/gate.rs` and is judged by the channel bus, which
//! is the one place a rejection gets written to `audit_log`. If this worker
//! judged anything itself, a rejected message could simply vanish with no
//! record — that is the exact failure mode the split is designed to prevent.
//!
//! Concretely: `email.poll` returns every `Authentication-Results` header
//! verbatim, in the order the mail server wrote them, and never looks at
//! their content. Do not add a check here, however small or well-intentioned
//! — put it in `core/src/channel/email/gate.rs` instead, where it will be
//! covered by the audit trail and the existing negative-control tests.
//!
//! Design: `docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`

mod client;
mod handler;

use kastellan_worker_prelude::serve_stdio;

fn main() -> anyhow::Result<()> {
    let mut handler = handler::EmailInHandler::from_env()?;
    serve_stdio(&mut handler)?;
    Ok(())
}
