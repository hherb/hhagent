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
//! verbatim, in the best order this worker can establish, and never looks at
//! their content. Do not add a check here, however small or well-intentioned
//! — put it in `core/src/channel/email/gate.rs` instead, where it will be
//! covered by the audit trail and the existing negative-control tests.
//!
//! ## `auth_results_order_known` — read this before touching header ordering
//!
//! Each event also carries `auth_results_order_known: bool`. This is NOT a
//! verdict — it is a signal that core's gate needs to make its own verdict
//! safely. localmail groups every wire occurrence of one EXACT-cased header
//! spelling into a single ordered JSON array; that is the realistic case
//! (a two-milter Postfix emitting two headers with the identical literal
//! name) and its order is trustworthy. But "the same" header spelled with a
//! DIFFERENT case lands in a second, separate JSON object key, and this
//! workspace's `serde_json` has no `preserve_order` feature — so iterating
//! multiple such keys visits them in byte/alphabetical order, not wire
//! order. That is a real, remotely-triggerable gate bypass if left
//! unsignalled: an attacker-forged `AUTHENTICATION-RESULTS` header sorts
//! before a genuine `Authentication-Results` one, so it would silently win
//! element 0 — exactly the element core's `trusted_dmarc_pass` consults.
//! `auth_results_order_known` is `false` whenever 2+ distinct-cased spellings
//! are present, so core can fail closed instead of trusting that ordering.
//! This worker still returns every header value it found either way — see
//! `handler::build_event`'s doc for the full mechanics, and
//! `task-7-report.md` for the confirmation trail and review discussion.
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
