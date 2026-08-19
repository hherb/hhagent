//! The operator-ask path — #564 slice 1b, extended by slice 2.
//!
//! Split by *nature*, not by feature: [`pure`] holds the sync decision
//! rules and codecs (unit-tested); [`lifecycle`] holds everything that
//! needs a `PgPool` (e2e-tested). Slice 2 adds [`delivery`] alongside them.
//!
//! Specs: `docs/superpowers/specs/2026-08-18-ask-path-slice-1b-design.md`
//! and `docs/superpowers/specs/2026-08-19-ask-channel-slice-2-design.md`.

pub mod lifecycle;
pub mod pure;

// Re-exported flat so every existing `scheduler::asks::<item>` path keeps
// resolving. Listed explicitly rather than with a glob so the module's
// public surface stays visible in one place.
pub use lifecycle::{raise_and_suspend, sweep_expired_and_audit};
pub use pure::{
    ask_deadline_seconds, decide, deadline_from_env, resolution_choice, resume_state_from,
    restore_resume_state, AskDecision, Choice, RestoredRun, ASK_DEADLINE_ENV,
    ASK_KIND_PLAN_APPROVAL, DEFAULT_ASK_DEADLINE_S,
};

pub(super) use lifecycle::emit_approval_applied;
