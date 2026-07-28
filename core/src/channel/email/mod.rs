//! Email fallback channel (Phase 2, slice #5). Inbound only in this slice.
//!
//! Design: `docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`.
//!
//! Email cannot authenticate its own senders the way Matrix can (E2E +
//! homeserver auth), so this module supplies the evidence the bus needs to
//! decide: a DMARC verdict from our own MX, and a per-pairing shared token.
//! Both are computed by pure functions in [`gate`] — in core, not in the
//! worker, so every rejection still lands in `audit_log`.

pub mod gate;
