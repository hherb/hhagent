//! Typed CRUD against the `asks` table (#564 slice 1a).
//!
//! An **ask** is a durable, correlated, deadline-bounded question the
//! daemon raises for a human. It exists because CASSANDRA's
//! `Verdict::Escalate` — "a human must decide this" — had nowhere to go:
//! `channel/bus.rs` is strictly inbound-message → task → outbound-reply,
//! with no way for core to initiate a question and suspend a task on the
//! answer. See
//! `docs/superpowers/specs/2026-08-16-ask-record-slice-1a-design.md`.
//!
//! # Invariants this module maintains
//!
//! * **An ask and its task move together.** Every operation that changes
//!   an ask's state also moves its task, in ONE transaction. A resolved
//!   ask whose task never resumed is a wedged task; an expired ask whose
//!   task stayed suspended is the same bug.
//! * **`tasks` writes go through [`crate::tasks`].** That module owns all
//!   `tasks` SQL; this one calls its executor-generic helpers inside its
//!   own transactions rather than writing `UPDATE tasks` here.
//! * **Resolution happens exactly once.** [`resolve`] and
//!   [`resolve_with_nonce`] both guard on `state = 'pending'` and report
//!   rows-affected, so the first responder wins and every later one is
//!   told it lost — no lock, same idiom as `memories::set_embedding`.
//! * **An untrusted-transport caller resolves by nonce, never by id.**
//!   [`resolve`] is keyed by row id — a guessable small integer — and is
//!   safe only because its caller is the local operator CLI.
//!   [`resolve_with_nonce`] is the form a channel/transport caller must
//!   use; see its doc for why (spec D3).
//! * **The nonce is never readable.** Only its hash is stored, and
//!   [`Ask`] deliberately has no nonce field. [`resolve_with_nonce`]
//!   matches an inbound nonce with a `WHERE nonce_sha256 = $1` predicate,
//!   never by reading the stored value out and comparing in Rust.

use sha2::{Digest, Sha256};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};
use time::OffsetDateTime;

use crate::tasks;
use crate::DbError;

/// Entropy in an ask nonce. 32 bytes → 64 hex chars, the same width as
/// the SHA-256 it is stored under, and far past guessing range for a
/// token that gates someone else's approval.
const NONCE_BYTES: usize = 32;

/// One decoded `asks` row.
///
/// **No nonce field, deliberately.** The plaintext is returned once by
/// [`raise`] and never stored; the hash stays in SQL where the only thing
/// that needs it is a `WHERE` predicate. A struct field would invite a
/// Rust-side comparison, and that is how timing-unsafe token checks get
/// written.
#[derive(Clone, Debug)]
pub struct Ask {
    pub id: i64,
    pub task_id: i64,
    pub kind: String,
    pub body: String,
    pub options: serde_json::Value,
    pub plan_digest: Option<String>,
    pub state: String,
    pub created_at: OffsetDateTime,
    pub deadline_at: OffsetDateTime,
    pub resolved_at: Option<OffsetDateTime>,
    pub resolved_by: Option<String>,
    pub resolution: Option<serde_json::Value>,
}

/// What [`raise`] hands back: the row id, and the correlation nonce **in
/// plaintext, exactly once**. Nothing persists the plaintext — if the
/// caller drops it, the ask can never be resolved through a nonce-bearing
/// transport again and must be expired or cancelled.
///
/// [`Debug`] is hand-written to **redact `nonce`**: it is a live approval
/// token — the very thing [`Ask`] deliberately has no field for, one
/// screen above this struct — and nothing debug-formats it today, but the
/// whole point of a `Debug` impl is that someone eventually will. A
/// `tracing::debug!(?raised, …)` added in a later slice would otherwise
/// write the plaintext straight into `~/.local/state/kastellan/*.out`.
/// Mirrors `core::channel::PeerEvidence`, which redacts
/// `presented_token` for the identical reason.
#[derive(Clone)]
pub struct RaisedAsk {
    pub ask_id: i64,
    pub nonce: String,
}

impl std::fmt::Debug for RaisedAsk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RaisedAsk")
            .field("ask_id", &self.ask_id)
            .field("nonce", &"<redacted>")
            .finish()
    }
}

/// One row [`expire_due`] retired, for the caller's audit emission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpiredAsk {
    pub ask_id: i64,
    pub task_id: i64,
}

/// Lowercase hex SHA-256. Public because callers matching an inbound
/// nonce need to hash it the same way this module did.
pub fn sha256_hex(input: &str) -> String {
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        write!(s, "{b:02x}").expect("write to String cannot fail");
    }
    s
}

/// Mint a fresh correlation nonce as lowercase hex.
///
/// `OsRng` rather than `thread_rng`: this token is matched against an
/// untrusted inbound message and is the only thing standing between a
/// peer and someone else's approval, so it takes the OS CSPRNG directly
/// — the same choice `core::secrets::vault` makes for secret refs.
fn generate_nonce() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; NONCE_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut s = String::with_capacity(NONCE_BYTES * 2);
    for b in bytes {
        use std::fmt::Write;
        write!(s, "{b:02x}").expect("write to String cannot fail");
    }
    s
}

fn decode_ask_row(row: &PgRow) -> Result<Ask, DbError> {
    Ok(Ask {
        id: row.try_get("id")
            .map_err(|e| DbError::Query(format!("decode asks.id: {e}")))?,
        task_id: row.try_get("task_id")
            .map_err(|e| DbError::Query(format!("decode asks.task_id: {e}")))?,
        kind: row.try_get("kind")
            .map_err(|e| DbError::Query(format!("decode asks.kind: {e}")))?,
        body: row.try_get("body")
            .map_err(|e| DbError::Query(format!("decode asks.body: {e}")))?,
        options: row.try_get("options")
            .map_err(|e| DbError::Query(format!("decode asks.options: {e}")))?,
        plan_digest: row.try_get("plan_digest")
            .map_err(|e| DbError::Query(format!("decode asks.plan_digest: {e}")))?,
        state: row.try_get("state")
            .map_err(|e| DbError::Query(format!("decode asks.state: {e}")))?,
        created_at: row.try_get("created_at")
            .map_err(|e| DbError::Query(format!("decode asks.created_at: {e}")))?,
        deadline_at: row.try_get("deadline_at")
            .map_err(|e| DbError::Query(format!("decode asks.deadline_at: {e}")))?,
        resolved_at: row.try_get("resolved_at")
            .map_err(|e| DbError::Query(format!("decode asks.resolved_at: {e}")))?,
        resolved_by: row.try_get("resolved_by")
            .map_err(|e| DbError::Query(format!("decode asks.resolved_by: {e}")))?,
        resolution: row.try_get("resolution")
            .map_err(|e| DbError::Query(format!("decode asks.resolution: {e}")))?,
    })
}

const ASK_COLUMNS: &str = "id, task_id, kind, body, options, plan_digest, state, \
                           created_at, deadline_at, resolved_at, resolved_by, resolution";

/// Raise an ask against a **running** task and suspend that task.
///
/// One transaction: the task is suspended first (its `state = 'running'`
/// guard is what makes the whole operation conditional on the task being
/// ours to suspend), then the row is inserted. Ordering them the other way
/// would leave an orphan ask behind whenever the guard fails.
///
/// Errors — rather than returning an `Option` — when the task is not
/// `running`. There is no benign reading of that: the caller believed it
/// held a claimed task and did not, so a silent `None` would let a plan
/// proceed as though the human had been asked.
///
/// `plan_digest` is `Some` for kinds that bind to a plan and `None`
/// otherwise; see `core::cassandra::plan_digest` for what the value means.
pub async fn raise(
    pool: &PgPool,
    task_id: i64,
    kind: &str,
    body: &str,
    options: &serde_json::Value,
    plan_digest: Option<&str>,
    deadline_at: OffsetDateTime,
) -> Result<RaisedAsk, DbError> {
    let nonce = generate_nonce();
    let nonce_hash = sha256_hex(&nonce);

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| DbError::Query(format!("asks raise begin: {e}")))?;

    if !tasks::suspend_for_ask(&mut *tx, task_id).await? {
        // Dropping `tx` rolls back. Nothing was written.
        return Err(DbError::Other(format!(
            "asks raise: task {task_id} is not running, so it cannot be suspended \
             for an ask (already terminal, cancelled, or never claimed)"
        )));
    }

    let row = sqlx::query(
        "INSERT INTO asks (task_id, kind, body, options, plan_digest, nonce_sha256, deadline_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         RETURNING id",
    )
    .bind(task_id)
    .bind(kind)
    .bind(body)
    .bind(options)
    .bind(plan_digest)
    .bind(&nonce_hash)
    .bind(deadline_at)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| DbError::Query(format!("asks raise insert: {e}")))?;

    let ask_id: i64 = row
        .try_get("id")
        .map_err(|e| DbError::Query(format!("decode asks.id: {e}")))?;

    tx.commit()
        .await
        .map_err(|e| DbError::Query(format!("asks raise commit: {e}")))?;

    Ok(RaisedAsk { ask_id, nonce })
}

/// Resolve a pending ask found by row **id**, and return its task to the
/// queue.
///
/// **Operator-CLI path only.** An `id` is a small sequential integer with
/// no unforgeability property — anyone who can guess or enumerate it can
/// resolve someone else's ask. That is safe here only because this
/// function's caller is local and already trusted (the operator's own
/// `kastellan-cli`). **A caller reachable from an untrusted transport — a
/// channel/Matrix handler parsing `resolve 41 approve` out of a room
/// message, for instance — MUST use [`resolve_with_nonce`] instead.**
/// Resolving by id from such a caller is exactly the openworker weakness
/// spec D3 exists to avoid: it embeds a plain item id and is safe only
/// because *its* transport is a single-user desktop app, which a Matrix
/// room is not.
///
/// Returns `true` iff **this** call resolved it. `false` means the ask was
/// not `pending` — already resolved by someone else, expired, cancelled,
/// or absent — and nothing was written.
///
/// The guard is `WHERE id = $1 AND state = 'pending'` with rows-affected as
/// the answer: the same race-safe idiom `memories::set_embedding` uses.
/// That is what makes resolution exactly-once and first-responder-wins
/// across surfaces (a Matrix reply and a CLI resolve racing each other)
/// with no lock and no read-then-write window.
///
/// `resolution` is a closed set — `{"choice": …}` indexing into the ask's
/// `options`, optionally with `free_text` for the audit row. Free text is
/// stored and shown, never fed back into a plan.
pub async fn resolve(
    pool: &PgPool,
    ask_id: i64,
    resolved_by: &str,
    resolution: &serde_json::Value,
) -> Result<bool, DbError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| DbError::Query(format!("asks resolve begin: {e}")))?;

    let row = sqlx::query(
        "UPDATE asks \
         SET state = 'resolved', \
             resolved_at = now(), \
             resolved_by = $2, \
             resolution = $3 \
         WHERE id = $1 AND state = 'pending' \
         RETURNING id, task_id",
    )
    .bind(ask_id)
    .bind(resolved_by)
    .bind(resolution)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| DbError::Query(format!("asks resolve: {e}")))?;

    let Some(row) = row else {
        // Lost the race, or no such ask. Dropping `tx` rolls back; nothing
        // was written either way.
        return Ok(false);
    };
    let (resolved_ask_id, task_id) = decode_resolved_ids(&row)?;

    finish_resolve(tx, resolved_ask_id, task_id).await
}

/// Resolve a pending ask found by its correlation **nonce**, and return its
/// task to the queue.
///
/// **This is the path a channel/transport caller must use.** Callers pass
/// the plaintext nonce (whatever the peer presented); it is hashed here
/// with [`sha256_hex`] and matched against the stored `nonce_sha256` —
/// never the other way around, so a DB read still cannot recover a live
/// token. Guarded `WHERE nonce_sha256 = $1 AND state = 'pending'`, so a
/// peer who does not hold the nonce [`raise`] handed out cannot resolve
/// (or even discover) anyone else's ask. See [`resolve`]'s doc for why the
/// by-id form is not safe for this caller.
///
/// Same semantics as [`resolve`] otherwise: one transaction, exactly-once,
/// first-responder-wins. Returns `true` iff **this** call resolved it;
/// `false` for a wrong/unissued nonce, an already-resolved ask, or one that
/// expired/was cancelled.
pub async fn resolve_with_nonce(
    pool: &PgPool,
    nonce: &str,
    resolved_by: &str,
    resolution: &serde_json::Value,
) -> Result<bool, DbError> {
    let nonce_hash = sha256_hex(nonce);

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| DbError::Query(format!("asks resolve_with_nonce begin: {e}")))?;

    let row = sqlx::query(
        "UPDATE asks \
         SET state = 'resolved', \
             resolved_at = now(), \
             resolved_by = $2, \
             resolution = $3 \
         WHERE nonce_sha256 = $1 AND state = 'pending' \
         RETURNING id, task_id",
    )
    .bind(&nonce_hash)
    .bind(resolved_by)
    .bind(resolution)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| DbError::Query(format!("asks resolve_with_nonce: {e}")))?;

    let Some(row) = row else {
        // Wrong/unissued nonce, or lost the race. Dropping `tx` rolls back;
        // nothing was written either way.
        return Ok(false);
    };
    let (resolved_ask_id, task_id) = decode_resolved_ids(&row)?;

    finish_resolve(tx, resolved_ask_id, task_id).await
}

/// Decode the `(id, task_id)` pair both resolvers' guarded UPDATE returns.
fn decode_resolved_ids(row: &PgRow) -> Result<(i64, i64), DbError> {
    let ask_id: i64 = row
        .try_get("id")
        .map_err(|e| DbError::Query(format!("decode asks.id: {e}")))?;
    let task_id: i64 = row
        .try_get("task_id")
        .map_err(|e| DbError::Query(format!("decode asks.task_id: {e}")))?;
    Ok((ask_id, task_id))
}

/// Shared tail of [`resolve`] and [`resolve_with_nonce`]: given a
/// transaction whose guarded `UPDATE asks … WHERE state = 'pending'` has
/// already found and locked exactly one row, resume that row's task and
/// commit. Both callers differ only in the `WHERE` predicate (and the bind
/// type it takes) — everything after "we found the row" is identical, and
/// lives here exactly once so the two resolvers cannot drift apart.
async fn finish_resolve(
    mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
    ask_id: i64,
    task_id: i64,
) -> Result<bool, DbError> {
    if !tasks::resume_from_ask(&mut *tx, task_id).await? {
        // A pending ask whose task is NOT awaiting_operator is an invariant
        // violation: `cancel_for_task` cancels the ask with the task, and
        // `expire_due` expires it with the timeout, so no other path should
        // be able to separate them. Fail closed and loudly — the rollback
        // leaves the ask `pending`, which is recoverable, where committing
        // would leave it resolved with no task to resume.
        return Err(DbError::Other(format!(
            "asks resolve: ask {ask_id} was pending but task {task_id} is not \
             awaiting_operator — refusing to resolve an ask whose task cannot resume"
        )));
    }

    tx.commit()
        .await
        .map_err(|e| DbError::Query(format!("asks resolve commit: {e}")))?;
    Ok(true)
}

/// The detail string a task's result carries when its ask timed out.
/// A named const because slice 1b's audit rows and any operator query
/// will both want to match on it, and two spellings would silently
/// partition the same population.
pub const ASK_TIMEOUT_DETAIL: &str = "ask_timeout";

/// Expire every ask past its deadline and fail its task closed.
///
/// A headless daemon cannot leave a question pending forever the way a
/// desktop app can — there is no window a human will eventually look at.
/// Without this, a raised ask nobody answers is a permanently wedged task.
///
/// One transaction over the whole sweep: a partially-applied sweep would
/// leave asks expired with their tasks still suspended, which is exactly
/// the wedge this exists to prevent.
///
/// Idempotent — the `state = 'pending'` guard means a second call finds
/// nothing. Returns the retired rows so the caller can emit one audit row
/// each; an empty vec is the normal case.
pub async fn expire_due(pool: &PgPool) -> Result<Vec<ExpiredAsk>, DbError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| DbError::Query(format!("asks expire_due begin: {e}")))?;

    let rows = sqlx::query(
        "UPDATE asks \
         SET state = 'expired' \
         WHERE state = 'pending' AND deadline_at < now() \
         RETURNING id, task_id",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| DbError::Query(format!("asks expire_due: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let ask_id: i64 = row
            .try_get("id")
            .map_err(|e| DbError::Query(format!("decode asks.id: {e}")))?;
        let task_id: i64 = row
            .try_get("task_id")
            .map_err(|e| DbError::Query(format!("decode asks.task_id: {e}")))?;

        // Best-effort on the task side by design: if the task already left
        // `awaiting_operator` (cancelled in the same instant, say) the ask
        // still expires. What must not happen is the reverse — an expired
        // ask with a still-suspended task — and that is what the shared
        // transaction guarantees.
        tasks::fail_awaiting_operator(&mut *tx, task_id, ASK_TIMEOUT_DETAIL).await?;

        out.push(ExpiredAsk { ask_id, task_id });
    }

    tx.commit()
        .await
        .map_err(|e| DbError::Query(format!("asks expire_due commit: {e}")))?;
    Ok(out)
}

/// Cancel every pending ask belonging to a task. Returns how many moved.
///
/// Called from [`crate::tasks::mark_cancelled`] inside its transaction —
/// see the note there for why it lives inside the cancel path rather than
/// in a separate cancel-both helper. **Sole intended caller:**
/// `tasks::mark_cancelled`.
///
/// `pub(crate)`, not `pub`: called from anywhere else, this cancels an ask
/// out from under a task that was never actually cancelled, and nothing
/// reconciles the two afterwards.
///
/// Executor-generic, and takes a `task_id` rather than an ask id: the
/// caller is cancelling a *task* and does not know or care how many asks
/// it has.
pub(crate) async fn cancel_for_task<'e, E>(executor: E, task_id: i64) -> Result<u64, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let r = sqlx::query(
        "UPDATE asks \
         SET state = 'cancelled' \
         WHERE task_id = $1 AND state = 'pending'",
    )
    .bind(task_id)
    .execute(executor)
    .await
    .map_err(|e| DbError::Query(format!("asks cancel_for_task: {e}")))?;
    Ok(r.rows_affected())
}

/// Every ask still awaiting a human, oldest first — the operator inbox
/// read. Capped at `limit`; `created_at ASC` because the oldest question
/// is the one holding a task up longest, with `id ASC` as a tiebreaker —
/// `created_at` defaults to `transaction_timestamp()`, so two asks raised
/// in the same transaction (or just landing in the same clock tick) can
/// tie, and without a tiebreaker the inbox order for that pair is
/// nondeterministic across calls.
pub async fn list_pending(pool: &PgPool, limit: i64) -> Result<Vec<Ask>, DbError> {
    let limit = limit.max(0); // LIMIT -1 is a PG error
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {ASK_COLUMNS} FROM asks \
         WHERE state = 'pending' \
         ORDER BY created_at ASC, id ASC \
         LIMIT $1"
    )))
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::Query(format!("asks list_pending: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(decode_ask_row(row)?);
    }
    Ok(out)
}

/// Fetch one ask by id, in any state.
pub async fn get(pool: &PgPool, ask_id: i64) -> Result<Option<Ask>, DbError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {ASK_COLUMNS} FROM asks WHERE id = $1"
    )))
        .bind(ask_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| DbError::Query(format!("asks get: {e}")))?;
    row.as_ref().map(decode_ask_row).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `RaisedAsk`'s `Debug` must never render the plaintext nonce — it is
    /// a live approval token, the very thing [`Ask`] deliberately has no
    /// field for. Asserted here rather than trusted, mirroring
    /// `core::channel::peer_evidence_debug_redacts_the_presented_token`:
    /// the failure mode is silent, and a `tracing::debug!(?raised, …)`
    /// added anywhere on the resolve path would otherwise write the token
    /// straight into the daemon log.
    #[test]
    fn raised_ask_debug_redacts_the_nonce() {
        let raised = RaisedAsk { ask_id: 41, nonce: "S3CRET-NONCE-VALUE".to_string() };
        let rendered = format!("{raised:?}");
        assert!(!rendered.contains("S3CRET-NONCE-VALUE"), "nonce leaked into Debug: {rendered}");
        assert!(rendered.contains("redacted"), "must say it was redacted: {rendered}");
        // The ask id is not a secret and stays legible for diagnosis.
        assert!(rendered.contains("41"), "ask_id must stay visible: {rendered}");
    }

    /// Redacting `Debug` must not have cost the derived `Clone` the rest of
    /// the code relies on (`raise` returns an owned `RaisedAsk`; callers
    /// may need to hold onto a copy of the id/nonce pair).
    #[test]
    fn raised_ask_still_clones() {
        let raised = RaisedAsk { ask_id: 7, nonce: "abc123".to_string() };
        let cloned = raised.clone();
        assert_eq!(cloned.ask_id, raised.ask_id);
        assert_eq!(cloned.nonce, raised.nonce);
    }
}
