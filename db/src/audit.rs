//! Append-only audit-log writes and reads.
//!
//! ## Where rows come from
//!
//! Write sites are many and growing — the daemon's bring-up row
//! ([`crate::probe::run`]), every tool call (`core::tool_host::dispatch`),
//! memory writes, egress verdicts, secrets administration, channel I/O.
//! An enumeration here only ever went stale (it said "exactly two" long
//! after there were twenty). The invariant worth stating is that **every
//! one of them goes through [`insert`]** — so every one of them is capped
//! by [`truncate_payload`], and every one of them writes as
//! [`crate::conn::RUNTIME_ROLE`], which is what makes the GRANT below
//! load-bearing rather than advisory.
//!
//! *How* that role is assumed differs at exactly one site, and the
//! difference is worth knowing before trusting the sentence above: the
//! runtime pool sets it once per connection through the `after_connect`
//! hook on [`crate::pool::connect_runtime_pool`], whereas
//! [`crate::probe::run`] issues an explicit `SET ROLE` on the single bare
//! connection it migrates over — the pool does not exist yet at that point
//! in bring-up. Same role, same [`insert`], different mechanism.
//!
//! The shape `(actor, action, payload)` is deliberately schema-less so
//! every future write site (memory writer, channel I/O, scheduler
//! transitions) can use the same single insert path.
//!
//! ## Append-only by *both* convention and database GRANT
//!
//! Migration `0002_runtime_role.sql` REVOKEs `UPDATE, DELETE,
//! TRUNCATE` on `audit_log` from [`crate::conn::RUNTIME_ROLE`]. So a
//! compromised dispatcher path running under the runtime role gets a
//! `permission denied` from Postgres if it tries to rewrite a row.
//! The application-level discipline of "only this module writes
//! audit rows" is layered on top — defense in depth.
//!
//! ## Truncation policy
//!
//! Tool-call payloads can be arbitrarily large (a `web-fetch` worker
//! could in principle return a megabyte of HTML). Storing the entire
//! body as JSONB inflates the table, the WAL, and the JSONL mirror
//! file with no operational value — operators tail the audit log to
//! see *who did what*, not to recover request bodies.
//!
//! [`truncate_payload`] enforces a 4 KiB cap (after JSON serialisation):
//! oversize payloads are replaced with a small envelope carrying a
//! SHA-256 fingerprint of the original bytes plus the original byte
//! length. The fingerprint lets two truncated rows be compared for
//! equality without storing the bytes themselves; the length tells an
//! operator how much was elided.
//!
//! **[`PRESERVED_KEYS`] ride through that replacement.** "Who did what"
//! includes the outcome of a control that ran on the payload, and such a
//! record is bounded, tiny, and — unlike a request body — recoverable
//! from nowhere else. Dropping it was measured live: on 2026-08-23 two
//! 85 KB `web.fetch` rows took the guard tier's score down with them.
//! A preserved key that still cannot be afforded is named by
//! [`DROPPED_PRESERVED_KEY`] rather than vanishing, because an
//! unrecorded loss is the shape of the defect above.
//!
//! Pure: returns a new `serde_json::Value`, performs no I/O. Tested
//! with deterministic-fingerprint regression pins.

use sqlx::Row;

use crate::DbError;

/// Maximum size in bytes of a serialised `audit_log.payload` JSONB
/// value before [`truncate_payload`] replaces it with a fingerprint
/// envelope.
///
/// 4 KiB is the same threshold called out in HANDOVER's Option I
/// brief. It comfortably holds a typical tool-call request/response
/// summary (`{"req": {...}, "result": {...}, "ms": 12}`) while
/// preventing any single row from dominating the `audit_log` heap or
/// the JSONL mirror line count.
pub const PAYLOAD_MAX_BYTES: usize = 4096;

/// Payload keys carried THROUGH truncation instead of being replaced by
/// the fingerprint envelope.
///
/// A key earns a place here only if it is all three of:
///
/// 1. **bounded by construction** — a fixed set of scalars, so it cannot
///    itself push the envelope over [`PAYLOAD_MAX_BYTES`];
/// 2. **a decision record, not data** — the outcome of a control, not the
///    document the control ran on. The cap exists to stop bodies dominating
///    the heap, and preserving a body under another name would defeat it;
/// 3. **irrecoverable** — it exists nowhere else *on the path that matters*.
///    `req` and `result` can be reconstructed from the worker and the
///    surrounding rows. A guard score is computed once, in memory; a Block
///    mirrors its `p`/`tau` onto the forensic `policy` / `injection.blocked`
///    row, but a **clear writes no second row at all**, and the cleared half
///    is exactly the half D5 needs.
///
/// `guard` is the wiring slice's per-dispatch guard-tier report
/// (`{state, p, tau, ms, body_byte_len, truncated}`). Before it was listed
/// here, a tool result over the cap took the score with it — measured live
/// on 2026-08-23 at 85,352 bytes — which silently inverted spec D5: blocked
/// dispatches kept their score (their result is a short placeholder) while
/// *cleared* ones lost theirs, leaving a size-selected sample that reads
/// like a score distribution.
///
/// This is a **wire contract** in the same sense as
/// [`TRUNCATED_MARKER_KEY`]: readers may rely on a preserved key meaning
/// exactly what it meant in the untruncated payload.
///
/// **Array order is priority order.** Keys are admitted left to right
/// against a shared budget, so an earlier member can starve a later one.
/// Moot at one member; decide it deliberately before adding a second.
pub const PRESERVED_KEYS: &[&str] = &[GUARD_KEY];

/// The payload key under which `core::tool_host::post_process` records the
/// guard tier's per-dispatch verdict — and the sole member of
/// [`PRESERVED_KEYS`].
///
/// It is a `const` rather than two independent string literals because the
/// producer lives in another crate. Spelled twice, a rename on either side
/// would compile, and preservation would stop silently: cleared scores
/// would vanish above the cap while blocked ones survived, which is
/// precisely the defect [`PRESERVED_KEYS`] was added to fix, restored
/// without a single failing test. Spelled once, that rename is a compile
/// error.
pub const GUARD_KEY: &str = "guard";

/// Payload key naming the [`PRESERVED_KEYS`] that did **not** fit.
///
/// Without it, an envelope whose preserved key was dropped for size carries
/// the *same key set* as one whose payload never carried that key (the
/// fingerprints differ, but they describe inputs a reader does not have) —
/// so a reader cannot tell "the control never ran" from "the control ran,
/// and its verdict was discarded here". That ambiguity *is* the defect this
/// allowlist exists to fix, one function further down; leaving it in place
/// would be fixing the loss and keeping the silence.
///
/// A **wire contract** in the same sense as [`TRUNCATED_MARKER_KEY`]:
/// present only on an envelope, and only when something was actually lost.
pub const DROPPED_PRESERVED_KEY: &str = "_dropped_preserved";

/// Bytes held back from [`PAYLOAD_MAX_BYTES`] so [`DROPPED_PRESERVED_KEY`]
/// is *always* affordable.
///
/// A drop that could not be recorded would be a silent drop again, so the
/// room for the record is reserved before any preserved key is admitted
/// rather than hoped for afterwards. [`drop_marker_worst_case`] is checked
/// against this at compile time.
///
/// Public because [`truncate_payload`]'s one hard postcondition is stated
/// in terms of it: a caller reasoning about what will survive the cap
/// cannot do so without this number.
pub const DROP_MARKER_RESERVE: usize = 64;

/// The envelope's fingerprint keys. Private — [`is_truncation_envelope`] is
/// the supported way to read an envelope — but named once so the producer
/// and the compile-time shadow check below cannot drift apart.
const FINGERPRINT_SHA256_KEY: &str = "sha256";
/// See [`FINGERPRINT_SHA256_KEY`].
const FINGERPRINT_LEN_KEY: &str = "len";

/// `a == b` for `&str` in const context, which `PartialEq` is not.
const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Worst-case serialised cost of adding [`DROPPED_PRESERVED_KEY`] to an
/// object that already has keys: one separating comma, the quoted key, a
/// colon, and an array naming every member of [`PRESERVED_KEYS`].
///
/// The comma is counted, not located. `serde_json`'s `Map` is a `BTreeMap`
/// unless `preserve_order` is enabled, so `_dropped_preserved` actually
/// sorts *before* `_truncated` and its separator is emitted after the pair
/// rather than before it — exactly one comma either way, and the count is
/// what the reserve is spent on.
///
/// Deliberately an over-estimate: the per-member term charges a trailing
/// comma to every element while `serde_json` emits only `N-1` of them.
const fn drop_marker_worst_case() -> usize {
    // `,"<key>":[]`
    let mut n = DROPPED_PRESERVED_KEY.len() + 6;
    let mut i = 0;
    while i < PRESERVED_KEYS.len() {
        // `"<key>",`
        n += PRESERVED_KEYS[i].len() + 3;
        i += 1;
    }
    n
}

/// The envelope's own keys are reserved, and the marker is affordable.
///
/// [`PRESERVED_KEYS`]' three admission criteria are prose, and a key can
/// satisfy all three and still be catastrophic: `len` or `sha256` would
/// silently overwrite the fingerprint — so two rows for the same body stop
/// comparing equal, which is precisely what
/// `a_preserved_key_does_not_change_the_fingerprint` was written to protect
/// and cannot catch for a key it does not name. [`TRUNCATED_MARKER_KEY`] is
/// worse still: shadowing it makes [`is_truncation_envelope`] report
/// whatever the payload happened to carry, resurrecting the issue-#62
/// misclassification the predicate exists to prevent.
///
/// Prose cannot enforce that. This can, at compile time, for every future
/// member — which is the same move the size postcondition already makes.
///
/// [`DROPPED_PRESERVED_KEY`] is checked against the same reserved names, and
/// not only against [`PRESERVED_KEYS`]: it is written onto the envelope by
/// the same `insert`, so pointing it at `len` would overwrite the
/// fingerprint just as surely — the failure this block exists to make
/// impossible, reachable through the one key it used not to look at.
const _: () = {
    // The three keys the envelope builds itself from. Shadowing any of
    // them means writing over the fingerprint, or over the marker that
    // makes the row self-declaring.
    //
    // `plan` is not one of them, and is here for a different reason:
    // promoting it would falsify
    // `core::observation::CapturedPlan::source_truncated`'s documented
    // guarantee that a null `plan_json` on a truncated row is an artefact
    // of truncation. A future member meets that reader here rather than in
    // production.
    let reserved =
        [TRUNCATED_MARKER_KEY, FINGERPRINT_SHA256_KEY, FINGERPRINT_LEN_KEY, "plan"];

    let mut j = 0;
    while j < reserved.len() {
        let mut i = 0;
        while i < PRESERVED_KEYS.len() {
            assert!(
                !str_eq(PRESERVED_KEYS[i], reserved[j]),
                "a PRESERVED_KEYS member may not shadow a reserved payload key"
            );
            i += 1;
        }
        // The marker is written onto the same envelope by the same
        // `insert`, so pointing it at `len` would overwrite the
        // fingerprint just as surely. Checking only PRESERVED_KEYS left
        // this reachable through the one key the block did not look at.
        assert!(
            !str_eq(DROPPED_PRESERVED_KEY, reserved[j]),
            "DROPPED_PRESERVED_KEY may not shadow a reserved payload key"
        );
        j += 1;
    }

    let mut i = 0;
    while i < PRESERVED_KEYS.len() {
        assert!(
            !str_eq(PRESERVED_KEYS[i], DROPPED_PRESERVED_KEY),
            "a PRESERVED_KEYS member may not shadow the dropped-key marker"
        );
        i += 1;
    }

    assert!(
        drop_marker_worst_case() <= DROP_MARKER_RESERVE,
        "DROP_MARKER_RESERVE no longer covers the marker PRESERVED_KEYS can produce"
    );
};

/// Payload key that marks a [`truncate_payload`] envelope. This is a **wire
/// contract**: readers in other crates (e.g. `kastellan-core`'s observation
/// capture, issue #62) detect truncation via [`is_truncation_envelope`], so
/// the writer and every reader share this single definition rather than
/// re-spelling the literal.
pub const TRUNCATED_MARKER_KEY: &str = "_truncated";

/// True iff `payload` is a truncation envelope produced by
/// [`truncate_payload`] — i.e. the original payload was over budget and
/// its keys were replaced by the `{_truncated, sha256, len}` fingerprint,
/// except any listed in [`PRESERVED_KEYS`], which ride along unchanged (and
/// [`DROPPED_PRESERVED_KEY`], which names those that could not).
///
/// The predicate deliberately tests only the marker, so adding a preserved
/// key stays additive for every existing reader.
///
/// Lives next to the producer so the two cannot drift: a shape change to the
/// envelope must update this predicate (and the shape-pin test below) in the
/// same file. Pure.
pub fn is_truncation_envelope(payload: &serde_json::Value) -> bool {
    payload
        .get(TRUNCATED_MARKER_KEY)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// One decoded `audit_log` row.
///
/// `payload` is whatever the writer stored — a `serde_json::Value`
/// (which may itself be a [`truncate_payload`] envelope). Decoding
/// happens through sqlx's `JsonValue` codec, which is enabled via the
/// workspace `sqlx` feature `"json"`.
#[derive(Clone, Debug)]
pub struct AuditRow {
    /// Strictly monotonic `BIGSERIAL` from the table.
    pub id: i64,
    /// `now()`-derived TIMESTAMPTZ from the row's `DEFAULT`. The
    /// audit-mirror task ships this verbatim (RFC 3339-ish via
    /// `time::OffsetDateTime`'s default `Display`).
    pub ts: time::OffsetDateTime,
    /// Free-form short string identifying who wrote the row.
    /// Conventions: `"core"` for daemon-internal events,
    /// `"tool:<name>"` for dispatcher-mediated tool calls,
    /// `"channel:<adapter>"` for channel I/O (Phase 2+).
    pub actor: String,
    /// Verb describing what happened: `"startup"`, `"call"`,
    /// `"deny"`, etc. Free-form, paired with `actor`.
    pub action: String,
    /// Structured details. May be a [`truncate_payload`] envelope.
    pub payload: serde_json::Value,
}

/// Returns the JSONB payload to *actually store* for a given input.
///
/// If the input serialises to ≤ [`PAYLOAD_MAX_BYTES`], the input is
/// returned unchanged. Otherwise it is replaced with:
///
/// ```json
/// { "_truncated": true, "sha256": "<64 hex>", "len": <bytes> }
/// ```
///
/// where `len` is the original serialised byte length and `sha256` is
/// the lowercase-hex SHA-256 digest of the same bytes — **of the input,
/// not of the envelope**, so two rows for the same body still compare
/// equal whatever else they carry.
///
/// Any [`PRESERVED_KEYS`] present in the input are then copied onto the
/// envelope **verbatim, whatever their value** — including a `null` or a
/// scalar, since this function judges keys and not shapes — because a
/// bounded decision record is not what the cap is defending against and is
/// worth more than the bytes it costs.
///
/// Keys are admitted **one at a time**, each against the budget less
/// [`DROP_MARKER_RESERVE`]. Two properties follow that an all-or-nothing
/// copy does not have: one oversized key cannot take a bounded sibling
/// down with it, and anything refused can still be *named*, under
/// [`DROPPED_PRESERVED_KEY`]. Nothing in this workspace can produce an
/// oversized preserved key — the one producer is
/// `core::tool_host::post_process`, via `GuardReport::audit_value`, which
/// emits six scalars — but the signature permits one, and
/// [`preserve_onto`] is tested against multi-key lists that exercise it.
///
/// The budget postcondition is therefore structural rather than checked
/// after the fact: every admitted key left [`DROP_MARKER_RESERVE`] bytes
/// free, and the compile-time assertion above pins the marker's worst case
/// under that reserve.
///
/// Pure: deterministic, no I/O, no global state. Same input → same
/// output, every call.
pub fn truncate_payload(payload: serde_json::Value) -> serde_json::Value {
    // `to_vec` is infallible for `serde_json::Value` (the value is
    // already valid JSON in memory). The serialised form is what
    // Postgres will see — so that's the form we measure.
    let bytes = serde_json::to_vec(&payload).expect("serde_json::Value cannot fail to serialise");
    if bytes.len() <= PAYLOAD_MAX_BYTES {
        return payload;
    }

    use sha2::Digest;
    let digest = sha2::Sha256::digest(&bytes);
    let mut hex = String::with_capacity(64);
    for b in digest.iter() {
        // Two lowercase hex chars per byte. Width-padded so a leading
        // zero in any byte is preserved — `format!("{b:02x}")` is the
        // canonical idiom for reproducible hex.
        use std::fmt::Write;
        write!(&mut hex, "{:02x}", b).expect("write to String cannot fail");
    }

    let mut bare = serde_json::Map::new();
    bare.insert(TRUNCATED_MARKER_KEY.to_string(), serde_json::Value::Bool(true));
    bare.insert(FINGERPRINT_SHA256_KEY.to_string(), serde_json::Value::String(hex));
    bare.insert(FINGERPRINT_LEN_KEY.to_string(), serde_json::json!(bytes.len()));

    // Only an object can have keys to preserve; a bare string or array
    // over the cap is all data by definition. Nothing is lost silently
    // here that the fingerprint does not already record: a non-object
    // payload has no keys, so no preserved key can have gone missing.
    let Some(source) = payload.as_object() else {
        return serde_json::Value::Object(bare);
    };

    preserve_onto(bare, source, PRESERVED_KEYS)
}

/// Copy each of `keys` from `source` onto `envelope`, admitting one at a
/// time against [`PAYLOAD_MAX_BYTES`] less [`DROP_MARKER_RESERVE`], and
/// naming under [`DROPPED_PRESERVED_KEY`] any that would not fit.
///
/// Split out of [`truncate_payload`] so `keys` can be a **parameter**.
/// With [`PRESERVED_KEYS`] at one member, the interesting half of this
/// function is unreachable from its only production caller: a running
/// envelope is indistinguishable from a fixed one, a starved sibling
/// cannot exist, and [`DROPPED_PRESERVED_KEY`] can never name more than a
/// single key. Those are the properties [`truncate_payload`]'s doc claims,
/// so they are tested here against multi-key lists rather than argued.
///
/// Measurement is exact rather than estimated: the candidate is inserted,
/// the whole object is serialised, and a key that does not fit is taken
/// back out. `displaced` restores a value this would otherwise clobber —
/// unreachable for [`PRESERVED_KEYS`], which the compile-time block above
/// forbids from shadowing an envelope key, but `keys` is a parameter and a
/// helper that silently ate a fingerprint under test would be its own
/// small version of the defect this file is about.
///
/// Pure.
fn preserve_onto(
    mut envelope: serde_json::Map<String, serde_json::Value>,
    source: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> serde_json::Value {
    let mut dropped: Vec<&str> = Vec::new();
    for &key in keys {
        let Some(value) = source.get(key) else { continue };
        let displaced = envelope.insert(key.to_string(), value.clone());
        // Infallible for a reason stronger than "Values serialise": this
        // subtree was already serialised whole by the caller, so a second
        // pass over a subset of it cannot fail.
        let grown = serde_json::to_vec(&envelope).expect("a JSON object cannot fail to serialise");
        // The reserve is what keeps the ELSE arm affordable: a key refused
        // for size must still leave room to say so.
        if grown.len() + DROP_MARKER_RESERVE <= PAYLOAD_MAX_BYTES {
            continue;
        }
        match displaced {
            Some(prev) => envelope.insert(key.to_string(), prev),
            None => envelope.remove(key),
        };
        dropped.push(key);
    }

    if !dropped.is_empty() {
        envelope.insert(DROPPED_PRESERVED_KEY.to_string(), serde_json::json!(dropped));
    }
    serde_json::Value::Object(envelope)
}

/// Insert one row into `audit_log` and return its `id`.
///
/// `payload` flows through [`truncate_payload`] so the caller does not
/// have to enforce the cap themselves. The insert is a single round-trip
/// (`INSERT … RETURNING id`) — there is no separate SELECT.
///
/// `executor` is generic so this works against both a `&PgPool`
/// (production: dispatcher write site) and a `&mut PgConnection`
/// (tests: deterministic single-connection setup against a per-test
/// cluster). Both implement [`sqlx::Executor`] for the
/// [`sqlx::Postgres`] backend.
///
/// Errors propagate as [`DbError::Query`] — the wrapped message includes
/// the underlying sqlx error so a `permission denied` from the runtime
/// role's REVOKEs is operator-readable in the daemon log.
pub async fn insert<'e, E>(
    executor: E,
    actor: &str,
    action: &str,
    payload: serde_json::Value,
) -> Result<i64, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let payload = truncate_payload(payload);
    let row = sqlx::query(
        "INSERT INTO audit_log (actor, action, payload) \
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(actor)
    .bind(action)
    .bind(payload)
    .fetch_one(executor)
    .await
    .map_err(|e| DbError::Query(format!("audit_log insert: {e}")))?;
    row.try_get::<i64, _>(0)
        .map_err(|e| DbError::Query(format!("decode audit_log.id: {e}")))
}

/// Fetch one row by `id`. Used by the audit-mirror task to expand a
/// NOTIFY payload (which carries only the id) into the full row that
/// gets written to the JSONL file.
///
/// Returns [`DbError::Query`] if the row does not exist — which can
/// happen legitimately when the listener catches a NOTIFY for a row
/// that was rolled back between trigger fire and SELECT. Callers
/// should treat "row not found" as a benign skip, not a hard error.
pub async fn fetch_by_id<'e, E>(executor: E, id: i64) -> Result<AuditRow, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let row = sqlx::query(
        "SELECT id, ts, actor, action, payload \
         FROM audit_log WHERE id = $1",
    )
    .bind(id)
    .fetch_one(executor)
    .await
    .map_err(|e| DbError::Query(format!("audit_log fetch_by_id({id}): {e}")))?;
    decode_audit_row(&row)
}

/// Fetch every row with `id > since`, ordered by `id`. The mirror task
/// uses this on first start (since=0 → drain the whole table) and on
/// listener reconnect (since=last_seen_id → catch up on rows committed
/// while we weren't listening).
///
/// `limit` caps the number of rows pulled in one call so a multi-day
/// outage doesn't OOM the listener. The caller loops until the result
/// is shorter than `limit`.
pub async fn fetch_since<'e, E>(
    executor: E,
    since: i64,
    limit: i64,
) -> Result<Vec<AuditRow>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query(
        "SELECT id, ts, actor, action, payload \
         FROM audit_log WHERE id > $1 ORDER BY id LIMIT $2",
    )
    .bind(since)
    .bind(limit)
    .fetch_all(executor)
    .await
    .map_err(|e| DbError::Query(format!("audit_log fetch_since({since}): {e}")))?;
    rows.iter().map(decode_audit_row).collect()
}

fn decode_audit_row(row: &sqlx::postgres::PgRow) -> Result<AuditRow, DbError> {
    Ok(AuditRow {
        id: row
            .try_get(0)
            .map_err(|e| DbError::Query(format!("decode audit_log.id: {e}")))?,
        ts: row
            .try_get(1)
            .map_err(|e| DbError::Query(format!("decode audit_log.ts: {e}")))?,
        actor: row
            .try_get(2)
            .map_err(|e| DbError::Query(format!("decode audit_log.actor: {e}")))?,
        action: row
            .try_get(3)
            .map_err(|e| DbError::Query(format!("decode audit_log.action: {e}")))?,
        payload: row
            .try_get(4)
            .map_err(|e| DbError::Query(format!("decode audit_log.payload: {e}")))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small payloads pass through unchanged — the truncation envelope
    /// must not be wrapped around already-fitting values.
    ///
    /// The fixture carries a preserved key deliberately: the under-cap
    /// path must return the payload *itself*, not an envelope that
    /// happens to have rescued the same key. Those two are easy to
    /// conflate once preservation exists.
    #[test]
    fn small_payload_is_not_truncated() {
        let v = serde_json::json!({
            "actor": "core",
            "ms": 12,
            (GUARD_KEY): {"state": "clear", "p": 0.5},
        });
        let out = truncate_payload(v.clone());
        assert_eq!(out, v);
        assert!(!is_truncation_envelope(&out), "a fitting payload is not an envelope");
    }

    /// Empty object is the canonical default and must stay byte-for-byte.
    #[test]
    fn empty_object_passes_through() {
        let v = serde_json::json!({});
        assert_eq!(truncate_payload(v.clone()), v);
    }

    /// A payload at exactly the threshold byte count must NOT be
    /// truncated — the bound is inclusive. (Off-by-one regression
    /// guard: an earlier draft used `<` instead of `<=`.)
    #[test]
    fn payload_at_exact_threshold_is_not_truncated() {
        // Build a string whose JSON serialisation is exactly
        // `PAYLOAD_MAX_BYTES`. The serialisation of `"...payload..."`
        // adds 2 bytes for the surrounding double quotes.
        let inner_len = PAYLOAD_MAX_BYTES - 2;
        let s: String = "x".repeat(inner_len);
        let v = serde_json::Value::String(s);
        // Sanity: serialised length is exactly the bound.
        assert_eq!(serde_json::to_vec(&v).unwrap().len(), PAYLOAD_MAX_BYTES);
        let out = truncate_payload(v.clone());
        assert_eq!(out, v, "boundary is inclusive: == max must not truncate");
    }

    /// One byte over the threshold must be truncated. The envelope
    /// shape (`_truncated: true` + `sha256` + `len`) is the wire
    /// contract the JSONL mirror relies on; a downstream parser will
    /// notice if any of these keys go missing.
    #[test]
    fn over_threshold_payload_is_replaced_with_envelope() {
        let s: String = "y".repeat(PAYLOAD_MAX_BYTES);
        let v = serde_json::Value::String(s);
        let original_len = serde_json::to_vec(&v).unwrap().len();
        assert!(original_len > PAYLOAD_MAX_BYTES);

        let out = truncate_payload(v);
        let obj = out.as_object().expect("envelope must be a JSON object");
        assert_eq!(obj.get("_truncated"), Some(&serde_json::Value::Bool(true)));
        assert_eq!(
            obj.get("len").and_then(|v| v.as_i64()),
            Some(original_len as i64)
        );
        let sha = obj
            .get("sha256")
            .and_then(|v| v.as_str())
            .expect("sha256 must be a string");
        assert_eq!(sha.len(), 64, "sha256 hex must be 64 chars");
        assert!(
            sha.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "sha256 must be lowercase hex: got {sha}"
        );
    }

    /// Producer↔predicate round-trip: whatever `truncate_payload` emits,
    /// `is_truncation_envelope` must recognize — and an untruncated payload
    /// must NOT be recognized. Cross-crate readers (core's observation
    /// capture, #62) key off this predicate, so a shape change that breaks
    /// the pairing must fail here, in the file that owns both sides.
    #[test]
    fn is_truncation_envelope_round_trips_producer() {
        let big = serde_json::Value::String("z".repeat(PAYLOAD_MAX_BYTES + 1));
        assert!(is_truncation_envelope(&truncate_payload(big)));

        let small = serde_json::json!({"plan": {"steps": []}});
        assert!(!is_truncation_envelope(&truncate_payload(small.clone())));
        assert!(!is_truncation_envelope(&small));
        // A non-boolean or absent marker is not an envelope.
        assert!(!is_truncation_envelope(&serde_json::json!({"_truncated": "yes"})));
    }

    /// Same input → same fingerprint. This is what makes truncated
    /// rows comparable: two operator queries that returned the same
    /// big body show the same `sha256`, even though the body itself
    /// is gone. Regression guard against accidentally salting the
    /// hash.
    #[test]
    fn truncate_is_deterministic_for_same_input() {
        let s = "z".repeat(PAYLOAD_MAX_BYTES + 100);
        let v1 = serde_json::Value::String(s.clone());
        let v2 = serde_json::Value::String(s);
        let a = truncate_payload(v1);
        let b = truncate_payload(v2);
        assert_eq!(a, b);
    }

    /// A `guard` decision record survives truncation.
    ///
    /// Found live on the DGX, 2026-08-23: two `web.fetch` rows whose
    /// payloads serialised to 85,352 and 85,351 bytes were stored as bare
    /// fingerprint envelopes, so the guard-tier score the dispatcher had
    /// just computed was gone. The tool payload is
    /// `{req, result, ms, guard}` and `result` carries the whole tool
    /// output, so any result past ~4 KiB took the `guard` object down with
    /// it.
    ///
    /// **The bias runs the wrong way.** A *blocked* dispatch usually keeps
    /// its score, because the result was already replaced by a short
    /// withheld placeholder — `req` is still in the payload, so a block on a
    /// multi-KiB `shell.exec` argv could lose one too, but not as a function
    /// of document size. A *cleared* dispatch loses its score as soon as the
    /// document is large. Recording `p` on the cleared half is the whole of the wiring
    /// spec's D5 — it is what makes production a score source that is not
    /// catalogue-selected — so what survived was every block plus only the
    /// small clears: a size-selected sample that reads like data.
    #[test]
    fn truncation_preserves_the_guard_decision_record() {
        let guard = serde_json::json!({
            "state": "clear",
            "p": 0.0074157947,
            "tau": 0.79552656,
            "ms": 75,
            "body_byte_len": 285,
            "truncated": false,
        });
        let v = serde_json::json!({
            "req": {"argv": ["/usr/bin/printf", "x"]},
            "result": {"text": "w".repeat(PAYLOAD_MAX_BYTES)},
            "ms": 12,
            "guard": guard.clone(),
        });
        assert!(serde_json::to_vec(&v).unwrap().len() > PAYLOAD_MAX_BYTES);

        let out = truncate_payload(v);
        assert!(
            is_truncation_envelope(&out),
            "the row must still declare itself truncated: {out}"
        );
        assert_eq!(
            out.get("guard"),
            Some(&guard),
            "the guard score exists nowhere else -- unlike `req` and `result`, \
             it cannot be recovered from the worker or from the JSONL mirror"
        );
        // The data is still gone: preserving a decision record is not
        // preserving the document it was about.
        assert!(out.get("result").is_none(), "the oversized data must NOT be kept");
        assert!(out.get("req").is_none(), "only the allowlisted keys ride along");
    }

    /// The fingerprint describes the ORIGINAL payload, not the envelope.
    ///
    /// Preserving a key must not change what `sha256`/`len` mean, or two
    /// rows for the same body would stop comparing equal the moment one of
    /// them carried a guard score.
    #[test]
    fn a_preserved_key_does_not_change_the_fingerprint() {
        let big = serde_json::json!({"result": "q".repeat(PAYLOAD_MAX_BYTES)});
        let mut with_guard = big.clone();
        with_guard["guard"] = serde_json::json!({"state": "clear", "p": 0.5});

        let bare = truncate_payload(big.clone());
        let kept = truncate_payload(with_guard.clone());

        // Each envelope fingerprints its OWN input...
        let expect = |v: &serde_json::Value| {
            use sha2::Digest;
            let bytes = serde_json::to_vec(v).unwrap();
            (format!("{:x}", sha2::Sha256::digest(&bytes)), bytes.len())
        };
        for (env, src) in [(&bare, &big), (&kept, &with_guard)] {
            let (sha, len) = expect(src);
            assert_eq!(env.get("sha256").and_then(|v| v.as_str()), Some(sha.as_str()));
            assert_eq!(env.get("len").and_then(|v| v.as_u64()), Some(len as u64));
        }
    }

    /// A payload with no preserved key keeps the exact three-key envelope.
    ///
    /// Pins the shape existing readers were written against, so preserving
    /// a key stays strictly additive.
    #[test]
    fn a_payload_without_a_preserved_key_keeps_the_bare_envelope() {
        let v = serde_json::json!({"plan": {"steps": ["x".repeat(PAYLOAD_MAX_BYTES)]}});
        let out = truncate_payload(v);
        let obj = out.as_object().expect("envelope is an object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["_truncated", "len", "sha256"]);
    }

    /// An oversized preserved key is dropped — and SAID to be dropped.
    ///
    /// The allowlisted keys are bounded *by construction* at every site
    /// this crate knows about, but `truncate_payload` is public and takes
    /// an arbitrary `Value`. Its one hard postcondition is that the return
    /// value fits the budget; a preserved key must never be able to break
    /// that, and silently storing an over-budget row is exactly the failure
    /// the cap exists to prevent.
    ///
    /// But dropping it *silently* would be the other failure the cap
    /// exists to prevent: an envelope with no `guard` and no explanation is
    /// byte-identical to one whose dispatch never ran a guard tier at all,
    /// which is the ambiguity that let the original defect hide. So the
    /// budget wins, and the loss is recorded rather than merely suffered.
    #[test]
    fn an_oversized_preserved_key_is_dropped_but_named() {
        let v = serde_json::json!({
            "guard": {"state": "clear", "junk": "!".repeat(PAYLOAD_MAX_BYTES)},
        });
        let out = truncate_payload(v);
        assert!(is_truncation_envelope(&out));
        assert!(
            out.get("guard").is_none(),
            "a preserved key that does not fit is dropped, not stored over budget"
        );
        assert_eq!(
            out.get(DROPPED_PRESERVED_KEY),
            Some(&serde_json::json!(["guard"])),
            "and a reader must be able to tell this from a dispatch that never ran the tier"
        );
        assert!(serde_json::to_vec(&out).unwrap().len() <= PAYLOAD_MAX_BYTES);
    }

    /// A preserved key that fits leaves NO drop marker.
    ///
    /// The marker means "something was lost". If it appeared on the happy
    /// path it would mean nothing at all, and the assertion above would be
    /// pinning noise rather than a signal.
    #[test]
    fn a_preserved_key_that_fits_leaves_no_drop_marker() {
        let v = serde_json::json!({
            "result": "z".repeat(PAYLOAD_MAX_BYTES),
            "guard": {"state": "clear", "p": 0.5},
        });
        let out = truncate_payload(v);
        assert!(out.get("guard").is_some(), "it fits, so it rides");
        assert!(out.get(DROPPED_PRESERVED_KEY).is_none(), "nothing was lost: {out}");
    }

    /// A preserved key is copied VERBATIM, whatever its value.
    ///
    /// `truncate_payload` allowlists *keys*, not shapes — it has no opinion
    /// about what a decision record should look like, and acquiring one
    /// would make it a second, silent validator of every producer. `null` is
    /// the case that matters in practice: `guard.p` is an `Option<f32>` and
    /// serialises to `null` on the unadjudicated arm, so a reader already
    /// has to handle it.
    #[test]
    fn a_preserved_key_is_copied_verbatim_including_null() {
        let big = "y".repeat(PAYLOAD_MAX_BYTES);
        for value in [
            serde_json::Value::Null,
            serde_json::json!(7),
            serde_json::json!("router_error"),
            serde_json::json!({"state": "unmeasured", "p": null}),
        ] {
            let out = truncate_payload(serde_json::json!({"result": big, "guard": value}));
            assert_eq!(out.get("guard"), Some(&value), "copied as-is, not normalised");
            assert!(out.get(DROPPED_PRESERVED_KEY).is_none());
        }
    }

    /// The admission boundary is exact, and it reserves the marker's room.
    ///
    /// `truncate_payload` admits a key when the grown envelope plus
    /// [`DROP_MARKER_RESERVE`] fits. Rather than hardcode the threshold —
    /// which would be a second implementation of the thing under test, and
    /// wrong the moment the `len` digit count changes — this walks a range
    /// that straddles it and asserts the invariant on whichever side each
    /// size lands, plus that the range really did contain both.
    ///
    /// **What that does and does not catch.** Deleting the reserve turns
    /// this red at the first admitted size with less than
    /// [`DROP_MARKER_RESERVE`] bytes free. Reversing the comparator turns
    /// it red via the budget assertion. Nudging `<=` to `<` does **not**:
    /// the boundary moves by one and every assertion still holds on
    /// whichever side each size lands. That is deliberate — a one-byte
    /// shift in the conservative direction is not a defect worth pinning,
    /// and pinning it would mean hardcoding the threshold this test exists
    /// to avoid hardcoding.
    #[test]
    fn admission_reserves_room_for_the_drop_marker_on_both_sides() {
        let big = "r".repeat(PAYLOAD_MAX_BYTES);
        let (mut saw_admitted, mut saw_dropped) = (false, false);
        // The bare envelope is ~110 bytes, so the boundary sits just under
        // `PAYLOAD_MAX_BYTES - DROP_MARKER_RESERVE`. Straddle it widely
        // enough that the window cannot drift off the transition.
        let lo = PAYLOAD_MAX_BYTES.saturating_sub(DROP_MARKER_RESERVE).saturating_sub(300);
        for n in lo..(lo + 400) {
            let v = serde_json::json!({"result": big, "guard": "!".repeat(n)});
            let out = truncate_payload(v);
            let len = serde_json::to_vec(&out).expect("serialises").len();
            assert!(len <= PAYLOAD_MAX_BYTES, "n={n} produced {len} bytes");
            if out.get("guard").is_some() {
                saw_admitted = true;
                assert!(
                    out.get(DROPPED_PRESERVED_KEY).is_none(),
                    "n={n}: admitted and yet marked dropped"
                );
                assert!(
                    len + DROP_MARKER_RESERVE <= PAYLOAD_MAX_BYTES,
                    "n={n}: admitted with only {} bytes free, less than the reserve",
                    PAYLOAD_MAX_BYTES - len
                );
            } else {
                saw_dropped = true;
                assert_eq!(
                    out.get(DROPPED_PRESERVED_KEY),
                    Some(&serde_json::json!(["guard"])),
                    "n={n}: dropped without saying so"
                );
            }
        }
        assert!(saw_admitted && saw_dropped, "the window must straddle the boundary");
    }

    /// Every envelope this function can return is within budget.
    ///
    /// The postcondition stated in the doc comment, asserted over both
    /// arms rather than argued for in prose.
    #[test]
    fn every_envelope_fits_the_budget() {
        let cases = [
            serde_json::json!({"result": "a".repeat(PAYLOAD_MAX_BYTES)}),
            serde_json::json!({
                "result": "b".repeat(PAYLOAD_MAX_BYTES),
                "guard": {"state": "flagged", "p": 0.92, "tau": 0.79552656},
            }),
            serde_json::json!({"guard": {"x": "c".repeat(PAYLOAD_MAX_BYTES)}}),
            serde_json::Value::String("d".repeat(PAYLOAD_MAX_BYTES)),
        ];
        for v in cases {
            let out = truncate_payload(v.clone());
            let n = serde_json::to_vec(&out).unwrap().len();
            // NOT `{v:.40}`: serde_json's `Display` streams straight through
            // `Formatter::write_str` and never consults `precision`, so the
            // width is silently ignored and a failure would dump the whole
            // 4 KiB fixture into the test output.
            let head: String = v.to_string().chars().take(40).collect();
            assert!(n <= PAYLOAD_MAX_BYTES, "envelope of {head} is {n} bytes");
        }
    }

    // ── The multi-key half of `preserve_onto`. ────────────────────────
    //
    // `PRESERVED_KEYS` has one member, so every property below is
    // unreachable through `truncate_payload` today. They are the
    // properties its doc comment claims, and the mutation that breaks
    // them -- measuring each candidate against a FIXED envelope instead
    // of the running one -- is invisible at N=1 and produces an
    // over-budget row at N=2. Tested through the parameterised helper so
    // they are executed rather than argued.

    /// Build the bare envelope the way `truncate_payload` does, so these
    /// tests measure the same starting size production does.
    fn bare_envelope() -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        m.insert(TRUNCATED_MARKER_KEY.to_string(), serde_json::Value::Bool(true));
        m.insert(FINGERPRINT_SHA256_KEY.to_string(), serde_json::json!("ab".repeat(32)));
        m.insert(FINGERPRINT_LEN_KEY.to_string(), serde_json::json!(85_352));
        m
    }

    fn source(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect()
    }

    /// Two bounded keys both ride, and neither is named as dropped.
    #[test]
    fn preserve_onto_admits_every_key_that_fits() {
        let src = source(&[
            ("alpha", serde_json::json!({"state": "clear"})),
            ("beta", serde_json::json!(7)),
        ]);
        let out = preserve_onto(bare_envelope(), &src, &["alpha", "beta"]);
        assert_eq!(out.get("alpha"), Some(&serde_json::json!({"state": "clear"})));
        assert_eq!(out.get("beta"), Some(&serde_json::json!(7)));
        assert!(out.get(DROPPED_PRESERVED_KEY).is_none(), "nothing was lost: {out}");
    }

    /// **One oversized key must not take a bounded sibling down with it.**
    ///
    /// This is `truncate_payload`'s headline claim for admitting keys one
    /// at a time rather than all-or-nothing, and the only test that can
    /// reach it. An all-or-nothing copy would drop `beta` too; a copy that
    /// measured against a fixed `bare` would admit `alpha` and blow the
    /// budget.
    #[test]
    fn preserve_onto_drops_only_the_key_that_does_not_fit() {
        let src = source(&[
            ("alpha", serde_json::json!("!".repeat(PAYLOAD_MAX_BYTES))),
            ("beta", serde_json::json!({"state": "clear", "p": 0.5})),
        ]);
        let out = preserve_onto(bare_envelope(), &src, &["alpha", "beta"]);
        assert!(out.get("alpha").is_none(), "the oversized key must not be stored");
        assert_eq!(
            out.get("beta"),
            Some(&serde_json::json!({"state": "clear", "p": 0.5})),
            "a bounded sibling must survive an oversized key"
        );
        assert_eq!(out.get(DROPPED_PRESERVED_KEY), Some(&serde_json::json!(["alpha"])));
        assert!(serde_json::to_vec(&out).unwrap().len() <= PAYLOAD_MAX_BYTES);
    }

    /// The drop marker names **every** key it lost, not just the first.
    #[test]
    fn preserve_onto_names_all_dropped_keys() {
        let huge = serde_json::json!("!".repeat(PAYLOAD_MAX_BYTES));
        let src = source(&[("alpha", huge.clone()), ("beta", huge)]);
        let out = preserve_onto(bare_envelope(), &src, &["alpha", "beta"]);
        assert_eq!(out.get(DROPPED_PRESERVED_KEY), Some(&serde_json::json!(["alpha", "beta"])));
        assert!(serde_json::to_vec(&out).unwrap().len() <= PAYLOAD_MAX_BYTES);
    }

    /// **Each candidate is measured against the RUNNING envelope.**
    ///
    /// The mutation this pins: `serde_json::to_vec(&envelope)` measuring a
    /// fixed starting envelope instead of the one already carrying
    /// `alpha`. Two keys that each fit alone but not together would then
    /// both be admitted and the row would land at roughly twice the cap --
    /// breaking the one postcondition `truncate_payload` guarantees, on a
    /// path no production caller can reach until `PRESERVED_KEYS` gains a
    /// second member.
    #[test]
    fn preserve_onto_measures_against_the_running_envelope() {
        // Each ~60% of the budget: either fits alone, together they cannot.
        let half = serde_json::json!("z".repeat(PAYLOAD_MAX_BYTES * 6 / 10));
        let src = source(&[("alpha", half.clone()), ("beta", half)]);

        // Sanity: each really does fit on its own, or the test proves nothing.
        for k in ["alpha", "beta"] {
            let solo = preserve_onto(bare_envelope(), &src, &[k]);
            assert!(solo.get(k).is_some(), "{k} must fit alone for this test to mean anything");
        }

        let out = preserve_onto(bare_envelope(), &src, &["alpha", "beta"]);
        let n = serde_json::to_vec(&out).unwrap().len();
        assert!(n <= PAYLOAD_MAX_BYTES, "two keys that fit alone produced {n} bytes together");
        assert_eq!(out.get(DROPPED_PRESERVED_KEY), Some(&serde_json::json!(["beta"])));
    }

    /// A key absent from `source` is skipped silently and is NOT a drop.
    ///
    /// The marker means "the control ran and its verdict was discarded".
    /// Reporting a key that was never there would make it mean nothing.
    #[test]
    fn preserve_onto_ignores_a_key_the_payload_never_carried() {
        let src = source(&[("alpha", serde_json::json!(1))]);
        let out = preserve_onto(bare_envelope(), &src, &["alpha", "absent"]);
        assert_eq!(out.get("alpha"), Some(&serde_json::json!(1)));
        assert!(out.get("absent").is_none());
        assert!(out.get(DROPPED_PRESERVED_KEY).is_none(), "absence is not loss: {out}");
    }

    /// A refused key leaves the envelope byte-for-byte as it found it.
    ///
    /// `preserve_onto` measures by inserting and then taking back out. If
    /// the take-back were wrong -- or clobbered a value it displaced --
    /// the fingerprint could be corrupted by a key that was not even
    /// stored. Unreachable via `PRESERVED_KEYS` (the compile-time block
    /// forbids shadowing), reachable through the parameter.
    #[test]
    fn preserve_onto_restores_what_a_refused_key_displaced() {
        let src = source(&[(FINGERPRINT_LEN_KEY, serde_json::json!("!".repeat(PAYLOAD_MAX_BYTES)))]);
        let out = preserve_onto(bare_envelope(), &src, &[FINGERPRINT_LEN_KEY]);
        assert_eq!(
            out.get(FINGERPRINT_LEN_KEY),
            Some(&serde_json::json!(85_352)),
            "a refused key must not damage the value it displaced"
        );
        assert_eq!(out.get(DROPPED_PRESERVED_KEY), Some(&serde_json::json!([FINGERPRINT_LEN_KEY])));
    }

    /// Only TOP-LEVEL keys are promoted.
    ///
    /// A recursive key search would look like a helpful refactor and would
    /// promote arbitrary nested data past the cap under a preserved name,
    /// defeating admission criterion 2 ("a decision record, not data").
    #[test]
    fn a_nested_preserved_key_is_not_promoted() {
        let v = serde_json::json!({
            "result": {(GUARD_KEY): {"state": "clear"}, "body": "q".repeat(PAYLOAD_MAX_BYTES)},
        });
        let out = truncate_payload(v);
        assert!(is_truncation_envelope(&out));
        assert!(out.get(GUARD_KEY).is_none(), "only a top-level key is a decision record");
        assert!(out.get(DROPPED_PRESERVED_KEY).is_none(), "it was never there to lose: {out}");
    }

    // ── The wire contract and the machinery that enforces it. ─────────

    /// The marker's literal value is the wire contract, not the constant.
    ///
    /// Every other reference to it in this crate and in `core` is
    /// symbolic, so renaming the string would break every downstream `jq`
    /// and SQL forensic query with a fully green suite. Same reason
    /// `envelope_shape_is_pinned` spells `_truncated` out.
    #[test]
    fn wire_key_literals_are_pinned() {
        assert_eq!(DROPPED_PRESERVED_KEY, "_dropped_preserved");
        assert_eq!(GUARD_KEY, "guard");
        assert_eq!(PRESERVED_KEYS, ["guard"].as_slice());
    }

    /// `str_eq` is the sole enforcer of the compile-time shadow guard.
    ///
    /// Mutate it to always return `false` and all of those assertions
    /// become no-ops -- the guard silently stops guarding, with nothing in
    /// the suite to notice. It is a hand-rolled comparator because
    /// `PartialEq` is not const; hand-rolled comparators get tested.
    #[test]
    fn str_eq_matches_partial_eq() {
        for (a, b) in [
            ("guard", "guard"),
            ("guard", "guar"),
            ("guard", "guardx"),
            ("guard", "guarD"),
            ("", ""),
            ("", "x"),
            ("_truncated", "_dropped_preserved"),
        ] {
            assert_eq!(str_eq(a, b), a == b, "str_eq({a:?}, {b:?})");
        }
    }

    /// `drop_marker_worst_case()` really does bound the marker it describes.
    ///
    /// The const block checks it against [`DROP_MARKER_RESERVE`], not
    /// against reality -- so shrinking the estimate keeps compiling
    /// (30 <= 64) while quietly ceasing to be a worst case. This measures
    /// the real cost of the largest marker `PRESERVED_KEYS` can produce:
    /// every member dropped at once.
    #[test]
    fn drop_marker_worst_case_bounds_the_real_marker() {
        let mut env = bare_envelope();
        let before = serde_json::to_vec(&serde_json::Value::Object(env.clone())).unwrap().len();
        env.insert(DROPPED_PRESERVED_KEY.to_string(), serde_json::json!(PRESERVED_KEYS));
        let after = serde_json::to_vec(&serde_json::Value::Object(env)).unwrap().len();
        let actual = after - before;
        assert!(
            actual <= drop_marker_worst_case(),
            "the marker really costs {actual} bytes but the estimate is {}",
            drop_marker_worst_case()
        );
        assert!(drop_marker_worst_case() <= DROP_MARKER_RESERVE);
    }

    /// Different inputs at the same length must produce different
    /// fingerprints. Catches a silly mistake like hashing the *length*
    /// instead of the bytes.
    #[test]
    fn truncate_fingerprint_distinguishes_different_payloads() {
        let a = serde_json::Value::String("a".repeat(PAYLOAD_MAX_BYTES + 50));
        let b = serde_json::Value::String("b".repeat(PAYLOAD_MAX_BYTES + 50));
        let oa = truncate_payload(a);
        let ob = truncate_payload(b);
        assert_ne!(
            oa.get("sha256"),
            ob.get("sha256"),
            "different bodies must produce different SHA-256s"
        );
    }
}
