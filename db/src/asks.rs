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
//! * **Resolution happens exactly once.** [`resolve`] guards on
//!   `state = 'pending'` and reports rows-affected, so the first responder
//!   wins and every later one is told it lost — no lock, same idiom as
//!   `memories::set_embedding`.
//! * **The nonce is never readable.** Only its hash is stored, and
//!   [`Ask`] deliberately has no nonce field. Slice 2 matches an inbound
//!   nonce with a `WHERE nonce_sha256 = $1` predicate, never by reading
//!   the stored value out and comparing in Rust.

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
#[derive(Clone, Debug)]
pub struct RaisedAsk {
    pub ask_id: i64,
    pub nonce: String,
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

/// Resolve a pending ask and return its task to the queue.
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
         RETURNING task_id",
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
    let task_id: i64 = row
        .try_get("task_id")
        .map_err(|e| DbError::Query(format!("decode asks.task_id: {e}")))?;

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
