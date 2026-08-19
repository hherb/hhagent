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
//!   `tasks` SQL; this one calls its `&mut PgConnection` helpers inside its
//!   own transactions rather than writing `UPDATE tasks` here. Those
//!   helpers take a connection rather than any `Executor` precisely so a
//!   `&PgPool` cannot run one standalone, outside the transaction that
//!   makes the ask and its task move together.
//! * **Resolution happens exactly once.** [`resolve`] and
//!   [`resolve_with_nonce`] both guard on `state = 'pending'` and return
//!   the affected row, so the first responder wins and every later one is
//!   told it lost — no lock, same shape as `memories::set_embedding`.
//! * **The deadline is enforced by the resolvers, not only by the sweep.**
//!   Both guard on `deadline_at > now()`, so an unanswered ask stops being
//!   answerable at its deadline even if [`expire_due`] has not run — the
//!   bound must not be only as good as the sweeper's liveness.
//! * **A resolution names an option that was actually offered.** Both
//!   resolvers reject a `choice` outside the ask's own `options`, so the
//!   closed set the schema documents is a fact rather than a contract on a
//!   caller that does not exist yet.
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
use zeroize::Zeroize;

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

/// A correlation nonce in plaintext: the unforgeable capability to
/// resolve one specific ask.
///
/// **A newtype rather than a `String`, and the reason is a bug the type
/// makes impossible.** [`resolve_with_nonce`] takes the secret and the
/// caller's `resolved_by` attribution in adjacent parameters; as two
/// `&str`s, transposing them **compiles**. At runtime that hashes the peer
/// id (matching nothing, so a silent `Ok(false)` rather than an error) and
/// writes the **plaintext nonce into `asks.resolved_by`** — a column on a
/// table with no DELETE grant, whence it flows into the operator inbox and
/// slice 1b's audit rows. Every other precaution here (no field on
/// [`Ask`], hash-only storage, matching by SQL predicate) is defeated by
/// an argument swap, so the compiler holds this one.
///
/// No `Display`, no `Serialize`, no `Deref`, and `Debug` renders
/// `<redacted>`: the plaintext leaves only through [`Nonce::expose`],
/// which is greppable in a way `{}` is not. Zeroized on drop — the `db`
/// crate already zeroizes vault key material, and it would be odd to
/// leave a live approval token in a plain heap allocation.
#[derive(Clone)]
pub struct Nonce(String);

impl Nonce {
    /// Wrap a nonce that arrived over a transport (whatever a peer
    /// presented). Not validated here: [`resolve_with_nonce`]'s `WHERE`
    /// predicate is the only thing that decides whether it is real, and a
    /// syntactic pre-check would only leak which shapes are plausible.
    pub fn from_wire(nonce: String) -> Self {
        Self(nonce)
    }

    /// The plaintext. Deliberately verbose at every call site.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Nonce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Nonce(<redacted>)")
    }
}

impl Drop for Nonce {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// What [`raise`] hands back: the row id, and the correlation nonce **in
/// plaintext, exactly once**. Nothing persists the plaintext — if the
/// caller drops it, the ask can never be resolved through a nonce-bearing
/// transport again and must be expired or cancelled.
///
/// The derived [`Debug`] is safe because [`Nonce`]'s own impl redacts: it
/// is a live approval token — the very thing [`Ask`] deliberately has no
/// field for, one screen above this struct — and nothing debug-formats it
/// today, but the whole point of a `Debug` impl is that someone eventually
/// will. A `tracing::debug!(?raised, …)` added in a later slice would
/// otherwise write the plaintext straight into
/// `~/.local/state/kastellan/*.out`. Mirrors
/// `core::channel::PeerEvidence`, which redacts `presented_token` for the
/// identical reason.
#[derive(Clone, Debug)]
pub struct RaisedAsk {
    pub ask_id: i64,
    pub nonce: Nonce,
}

/// One row [`expire_due`] retired, for the caller's audit emission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpiredAsk {
    pub ask_id: i64,
    pub task_id: i64,
}

/// Lowercase hex SHA-256.
///
/// `pub` only so tests can assert that what landed in `nonce_sha256` is
/// the hash of the nonce [`raise`] returned — **not** as an invitation to
/// hash a nonce and compare it in Rust. [`resolve_with_nonce`] hashes
/// internally and matches with a `WHERE` predicate; see this module's
/// invariants. Nothing outside this crate needs to call it.
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
/// ours to suspend), then the row is inserted.
///
/// **Both orderings are equally safe against an orphan ask** — this is one
/// transaction, so a failed guard rolls the INSERT back either way (see
/// the `return Err` below, and `tasks::mark_cancelled`, whose `Ok(None)`
/// branch depends on exactly that). An earlier version of this comment
/// claimed insert-first "would leave an orphan ask behind", which the
/// rollback makes impossible. The ordering that actually matters is the
/// **lock** order, and this function is the one writer of both tables that
/// takes `tasks` before `asks`; `tasks::mark_cancelled`'s lock-order note
/// explains why that is tolerable here and what compensates for it.
///
/// Errors — rather than returning an `Option` — when the task is not
/// `running`. There is no benign reading of that: the caller believed it
/// held a claimed task and did not, so a silent `None` would let a plan
/// proceed as though the human had been asked. The error names the state
/// it actually found, because "already suspended on another ask" is a
/// distinct cause the three-way parenthetical used to point away from.
///
/// `deadline_at` must be in the future **by the database's clock**; the
/// `asks_deadline_after_created` CHECK rejects anything else rather than
/// minting an ask that is already expirable.
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

    if !tasks::suspend_for_ask(&mut tx, task_id).await? {
        // Dropping `tx` rolls back. Nothing was written.
        //
        // Name the state we actually found. The old message enumerated
        // "already terminal, cancelled, or never claimed" and omitted the
        // cause a second escalation actually hits — the task is already
        // `awaiting_operator` on another ask — sending the reader looking
        // at three states it is not in.
        let state = tasks::state_in_tx(&mut tx, task_id)
            .await?
            .unwrap_or_else(|| "<no such task>".to_string());
        return Err(DbError::Other(format!(
            "asks raise: task {task_id} is in state '{state}', not 'running', so it \
             cannot be suspended for an ask (already suspended on another ask, \
             terminal, cancelled, or never claimed)"
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

    Ok(RaisedAsk {
        ask_id,
        nonce: Nonce(nonce),
    })
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
/// not resolvable — already resolved by someone else, expired, cancelled,
/// **past its deadline**, or absent — and nothing was written.
///
/// The guard is `WHERE id = $1 AND state = 'pending' AND deadline_at >
/// now()`, with the presence of a `RETURNING` row as the answer: the same
/// race-safe idiom `memories::set_embedding` reaches for (that one reads
/// `rows_affected`; the shape and the guarantee are the same). It is what
/// makes resolution exactly-once and first-responder-wins across surfaces
/// (a Matrix reply and a CLI resolve racing each other) with no lock and
/// no read-then-write window.
///
/// **The deadline is in the predicate, not only in [`expire_due`].** An
/// ask whose deadline has passed is unresolvable from this instant,
/// whether or not the sweep has run — otherwise the bound is only as good
/// as the sweeper's liveness, and a nonce sitting in durable Matrix room
/// history stays a live approval token for as long as the sweep is down.
/// The sweep's remaining job is the task side: unwedging the suspended
/// task and recording the timeout.
///
/// `resolution` is a closed set and this function **enforces** it:
/// `{"choice": …}` must name one of that ask's own `options`, optionally
/// with `free_text` for the audit row. A resolution that does not is an
/// `Err` and the transaction rolls back — it is a protocol violation by
/// the caller, not a lost race, and storing it would leave slice 1b
/// reading a decision nobody offered. Free text is stored and shown,
/// never fed back into a plan.
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
         WHERE id = $1 AND state = 'pending' AND deadline_at > now() \
         RETURNING id, task_id, options",
    )
    .bind(ask_id)
    .bind(resolved_by)
    .bind(resolution)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| DbError::Query(format!("asks resolve: {e}")))?;

    let Some(row) = row else {
        // Lost the race, no such ask, or past its deadline. Dropping `tx`
        // rolls back; nothing was written either way.
        return Ok(false);
    };
    let (resolved_ask_id, task_id) = decode_resolved_ids(&row)?;
    reject_choice_outside_options(&row, resolved_ask_id, resolution)?;

    finish_resolve(tx, resolved_ask_id, task_id).await
}

/// Resolve a pending ask found by its correlation **nonce**, and return its
/// task to the queue.
///
/// **This is the path a channel/transport caller must use.** Callers pass
/// the plaintext nonce (whatever the peer presented, wrapped with
/// [`Nonce::from_wire`]); it is hashed here with [`sha256_hex`] and matched
/// against the stored `nonce_sha256` — never the other way around, so a DB
/// read still cannot recover a live token. Guarded `WHERE nonce_sha256 =
/// $1 AND state = 'pending' AND deadline_at > now()`, so a peer who does
/// not hold the nonce [`raise`] handed out cannot resolve (or even
/// discover) anyone else's ask. See [`resolve`]'s doc for why the by-id
/// form is not safe for this caller.
///
/// **Timing is not a concern here and "fixing" it would be a regression.**
/// What the `WHERE` compares is the SHA-256 *hash*, not the token, so a
/// timing oracle over a btree probe (which is emphatically not
/// constant-time) leaves an attacker with a preimage problem. Do not
/// "harden" this by reading `nonce_sha256` out and calling
/// `core::channel::auth::constant_time_eq` — that would reintroduce the
/// Rust-side comparison this module exists to avoid.
///
/// **What this does NOT establish: that the peer is entitled to answer.**
/// The nonce proves the caller holds the capability for *this* ask; it says
/// nothing about who they are, and `resolved_by` is an unverified string
/// this function stores verbatim into the audit trail. Pairing the nonce
/// with a peer `channel::auth` has already authorized — "id and authority
/// kept separate", per the ROADMAP — is the caller's job in slice 2.
///
/// Same semantics as [`resolve`] otherwise: one transaction, exactly-once,
/// first-responder-wins, `choice` enforced against `options`. Returns
/// `true` iff **this** call resolved it; `false` for a wrong/unissued
/// nonce, an already-resolved ask, one that expired/was cancelled, or one
/// past its deadline. Those cases are deliberately indistinguishable —
/// splitting them would hand a nonce-guessing peer an existence oracle
/// over ask ids.
pub async fn resolve_with_nonce(
    pool: &PgPool,
    nonce: &Nonce,
    resolved_by: &str,
    resolution: &serde_json::Value,
) -> Result<bool, DbError> {
    let nonce_hash = sha256_hex(nonce.expose());

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
         WHERE nonce_sha256 = $1 AND state = 'pending' AND deadline_at > now() \
         RETURNING id, task_id, options",
    )
    .bind(&nonce_hash)
    .bind(resolved_by)
    .bind(resolution)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| DbError::Query(format!("asks resolve_with_nonce: {e}")))?;

    let Some(row) = row else {
        // Wrong/unissued nonce, lost the race, or past the deadline.
        // Dropping `tx` rolls back; nothing was written either way.
        return Ok(false);
    };
    let (resolved_ask_id, task_id) = decode_resolved_ids(&row)?;
    reject_choice_outside_options(&row, resolved_ask_id, resolution)?;

    finish_resolve(tx, resolved_ask_id, task_id).await
}

/// Enforce that `resolution.choice` names one of the ask's own `options`.
///
/// The migration calls `resolution` a closed set; before this, nothing
/// made that true, and the idiomatic slice-1b read
/// (`…get("choice")…as_str() == Some("deny")`) puts every malformed,
/// absent, or misspelled value in the **proceed** arm. So the check lives
/// on the write path, where there is exactly one of it.
///
/// Runs *after* the guarded UPDATE rather than before: the row must be
/// found and locked to know which `options` apply, and returning `Err`
/// rolls the whole transaction back, so a rejected resolution writes
/// nothing. `{choice:?}` and not `{choice}` — `Debug` for `&str` escapes
/// control characters, and this value came off an untrusted transport
/// (#544's lesson: never render untrusted text raw into something a
/// terminal will display).
fn reject_choice_outside_options(
    row: &PgRow,
    ask_id: i64,
    resolution: &serde_json::Value,
) -> Result<(), DbError> {
    let options: serde_json::Value = row
        .try_get("options")
        .map_err(|e| DbError::Query(format!("decode asks.options: {e}")))?;

    let choice = resolution
        .get("choice")
        .and_then(|c| c.as_str())
        .ok_or_else(|| {
            DbError::Other(format!(
                "asks resolve: ask {ask_id} resolution must be an object with a string \
                 `choice`; refusing to store a decision that names no option"
            ))
        })?;

    let offered = options.as_array().ok_or_else(|| {
        DbError::Other(format!(
            "asks resolve: ask {ask_id} has a non-array `options`, so no choice can be \
             validated against it"
        ))
    })?;

    if !offered.iter().any(|o| o.as_str() == Some(choice)) {
        return Err(DbError::Other(format!(
            "asks resolve: ask {ask_id} choice {choice:?} is not one of the {} options it \
             offered — refusing to record a decision the operator was never given",
            offered.len()
        )));
    }
    Ok(())
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
    if !tasks::resume_from_ask(&mut tx, task_id).await? {
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
        // `awaiting_operator` the ask still expires. What must not happen
        // is the reverse — an expired ask with a still-suspended task —
        // and that is what the shared transaction guarantees.
        //
        // The `false` case is NOT a wedge (the guard is exactly
        // `state = 'awaiting_operator'`, so a miss proves the task is not
        // parked in it), but it is the same class of invariant violation
        // `finish_resolve` raises a loud `Err` for, and dropping it
        // silently is what made the two paths disagree. It is reachable
        // only by a writer touching `tasks.state` outside the five
        // functions that own this transition — direct SQL, or a future
        // call site. Warn, and do not claim the transition in the returned
        // row: the caller emits one audit row per `ExpiredAsk`, and that
        // row would otherwise assert a task failure that did not happen.
        let task_failed =
            tasks::fail_awaiting_operator(&mut tx, task_id, ASK_TIMEOUT_DETAIL).await?;
        if !task_failed {
            tracing::warn!(
                ask_id,
                task_id,
                "expired an ask whose task was no longer awaiting_operator; \
                 the ask is expired but no task transition is being reported",
            );
            continue;
        }

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
/// Takes a `&mut PgConnection`, not a generic `E: Executor`, and takes a
/// `task_id` rather than an ask id: the caller is cancelling a *task* and
/// does not know or care how many asks it has.
///
/// The narrower parameter is load-bearing. `E: Executor` also accepts
/// `&PgPool`, which would run this UPDATE **standalone** — committing an
/// ask cancel that its caller's transaction may then roll back, i.e. an
/// ask cancelled out from under a task that is still alive. `&mut *tx`
/// derefs to exactly this type, so every real call site is unchanged and
/// the pool version stops compiling. (It still does not *prove* a
/// transaction — a `pool.acquire()` handle fits too — it removes the
/// failure that was one keystroke away.)
pub(crate) async fn cancel_for_task(
    conn: &mut sqlx::PgConnection,
    task_id: i64,
) -> Result<u64, DbError> {
    let r = sqlx::query(
        "UPDATE asks \
         SET state = 'cancelled' \
         WHERE task_id = $1 AND state = 'pending'",
    )
    .bind(task_id)
    .execute(conn)
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

/// **Every** decision an operator has already made about this task,
/// newest first. Empty when nobody has answered anything yet.
///
/// Slice 1b's single read: `run_one` calls it once per claimed task and
/// both consumers work from that one list — the pre-plan deny check and
/// the `Escalate` arm's digest comparison (spec D4).
///
/// **All of them, not just the newest, and that is the whole point.** An
/// approval binds to a *plan digest*, so the caller has to answer "did the
/// operator approve THIS plan", which is a lookup by digest and not by
/// recency. A task that escalates at two different plans holds two
/// approvals at once; returning only the newest made the older one
/// invisible, so the earlier plan re-asked a question that had already been
/// answered — and approving *that* made the newer approval the stale one.
/// The two alternate forever, and `resume_budget` hands out a fresh plan
/// allowance on every resume, so nothing but the ask deadline ends it. A
/// task raises one ask per escalation, so this list is small and bounded by
/// how many times a human chose to answer.
///
/// **`state = 'resolved'` only.** An `expired` or `cancelled` ask is not a
/// decision anybody made, and returning one would let a timeout read as an
/// answer. A `pending` ask cannot be seen here either, and that is not
/// merely filtered: a task with a pending ask is `awaiting_operator`, which
/// `claim_one` never returns, so no caller of this function can be running
/// one.
///
/// Ordered `resolved_at DESC, id DESC`. `resolved_at` is `now()` at resolve
/// time, so two asks resolved inside one transaction tick can tie — the
/// same tiebreaker [`list_pending`] carries, for the same reason. The order
/// is no longer load-bearing for *which* decision applies (the caller
/// matches on digest), but it stays deterministic so a caller that does
/// look at `first()` — an operator display, a log line — sees the same row
/// on every call rather than whatever physical row order the planner picked.
pub async fn resolved_for_task(pool: &PgPool, task_id: i64) -> Result<Vec<Ask>, DbError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {ASK_COLUMNS} FROM asks \
         WHERE task_id = $1 AND state = 'resolved' \
         ORDER BY resolved_at DESC, id DESC"
    )))
    .bind(task_id)
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::Query(format!("asks resolved_for_task: {e}")))?;

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
        let raised = RaisedAsk {
            ask_id: 41,
            nonce: Nonce("S3CRET-NONCE-VALUE".to_string()),
        };
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
        let raised = RaisedAsk {
            ask_id: 7,
            nonce: Nonce("abc123".to_string()),
        };
        let cloned = raised.clone();
        assert_eq!(cloned.ask_id, raised.ask_id);
        assert_eq!(cloned.nonce.expose(), raised.nonce.expose());
    }

    /// [`Nonce`]'s own `Debug` is what [`RaisedAsk`]'s derived one leans on,
    /// so it is asserted directly too — a future struct holding a `Nonce`
    /// inherits this and nothing else needs to remember to redact.
    #[test]
    fn nonce_debug_redacts() {
        let n = Nonce("S3CRET-NONCE-VALUE".to_string());
        let rendered = format!("{n:?}");
        assert!(!rendered.contains("S3CRET"), "nonce leaked into Debug: {rendered}");
        assert!(rendered.contains("redacted"), "must say it was redacted: {rendered}");
        // ...and the plaintext is still reachable, deliberately verbosely.
        assert_eq!(n.expose(), "S3CRET-NONCE-VALUE");
    }

    /// A known-answer test for [`sha256_hex`], because every other
    /// assertion about it in this tree compares it against ITSELF: `raise`
    /// writes `sha256_hex(nonce)`, `resolve_with_nonce` looks up
    /// `sha256_hex(nonce)`, and the e2e asserts the two match. Storage,
    /// lookup and test all move together, so truncating the digest — or
    /// swapping SHA-256 for any other 32-bytes-in/hex-out function — passes
    /// all of them. Vector from NIST FIPS 180-2 (the same one
    /// `core::recall_assembly` pins its own hasher against).
    #[test]
    fn sha256_hex_matches_the_known_answer_for_abc() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        );
        // Empty input, the other standard vector — pins that nothing
        // special-cases the zero-length case.
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
    }

    /// `generate_nonce` must draw its full width from the CSPRNG. Length
    /// alone does not establish that: `fill_bytes(&mut bytes[..4])` still
    /// yields 64 hex chars and still differs between two draws, while
    /// leaving 28 of 32 bytes permanently zero — for a token whose only job
    /// is to be unguessable by an untrusted peer.
    #[test]
    fn generate_nonce_varies_in_every_byte_position() {
        const DRAWS: usize = 64;
        let sample: Vec<String> = (0..DRAWS).map(|_| generate_nonce()).collect();

        for n in &sample {
            assert_eq!(n.len(), NONCE_BYTES * 2, "32 bytes hex-encoded");
            assert!(n.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        }

        // Every hex position must take at least two distinct values across
        // the sample. With 64 draws a genuinely random nibble is constant
        // with probability 16 * (1/16)^64 — far below any flake threshold.
        for pos in 0..NONCE_BYTES * 2 {
            let distinct: std::collections::HashSet<char> =
                sample.iter().map(|n| n.as_bytes()[pos] as char).collect();
            assert!(
                distinct.len() > 1,
                "hex position {pos} is constant across {DRAWS} nonces — is the CSPRNG \
                 filling the whole buffer?",
            );
        }
    }
}
