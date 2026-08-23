//! Append-only audit-log writes and reads.
//!
//! ## Where rows come from
//!
//! Today exactly two callers write into `audit_log`:
//!
//!   1. [`crate::probe::run`] — the daemon's bring-up row, written
//!      under [`crate::conn::RUNTIME_ROLE`] right after migrations.
//!   2. `core::tool_host::dispatch` (Phase 0 Option I) — one row per
//!      tool call, again under the runtime role via the
//!      `after_connect` SET ROLE hook on
//!      [`crate::pool::connect_runtime_pool`].
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
/// 3. **irrecoverable** — it exists nowhere else. `req` and `result` can be
///    reconstructed from the worker and the surrounding rows; a guard score
///    is computed once, in memory, and is gone if this row drops it.
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
pub const PRESERVED_KEYS: [&str; 1] = ["guard"];

/// Payload key that marks a [`truncate_payload`] envelope. This is a **wire
/// contract**: readers in other crates (e.g. `kastellan-core`'s observation
/// capture, issue #62) detect truncation via [`is_truncation_envelope`], so
/// the writer and every reader share this single definition rather than
/// re-spelling the literal.
pub const TRUNCATED_MARKER_KEY: &str = "_truncated";

/// True iff `payload` is a truncation envelope produced by
/// [`truncate_payload`] — i.e. the original payload was over budget and
/// its keys were replaced by the `{_truncated, sha256, len}` fingerprint,
/// except any listed in [`PRESERVED_KEYS`], which ride along unchanged.
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
/// envelope verbatim, because a bounded decision record is not what the
/// cap is defending against and is worth more than the bytes it costs.
/// The return value is always within budget: if the preserved keys would
/// break that — which nothing in this workspace can do, but the signature
/// permits — the bare envelope is returned instead.
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

    let bare = serde_json::json!({
        (TRUNCATED_MARKER_KEY): true,
        "sha256": hex,
        "len": bytes.len(),
    });

    // Only an object can have keys to preserve; a bare string or array
    // over the cap is all data by definition.
    let Some(source) = payload.as_object() else {
        return bare;
    };

    let mut envelope = bare.clone();
    let keep = envelope.as_object_mut().expect("built from a JSON object literal above");
    for key in PRESERVED_KEYS {
        if let Some(value) = source.get(key) {
            keep.insert(key.to_string(), value.clone());
        }
    }

    // The postcondition, enforced rather than argued: whatever a caller put
    // under a preserved key, the stored row fits the budget.
    let grown = serde_json::to_vec(&envelope).expect("serde_json::Value cannot fail to serialise");
    if grown.len() > PAYLOAD_MAX_BYTES {
        return bare;
    }
    envelope
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
    #[test]
    fn small_payload_is_not_truncated() {
        let v = serde_json::json!({"actor": "core", "ms": 12});
        let out = truncate_payload(v.clone());
        assert_eq!(out, v);
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
    /// **The bias runs the wrong way.** A *blocked* dispatch keeps its
    /// score, because the result was already replaced by a short withheld
    /// placeholder; a *cleared* one loses it as soon as the document is
    /// large. Recording `p` on the cleared half is the whole of the wiring
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

    /// An oversized preserved key falls back to the bare envelope.
    ///
    /// The allowlisted keys are bounded *by construction* at every site
    /// this crate knows about, but `truncate_payload` is public and takes
    /// an arbitrary `Value`. Its one hard postcondition is that the return
    /// value fits the budget; a preserved key must never be able to break
    /// that, and silently storing an over-budget row is exactly the failure
    /// the cap exists to prevent.
    #[test]
    fn an_oversized_preserved_key_falls_back_to_the_bare_envelope() {
        let v = serde_json::json!({
            "guard": {"state": "clear", "junk": "!".repeat(PAYLOAD_MAX_BYTES)},
        });
        let out = truncate_payload(v);
        assert!(is_truncation_envelope(&out));
        assert!(
            out.get("guard").is_none(),
            "a preserved key that does not fit is dropped, not stored over budget"
        );
        assert!(serde_json::to_vec(&out).unwrap().len() <= PAYLOAD_MAX_BYTES);
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
            assert!(n <= PAYLOAD_MAX_BYTES, "envelope of {v:.40} is {n} bytes");
        }
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
