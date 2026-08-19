//! The operator-ask path — #564 slice 1b.
//!
//! Slice 1a built the durable record (`db::asks`) and nothing called it.
//! This module is the caller: it turns a `Verdict::Escalate` into a raised
//! ask, reads a resolved ask back into a decision, and sweeps overdue asks.
//!
//! The reading half is **pure** and lives here rather than inside the inner
//! loop, because the `Escalate` arm is otherwise reachable only through a
//! Postgres e2e — and a rule with no unit test is a rule nobody can
//! exercise cheaply.
//!
//! Spec: `docs/superpowers/specs/2026-08-18-ask-path-slice-1b-design.md`.

use kastellan_db::asks::Ask;

/// The answers a `plan_approval` ask offers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Choice {
    Approve,
    Deny,
}

/// What a resolved ask means for one specific plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AskDecision {
    /// The operator approved, and this is the plan they approved.
    Approved,
    /// The operator denied. Terminal for the task, whatever the plan.
    Denied,
    /// There is no usable decision for *this* plan — a different plan was
    /// approved, the ask binds to no plan, or the resolution is malformed.
    /// The safe arm: it leads to raising a fresh ask, never to proceeding.
    NotForThisPlan,
}

/// `asks.kind` for an escalated-plan approval. The column's CHECK accepts
/// only this value today; a new kind is a deliberate migration.
pub const ASK_KIND_PLAN_APPROVAL: &str = "plan_approval";

/// Operator override for how long an ask stays answerable.
pub const ASK_DEADLINE_ENV: &str = "KASTELLAN_ASK_DEADLINE_S";

/// 24 hours. Long enough that an escalation raised overnight is still
/// answerable in the morning, short enough that an abandoned task does not
/// sit suspended for a week.
pub const DEFAULT_ASK_DEADLINE_S: i64 = 86_400;

/// The ask's resolution as a validated [`Choice`], or `None` if there is
/// no usable one.
///
/// **Validated against the ask's own `options`, not against a constant.**
/// The closed set is per-ask: a future kind offers different options, and
/// checking against a hardcoded `{approve, deny}` would silently accept the
/// wrong vocabulary for it.
///
/// `db::asks::resolve` already enforces the closed set on the way in, so
/// every arm returning `None` here is defence in depth — against a
/// direct-SQL writer, or a resolver added later that forgets. They must
/// return `None` and never `Approve`: `None` leads to raising a fresh ask,
/// which costs one operator interaction, while a wrong `Approve` lets a
/// plan run that nobody approved.
pub fn resolution_choice(ask: &Ask) -> Option<Choice> {
    let choice = ask.resolution.as_ref()?.get("choice")?.as_str()?;
    let offered = ask.options.as_array()?;
    if !offered.iter().any(|o| o.as_str() == Some(choice)) {
        return None;
    }
    match choice {
        "approve" => Some(Choice::Approve),
        "deny" => Some(Choice::Deny),
        _ => None,
    }
}

/// What `ask` decides about the plan whose digest is `plan_digest`.
///
/// An approval binds to a plan digest (slice 1a, spec D1): the resumed task
/// replans from scratch and goes through review again, and the approval
/// carries only if the new plan is the one that was approved. A denial
/// binds to the task, not the plan — see spec D2 for why the asymmetry is
/// deliberate.
///
/// An approval whose ask carries no `plan_digest` approves nothing. The
/// column is nullable for kinds that do not bind to a plan, and defaulting
/// such an approval to "covers this plan" is the fail-open direction.
pub fn decide(ask: &Ask, plan_digest: &str) -> AskDecision {
    match resolution_choice(ask) {
        Some(Choice::Deny) => AskDecision::Denied,
        Some(Choice::Approve) if ask.plan_digest.as_deref() == Some(plan_digest) => {
            AskDecision::Approved
        }
        _ => AskDecision::NotForThisPlan,
    }
}

/// Parse an ask deadline in seconds, falling back to
/// [`DEFAULT_ASK_DEADLINE_S`] for anything unusable.
///
/// Pure, taking the raw string, so the fallback rules have a test that does
/// not mutate process environment (which is global and races Rust's
/// parallel test threads — the `KASTELLAN_WORKER_OUT` flake).
///
/// Non-positive values fall back rather than being honoured: a zero or
/// negative deadline is already in the past when `raise` runs, so the
/// `asks_deadline_after_created` CHECK rejects the INSERT and the
/// escalation fails instead of asking the human — an operator typo turning
/// into a task failure.
pub fn ask_deadline_seconds(raw: Option<&str>) -> i64 {
    raw.and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_ASK_DEADLINE_S)
}

/// [`ask_deadline_seconds`] against the live environment.
pub fn deadline_from_env() -> i64 {
    ask_deadline_seconds(std::env::var(ASK_DEADLINE_ENV).ok().as_deref())
}

// ---------------------------------------------------------------------------
// Async half: raising an ask (and suspending its task), and sweeping asks
// past their deadline. Both are thin wiring over `db::asks` plus the
// best-effort audit rows this slice adds.
// ---------------------------------------------------------------------------

use sqlx::PgPool;
use time::{Duration, OffsetDateTime};

use kastellan_db::asks as db_asks;
use kastellan_db::DbError;

use crate::cassandra::plan_digest::plan_digest;
use crate::cassandra::types::{Plan, Severity};

use super::audit::{
    action_task_terminal, build_ask_expiry_finalize_payload, build_lifecycle_payload,
    ACTION_ASK_APPROVAL_APPLIED, ACTION_ASK_EXPIRED, ACTION_ASK_RAISED, ACTION_TASK_FINALIZE,
    SCHEDULER_AUDIT_ACTOR,
};

/// Raise a `plan_approval` ask for an escalated plan and suspend its task.
///
/// Returns the new ask's id. The task is `awaiting_operator` on success —
/// `db::asks::raise` writes the ask INSERT and the task UPDATE in one
/// transaction, so there is no window where either exists without the
/// other.
///
/// **The plaintext nonce is dropped unread, and that is correct for this
/// slice** (spec D5). Slice 1b's only answer surface is `kastellan-cli
/// inbox`, which resolves by row id — the path `db::asks::resolve` reserves
/// for a trusted local caller. Logging the nonce would put a live approval
/// token into `~/.local/state/kastellan/*.out`, a plaintext file with none
/// of `audit_log`'s role gating. Slice 2 delivers it over Matrix at raise
/// time; it never needs to recover one raised earlier.
///
/// `plan` is digested as passed — i.e. *after* `apply_floor_raise`,
/// `data_ceiling` resolution, invoke expansion and namespace completion.
/// That is deliberate: the digest must cover what would execute, and the
/// same normalisations run again on the replan, so the two digests are
/// comparable.
pub async fn raise_and_suspend(
    pool: &PgPool,
    task_id: i64,
    plan: &Plan,
    concern: &str,
    severity: Severity,
) -> Result<i64, DbError> {
    let digest = plan_digest(plan);
    let deadline_at = OffsetDateTime::now_utc() + Duration::seconds(deadline_from_env());

    let raised = db_asks::raise(
        pool,
        task_id,
        ASK_KIND_PLAN_APPROVAL,
        concern,
        &serde_json::json!(["approve", "deny"]),
        Some(&digest),
        deadline_at,
    )
    .await?;

    // Destructured rather than field-accessed so the nonce's drop (and its
    // zeroize) is visible at this call site instead of implied.
    let db_asks::RaisedAsk { ask_id, nonce } = raised;
    drop(nonce);

    emit_ask_raised(pool, ask_id, task_id, &digest, severity, deadline_at).await;
    Ok(ask_id)
}

/// Best-effort `ask.raised` row. Same posture as
/// `runner::audit_rows::write_lifecycle_row`: the ask and the suspension
/// have already committed, and a transient `audit_log` failure must not
/// undo them.
async fn emit_ask_raised(
    pool: &PgPool,
    ask_id: i64,
    task_id: i64,
    plan_digest: &str,
    severity: Severity,
    deadline_at: OffsetDateTime,
) {
    let payload = serde_json::json!({
        "ask_id": ask_id,
        "task_id": task_id,
        "kind": ASK_KIND_PLAN_APPROVAL,
        "plan_digest": plan_digest,
        "severity": severity,
        "deadline_at": deadline_at.to_string(),
    });
    if let Err(e) =
        kastellan_db::audit::insert(pool, SCHEDULER_AUDIT_ACTOR, ACTION_ASK_RAISED, payload).await
    {
        tracing::warn!(
            ask_id, task_id, error = %e,
            "audit insert for scheduler/ask.raised failed (best-effort)"
        );
    }
}

/// Best-effort `ask.approval_applied` row, written when an escalated plan
/// turned out to be one the operator had already approved and the loop
/// proceeded instead of asking again.
///
/// Same posture as [`emit_ask_raised`]: the decision to proceed has already
/// been made by the time this is called, and a transient `audit_log`
/// failure must not change it. See [`ACTION_ASK_APPROVAL_APPLIED`] for why
/// the row exists at all.
pub(super) async fn emit_approval_applied(
    pool: &PgPool,
    ask_id: i64,
    task_id: i64,
    plan_digest: &str,
) {
    let payload = serde_json::json!({
        "ask_id": ask_id,
        "task_id": task_id,
        "plan_digest": plan_digest,
    });
    if let Err(e) = kastellan_db::audit::insert(
        pool,
        SCHEDULER_AUDIT_ACTOR,
        ACTION_ASK_APPROVAL_APPLIED,
        payload,
    )
    .await
    {
        tracing::warn!(
            ask_id, task_id, error = %e,
            "audit insert for scheduler/ask.approval_applied failed (best-effort)"
        );
    }
}

/// Expire every overdue ask and emit its audit rows. Returns how many were
/// retired.
///
/// Mirrors [`super::crash_recovery::sweep_and_audit`] exactly: the DB sweep
/// is fail-closed (its error propagates) and the audit inserts are
/// best-effort.
///
/// **Three rows per expired ask, not one.** `ask.expired` records the ask
/// side, but `db::asks::expire_due` also moves the *task*
/// `awaiting_operator → failed`, and observation-phase SQL pivots on the
/// audit log — a bare `tasks.state` UPDATE is invisible to it. So this also
/// writes the `task.failed` lifecycle row and the `task.finalize` summary
/// row, exactly as `crash_recovery` does for the same reason: without them
/// any query grouping on `task.finalize` silently drops every task that
/// timed out waiting for a human.
///
/// `db::asks::expire_due` returns only the rows *it* moved, so a concurrent
/// second sweep cannot make two callers each emit a row for the same ask,
/// and re-running finds nothing.
pub async fn sweep_expired_and_audit(pool: &PgPool) -> Result<usize, DbError> {
    let expired = db_asks::expire_due(pool).await?;
    for e in &expired {
        let payload = serde_json::json!({"ask_id": e.ask_id, "task_id": e.task_id});
        if let Err(err) =
            kastellan_db::audit::insert(pool, SCHEDULER_AUDIT_ACTOR, ACTION_ASK_EXPIRED, payload)
                .await
        {
            tracing::warn!(
                ask_id = e.ask_id, task_id = e.task_id, error = %err,
                "audit insert for scheduler/ask.expired failed (best-effort)"
            );
        }
        emit_expired_task_rows(pool, e).await;
    }
    if !expired.is_empty() {
        tracing::warn!(count = expired.len(), "expired overdue operator asks; their tasks failed closed");
    }
    Ok(expired.len())
}

/// The two task-lifecycle rows for one expired ask: `task.failed` and
/// `task.finalize`. Best-effort throughout — the task has already been
/// failed in its own transaction and nothing here may undo that.
///
/// Re-reads the task rather than deriving the payload from the
/// [`db_asks::ExpiredAsk`], which carries only the two ids: `lane`,
/// `plan_count` and `started_at` are all real facts about the task and the
/// audit row states them rather than guessing. If the read fails or finds
/// nothing, no row is written — a missing row is recoverable, an invented
/// one is not.
async fn emit_expired_task_rows(pool: &PgPool, expired: &db_asks::ExpiredAsk) {
    let task = match kastellan_db::tasks::get(pool, expired.task_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            tracing::warn!(
                ask_id = expired.ask_id, task_id = expired.task_id,
                "expired ask names a task that no longer reads back; skipping its \
                 task.failed / task.finalize rows rather than inventing their contents"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                ask_id = expired.ask_id, task_id = expired.task_id, error = %e,
                "could not re-read the task an expired ask failed; skipping its \
                 task.failed / task.finalize rows (best-effort)"
            );
            return;
        }
    };

    let action = action_task_terminal("failed");
    let payload = build_lifecycle_payload(task.id, task.lane, task.plan_count);
    if let Err(e) =
        kastellan_db::audit::insert(pool, SCHEDULER_AUDIT_ACTOR, &action, payload).await
    {
        tracing::warn!(
            task_id = task.id, error = %e,
            "audit insert for scheduler/task.failed (ask expiry) failed (best-effort)"
        );
    }

    // `expire_due` sets `finished_at = now()` in the same UPDATE that
    // fails the task, so this is `Some` in practice. The fallback is loud
    // rather than silent for the same reason `crash_recovery` makes its
    // one loud: a row carrying the emitter's wall clock is off by the
    // sweep-lag delta and an operator reading it needs to know.
    let finished_at = task.finished_at.unwrap_or_else(|| {
        tracing::error!(
            task_id = task.id,
            "scheduler::asks::emit_expired_task_rows: task.finished_at is None after \
             expire_due — expected its unconditional `finished_at = now()`; falling back \
             to the local clock so the audit row still emits",
        );
        OffsetDateTime::now_utc()
    });
    let payload = build_ask_expiry_finalize_payload(
        task.id,
        task.lane,
        task.plan_count,
        task.started_at,
        finished_at,
    );
    if let Err(e) =
        kastellan_db::audit::insert(pool, SCHEDULER_AUDIT_ACTOR, ACTION_TASK_FINALIZE, payload)
            .await
    {
        tracing::warn!(
            task_id = task.id, error = %e,
            "audit insert for scheduler/task.finalize (ask expiry) failed (best-effort)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_db::asks::Ask;
    use time::OffsetDateTime;

    /// A resolved `Ask` fixture. Every field is set explicitly so a new
    /// column added to `Ask` is a compile error here rather than a silent
    /// default the tests stop covering.
    fn ask(resolution: Option<serde_json::Value>, plan_digest: Option<&str>) -> Ask {
        Ask {
            id: 42,
            task_id: 7,
            kind: ASK_KIND_PLAN_APPROVAL.to_string(),
            body: "sends mail to a stranger".to_string(),
            options: serde_json::json!(["approve", "deny"]),
            plan_digest: plan_digest.map(String::from),
            state: "resolved".to_string(),
            created_at: OffsetDateTime::now_utc(),
            deadline_at: OffsetDateTime::now_utc(),
            resolved_at: Some(OffsetDateTime::now_utc()),
            resolved_by: Some("operator".to_string()),
            resolution,
        }
    }

    #[test]
    fn resolution_choice_reads_the_two_offered_answers() {
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!({"choice": "approve"})), Some("d"))),
            Some(Choice::Approve)
        );
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!({"choice": "deny"})), Some("d"))),
            Some(Choice::Deny)
        );
    }

    #[test]
    fn resolution_choice_is_none_for_every_malformed_shape() {
        // `asks::resolve` enforces the closed set on the way in, so these
        // arms are defence in depth against a direct-SQL writer or a future
        // resolver. They must answer None — never Approve — because None is
        // the arm that leads to raising a fresh ask.
        assert_eq!(resolution_choice(&ask(None, Some("d"))), None, "no resolution");
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!("approve")), Some("d"))),
            None,
            "resolution is not an object"
        );
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!({"note": "ok"})), Some("d"))),
            None,
            "no choice key"
        );
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!({"choice": 1})), Some("d"))),
            None,
            "choice is not a string"
        );
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!({"choice": "maybe"})), Some("d"))),
            None,
            "choice is not one this ask offered"
        );
    }

    #[test]
    fn a_choice_outside_this_asks_own_options_is_refused() {
        // The closed set is per-ask, not a global {approve,deny}: a future
        // kind offers different options, and validating against a constant
        // would silently accept the wrong vocabulary.
        let mut a = ask(Some(serde_json::json!({"choice": "approve"})), Some("d"));
        a.options = serde_json::json!(["yes", "no"]);
        assert_eq!(resolution_choice(&a), None);
    }

    #[test]
    fn an_offered_choice_this_module_does_not_understand_is_refused() {
        // "defer" is one of this ask's own `options`, so it clears the
        // `offered` guard and reaches the final match arm — unlike
        // "maybe" above, which never offered and is rejected earlier.
        // That arm must land on `None`, never on an unpinned `Approve`,
        // since `options` is free-form JSONB and a future ask kind is
        // exactly the case that would offer a third choice string.
        let mut a = ask(Some(serde_json::json!({"choice": "defer"})), Some("digest-a"));
        a.options = serde_json::json!(["approve", "deny", "defer"]);
        assert_eq!(resolution_choice(&a), None);
        assert_eq!(decide(&a, "digest-a"), AskDecision::NotForThisPlan);
    }

    #[test]
    fn decide_approves_only_the_digest_that_was_approved() {
        let a = ask(Some(serde_json::json!({"choice": "approve"})), Some("digest-a"));
        assert_eq!(decide(&a, "digest-a"), AskDecision::Approved);
        assert_eq!(decide(&a, "digest-b"), AskDecision::NotForThisPlan);
    }

    #[test]
    fn decide_denies_regardless_of_digest() {
        let a = ask(Some(serde_json::json!({"choice": "deny"})), Some("digest-a"));
        assert_eq!(decide(&a, "digest-a"), AskDecision::Denied);
        assert_eq!(decide(&a, "digest-b"), AskDecision::Denied);
    }

    #[test]
    fn an_approval_bound_to_no_digest_approves_nothing() {
        // plan_digest is nullable because a future kind need not bind to a
        // plan. Such an approval must never cover a plan by default.
        let a = ask(Some(serde_json::json!({"choice": "approve"})), None);
        assert_eq!(decide(&a, "digest-a"), AskDecision::NotForThisPlan);
    }

    #[test]
    fn a_malformed_resolution_never_reaches_the_approved_arm() {
        let a = ask(Some(serde_json::json!({"choice": "maybe"})), Some("digest-a"));
        assert_eq!(decide(&a, "digest-a"), AskDecision::NotForThisPlan);
    }

    #[test]
    fn ask_deadline_seconds_defaults_and_rejects_unusable_values() {
        assert_eq!(ask_deadline_seconds(None), DEFAULT_ASK_DEADLINE_S);
        assert_eq!(ask_deadline_seconds(Some("3600")), 3600);
        assert_eq!(ask_deadline_seconds(Some("  3600  ")), 3600);
        // A non-positive deadline would be in the past by the time `raise`
        // runs and the asks_deadline_after_created CHECK would reject the
        // INSERT, failing the escalation instead of asking the human.
        assert_eq!(ask_deadline_seconds(Some("0")), DEFAULT_ASK_DEADLINE_S);
        assert_eq!(ask_deadline_seconds(Some("-1")), DEFAULT_ASK_DEADLINE_S);
        assert_eq!(ask_deadline_seconds(Some("later")), DEFAULT_ASK_DEADLINE_S);
        assert_eq!(ask_deadline_seconds(Some("")), DEFAULT_ASK_DEADLINE_S);
    }
}
