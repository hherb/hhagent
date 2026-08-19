//! The async half of the operator-ask path: raising an ask (which suspends
//! its task), sweeping overdue asks, and the audit rows both emit.
//!
//! Everything here needs a live `PgPool`, so its coverage is the
//! `scheduler_ask*_e2e` integration suites rather than unit tests. The
//! decision rules it applies are in [`super::pure`], which is unit-tested.
//!
//! Spec: `docs/superpowers/specs/2026-08-18-ask-path-slice-1b-design.md`.

use sqlx::PgPool;
use time::{Duration, OffsetDateTime};

use kastellan_db::asks as db_asks;
use kastellan_db::DbError;

use crate::cassandra::plan_digest::plan_digest;
use crate::cassandra::types::{Plan, Severity};

use super::pure::{deadline_from_env, ASK_KIND_PLAN_APPROVAL};
use crate::scheduler::audit::{
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
///
/// `resume_state` is the suspended run's history, from
/// [`resume_state_from`], stored on the ask so the resumed task does not
/// re-formulate — and re-execute — iterations it already ran (#564 slice
/// 1b, D11). `None` means "no history to carry", which is what a run that
/// escalated on its very first plan honestly has.
pub async fn raise_and_suspend(
    pool: &PgPool,
    task_id: i64,
    plan: &Plan,
    concern: &str,
    severity: Severity,
    resume_state: Option<&serde_json::Value>,
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
        resume_state,
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
pub(crate) async fn emit_approval_applied(
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
