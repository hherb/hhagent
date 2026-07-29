# Email fallback channel — slice 1 (gated inbound) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A paired user can email the agent and have it become a normal channel task, gated by DMARC-pass plus a per-pairing in-body token, with every rejection audited. No replies yet (slice 2).

**Architecture:** A sandboxed `email-in` worker polls localmail's `/v1` subscription and returns raw material only. Core owns every security decision as pure functions (`channel/email/gate.rs`), the bus enforces them through the existing `PeerAuthorizer` chokepoint, and the channel-generic `PolledWorkerDriver` supplies poll/ack plumbing. The inbound cursor lives in localmail, so kastellan holds no inbound position state.

**Tech Stack:** Rust (kastellan workspace, rustc 1.96.0), Python 3 + FastAPI + psycopg (localmail), Postgres, sqlx migrations, JSON-RPC 2.0 over stdio.

**Spec:** `docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`

## Global Constraints

- **AGPL-3.0 project; AGPL-compatible dependencies only.** No CDDL, BUSL, SSPL, Elastic, or "source-available" deps. Slice 1 adds **no new Rust dependency** — everything reuses existing workspace crates.
- **Cross-platform: Linux + macOS first-class.** No `#[cfg(target_os)]` code in this slice; if one appears, it needs a counterpart of equivalent guarantee.
- **Every worker is sandboxed before it runs.** No unsandboxed spawn path.
- **Rust core, Python only inside sandboxed workers.** No PyO3, no in-process Python.
- **Security decisions are pure and live in `core`.** The worker returns raw material and makes no decisions (spec D6).
- **Files stay under 500 LOC** where feasible; split by responsibility.
- **Fail closed.** Any gate that cannot be evaluated rejects.
- **Unset config ⇒ byte-identical behaviour.** With no email config the daemon must behave exactly as today.
- **Matrix parity is a test-pinned invariant.** A pairing row with `token_sha256 IS NULL` ignores evidence entirely.
- Run cargo in the **foreground**, never backgrounded. Source the toolchain first: `source "$HOME/.cargo/env"`.
- Stage **specific files** in every commit (`git add <paths>`), never `git add -A`.

---

### Task 1: localmail subscription cursor (DIFFERENT REPO: `~/src/localmail`)

This task is entirely in the localmail repo and gets its own PR there. It defines the contract Task 7 consumes and Task 9 mocks, so it goes first.

**Files:**
- Create: `migrations/0032_channel_subscriptions.sql`
- Modify: `src/localmail/serve/routes/changes.py`
- Test: `tests/test_serve_changes_route.py`

**Interfaces:**
- Consumes: nothing.
- Produces: `GET /v1/changes?subscription=<name>` → `{"new_messages": [...], "next_cursor": "<id>"}` returning only messages after the stored cursor; `POST /v1/changes/ack` with body `{"subscription": "<name>", "cursor": "<id>"}` → `204`, advancing the cursor. Cursor is monotonic — an ack with a cursor lower than the stored one is a no-op, never a rewind.

- [ ] **Step 1: Write the failing tests**

Append to `tests/test_serve_changes_route.py` (match the existing fixtures in that file for app/client/user setup):

```python
def test_subscription_returns_only_unacked_then_advances(client, seed_messages):
    """A named subscription is a server-side cursor: poll, ack, poll again."""
    ids = seed_messages(3)  # returns ascending message ids

    first = client.get("/v1/changes", params={"subscription": "kastellan"}).json()
    assert [m["message_id"] for m in first["new_messages"]] == [str(i) for i in ids]

    # Ack only the first message; the next poll must resume from there.
    r = client.post("/v1/changes/ack",
                    json={"subscription": "kastellan", "cursor": str(ids[0])})
    assert r.status_code == 204

    second = client.get("/v1/changes", params={"subscription": "kastellan"}).json()
    assert [m["message_id"] for m in second["new_messages"]] == [str(i) for i in ids[1:]]


def test_ack_never_rewinds(client, seed_messages):
    """A stale/replayed ack must not resurface already-acked messages."""
    ids = seed_messages(2)
    client.post("/v1/changes/ack", json={"subscription": "kastellan", "cursor": str(ids[1])})
    client.post("/v1/changes/ack", json={"subscription": "kastellan", "cursor": str(ids[0])})

    after = client.get("/v1/changes", params={"subscription": "kastellan"}).json()
    assert after["new_messages"] == []


def test_subscriptions_are_per_user_and_per_name(client, other_client, seed_messages):
    """One subscription's cursor must not affect another's."""
    ids = seed_messages(2)
    client.post("/v1/changes/ack", json={"subscription": "kastellan", "cursor": str(ids[1])})

    other_name = client.get("/v1/changes", params={"subscription": "gui"}).json()
    assert len(other_name["new_messages"]) == 2

    other_user = other_client.get("/v1/changes", params={"subscription": "kastellan"}).json()
    assert len(other_user["new_messages"]) == 2


def test_unknown_subscription_starts_from_tip_not_backlog(client, seed_messages):
    """A fresh subscription must NOT replay history as if it were new mail."""
    seed_messages(3)
    fresh = client.get("/v1/changes", params={"subscription": "brand-new"}).json()
    assert fresh["new_messages"] == []
    assert fresh["next_cursor"] != "0"
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd ~/src/localmail && uv run pytest tests/test_serve_changes_route.py -v`
Expected: FAIL — the `subscription` parameter is ignored and `/v1/changes/ack` returns 404/405.

- [ ] **Step 3: Write the migration**

Create `migrations/0032_channel_subscriptions.sql`:

```sql
-- Server-side polling cursors for named subscriptions (one per api-user +
-- name). Lets a polling client be stateless: poll, process, ack. Without this
-- a cursorless client re-reads the 200 most recent messages on every restart.
CREATE TABLE channel_subscriptions (
    id          BIGSERIAL   PRIMARY KEY,
    user_id     BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name        TEXT        NOT NULL,
    cursor      BIGINT      NOT NULL DEFAULT 0,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, name)
);
```

- [ ] **Step 4: Implement the endpoints**

In `src/localmail/serve/routes/changes.py`, add a `subscription` query parameter to `changes()` and a new ack route. Key rules, all load-bearing:

```python
_MAX_SUBSCRIPTION_NAME = 64


def _subscription_cursor(conn, user_id: int, name: str) -> int | None:
    """Stored cursor for (user, name); None when the subscription is new."""
    with conn.cursor() as cur:
        cur.execute(
            "SELECT cursor FROM channel_subscriptions WHERE user_id = %s AND name = %s",
            (user_id, name),
        )
        row = cur.fetchone()
    return None if row is None else int(row[0])


def _current_tip(conn, allowed: list[int]) -> int:
    """Highest visible message id — where a brand-new subscription starts."""
    with conn.cursor() as cur:
        cur.execute(
            "SELECT COALESCE(MAX(id), 0) FROM messages WHERE account_id = ANY(%s)",
            (allowed,),
        )
        return int(cur.fetchone()[0])
```

In `changes()`: when `subscription` is supplied, validate the name (non-empty,
`<= _MAX_SUBSCRIPTION_NAME`, `[A-Za-z0-9_-]+`), look up the cursor, and:

- cursor found ⇒ take the existing `since_id` branch with that value;
- cursor **not** found ⇒ create the row at `_current_tip(...)` and return
  `{"new_messages": [], "next_cursor": str(tip)}`. Starting a fresh
  subscription at the tip is what stops history being replayed as new mail.

`subscription` and `since` are mutually exclusive — passing both is a 400, so
a caller cannot accidentally mix server-side and client-side cursors.

Add the ack route in the same module:

```python
@router.post("/ack", status_code=204)
def ack(
    request: Request,
    payload: dict[str, Any],
    user=Depends(get_authenticated_user),
) -> Response:
    """Advance a subscription's cursor. Monotonic: never rewinds."""
    name = _validate_subscription_name(payload.get("subscription"))
    cursor = parse_int_id(str(payload.get("cursor")), field="cursor")
    pool = request.app.state.pool
    with pool.connection() as conn:
        with conn.cursor() as cur:
            # GREATEST keeps this monotonic, so a stale or replayed ack cannot
            # resurface already-processed messages.
            cur.execute(
                """INSERT INTO channel_subscriptions (user_id, name, cursor)
                        VALUES (%s, %s, %s)
                   ON CONFLICT (user_id, name) DO UPDATE
                        SET cursor = GREATEST(channel_subscriptions.cursor, EXCLUDED.cursor),
                            updated_at = now()""",
                (user.id, name, cursor),
            )
        conn.commit()
    return Response(status_code=204)
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd ~/src/localmail && uv run pytest tests/test_serve_changes_route.py -v`
Expected: PASS, all four new tests plus the pre-existing ones.

- [ ] **Step 6: Commit (in the localmail repo)**

```bash
cd ~/src/localmail
git add migrations/0032_channel_subscriptions.sql src/localmail/serve/routes/changes.py tests/test_serve_changes_route.py
git commit -m "feat(changes): server-side subscription cursors with ack

A named subscription per api-user makes a polling client stateless: poll,
process, ack. Without it a cursorless client re-reads the 200 most recent
messages on every restart, which for kastellan's email channel would mean
replaying old mail as new agent tasks.

A fresh subscription starts at the current tip, not at the backlog. Acks are
monotonic via GREATEST so a stale ack cannot resurface processed messages.
Still tail-only: no min_id/before backfill parameter is added."
```

---

### Task 2: `pairings.token_sha256` column + db helpers

**Files:**
- Create: `db/migrations/0022_pairing_token.sql`
- Modify: `db/src/pairings.rs`
- Test: `db/tests/postgres_e2e.rs` (append)

**Interfaces:**
- Consumes: existing `pairings` table (migration 0018).
- Produces:
  - `kastellan_db::pairings::insert_pairing_with_token(executor, channel, peer, method, token_sha256: Option<&str>) -> Result<i64, DbError>`
  - `kastellan_db::pairings::token_hash_for(executor, channel, peer) -> Result<Option<Option<String>>, DbError>` — outer `None` = no active pairing; inner `None` = paired but no token required.

- [ ] **Step 1: Write the failing test**

Append to `db/tests/postgres_e2e.rs`:

```rust
#[tokio::test]
async fn pairing_token_hash_round_trips_and_distinguishes_absent_from_null() {
    let Some(cluster) = bring_up_pg_cluster().await else { return };
    let pool = cluster.pool();

    // No pairing at all → outer None.
    let missing = kastellan_db::pairings::token_hash_for(&pool, "email", "nobody@example.org")
        .await
        .unwrap();
    assert_eq!(missing, None, "absent pairing must be distinguishable");

    // Paired WITHOUT a token (the Matrix shape) → Some(None).
    kastellan_db::pairings::insert_pairing_with_token(&pool, "matrix", "@me:srv", "code", None)
        .await
        .unwrap();
    let matrix = kastellan_db::pairings::token_hash_for(&pool, "matrix", "@me:srv").await.unwrap();
    assert_eq!(matrix, Some(None), "a NULL token must mean 'no token required'");

    // Paired WITH a token → Some(Some(hash)).
    kastellan_db::pairings::insert_pairing_with_token(
        &pool, "email", "me@example.org", "operator", Some("abc123"),
    )
    .await
    .unwrap();
    let email = kastellan_db::pairings::token_hash_for(&pool, "email", "me@example.org")
        .await
        .unwrap();
    assert_eq!(email, Some(Some("abc123".to_string())));

    // Revoking must make the token invisible, not merely inert.
    kastellan_db::pairings::revoke_pairing(&pool, "email", "me@example.org").await.unwrap();
    let revoked = kastellan_db::pairings::token_hash_for(&pool, "email", "me@example.org")
        .await
        .unwrap();
    assert_eq!(revoked, None, "a revoked pairing must not surface its token");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-db --test postgres_e2e pairing_token_hash -- --nocapture`
Expected: FAIL to compile — `insert_pairing_with_token` and `token_hash_for` do not exist.

- [ ] **Step 3: Write the migration and helpers**

Create `db/migrations/0022_pairing_token.sql`:

```sql
-- Per-pairing long-lived shared secret for transports that cannot authenticate
-- a sender themselves (email). Hash only, never plaintext. NULL means "this
-- pairing needs no token", which is every pre-existing row — so Matrix
-- behaviour is unchanged.
ALTER TABLE pairings ADD COLUMN token_sha256 TEXT;
```

Add to `db/src/pairings.rs`:

```rust
/// Insert a pairing directly (operator action — no in-channel handshake), with
/// an optional long-lived token hash. `token_sha256` is `None` for transports
/// that authenticate their own peers (Matrix); `Some(hash)` for email, where
/// the sender must present the plaintext in every message.
pub async fn insert_pairing_with_token<'e, E>(
    executor: E,
    channel: &str,
    peer: &str,
    method: &str,
    token_sha256: Option<&str>,
) -> Result<i64, DbError>
where
    E: sqlx::PgExecutor<'e>,
{
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO pairings (channel, peer, method, token_sha256)
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(channel)
    .bind(peer)
    .bind(method)
    .bind(token_sha256)
    .fetch_one(executor)
    .await?;
    Ok(id)
}

/// Token requirement for an ACTIVE pairing.
///
/// Three-state on purpose, and the caller must not collapse it:
/// * `None` — no active pairing (revoked rows included). Not authorized.
/// * `Some(None)` — paired, no token required (Matrix).
/// * `Some(Some(hash))` — paired, and the sender must present this token.
pub async fn token_hash_for<'e, E>(
    executor: E,
    channel: &str,
    peer: &str,
) -> Result<Option<Option<String>>, DbError>
where
    E: sqlx::PgExecutor<'e>,
{
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT token_sha256 FROM pairings
          WHERE channel = $1 AND peer = $2 AND revoked_at IS NULL",
    )
    .bind(channel)
    .bind(peer)
    .fetch_optional(executor)
    .await?;
    Ok(row.map(|(h,)| h))
}
```

- [ ] **Step 4: Rebuild so the migration is embedded, then run the test**

`sqlx::migrate!` embeds migrations at **compile** time, so a new `.sql` does not apply until the crate is rebuilt.

Run: `source "$HOME/.cargo/env" && touch db/src/lib.rs && cargo test -p kastellan-db --test postgres_e2e pairing_token_hash -- --nocapture`
Expected: PASS (or skip-as-pass with a `[SKIP]` line if no PG is available — in that case run it on the DGX before merging).

- [ ] **Step 5: Commit**

```bash
git add db/migrations/0022_pairing_token.sql db/src/pairings.rs db/tests/postgres_e2e.rs
git commit -m "feat(db): optional per-pairing token hash (migration 0022)

Nullable token_sha256 on pairings, plus insert_pairing_with_token and the
three-state token_hash_for. NULL means 'no token required', so every existing
Matrix row is unaffected. The three states (no pairing / paired without token /
paired with token) are deliberately not collapsed: conflating the first two
would admit an unpaired peer."
```

---

### Task 3: `kastellan-cli pair issue-token`

**Files:**
- Modify: `core/src/bin/kastellan-cli/pair.rs`
- Test: inline `#[cfg(test)] mod tests` in the same file

**Interfaces:**
- Consumes: `kastellan_db::pairings::insert_pairing_with_token` (Task 2), `kastellan_core::channel::ingest::sha256_hex`.
- Produces: CLI `kastellan-cli pair issue-token --channel <ch> --peer <peer>`, printing the plaintext token exactly once.

Deliberately a **separate subcommand**, not flags on `pair issue`: `issue` mints a single-use code for an in-channel handshake, while this creates a pairing outright with a long-lived secret. Same command, two meanings, would be a footgun.

- [ ] **Step 1: Write the failing test**

Append to the `mod tests` block in `core/src/bin/kastellan-cli/pair.rs`:

```rust
#[test]
fn parse_issue_token_requires_channel_and_peer() {
    assert!(parse_issue_token_args(&[]).is_err());
    assert!(parse_issue_token_args(&["--channel".into(), "email".into()]).is_err());
    assert!(parse_issue_token_args(&["--peer".into(), "me@example.org".into()]).is_err());
    assert!(parse_issue_token_args(&["--channel".into()]).is_err());
}

#[test]
fn parse_issue_token_accepts_channel_and_peer() {
    let args = vec![
        "--channel".to_string(), "email".to_string(),
        "--peer".to_string(), "Me@Example.ORG".to_string(),
    ];
    let (channel, peer) = parse_issue_token_args(&args).unwrap();
    assert_eq!(channel, "email");
    // Normalized at mint time so it matches the peer the channel derives from
    // a From header, which is lowercased there.
    assert_eq!(peer, "me@example.org");
}

#[test]
fn generated_tokens_are_hex_and_unique() {
    let a = generate_code();
    let b = generate_code();
    assert_eq!(a.len(), CODE_BYTES * 2);
    assert_ne!(a, b);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --bin kastellan-cli parse_issue_token -- --nocapture`
Expected: FAIL to compile — `parse_issue_token_args` does not exist.

- [ ] **Step 3: Implement**

In `core/src/bin/kastellan-cli/pair.rs`, add the dispatch arm in `run()`:

```rust
"issue-token" => with_runtime("pair issue-token", pair_issue_token(&args[1..])),
```

and update the usage line to `pair <issue|issue-token|list|revoke>`. Then:

```rust
/// Parse `pair issue-token --channel <ch> --peer <peer>`. Both are required:
/// a token is meaningless without the pairing it belongs to.
fn parse_issue_token_args(args: &[String]) -> Result<(String, String), String> {
    let mut channel: Option<String> = None;
    let mut peer: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--channel" => {
                channel = Some(args.get(i + 1).ok_or("--channel requires a value")?.clone());
                i += 2;
            }
            "--peer" => {
                peer = Some(args.get(i + 1).ok_or("--peer requires a value")?.clone());
                i += 2;
            }
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    let channel = channel.ok_or("--channel is required")?;
    // Lowercased to match the channel's own normalization of a From address;
    // a case-mismatched pairing row would silently never authorize.
    let peer = peer.ok_or("--peer is required")?.trim().to_ascii_lowercase();
    if channel.trim().is_empty() || peer.is_empty() {
        return Err("--channel and --peer must be non-empty".to_string());
    }
    Ok((channel, peer))
}

async fn pair_issue_token(args: &[String]) -> ExitCode {
    use kastellan_db::pool::connect_admin_pool;

    let (channel, peer) = match parse_issue_token_args(args) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("{msg}\nusage: kastellan-cli pair issue-token --channel <ch> --peer <peer>");
            return ExitCode::from(2);
        }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_admin_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    let token = generate_code();
    let hash = kastellan_core::channel::ingest::sha256_hex(token.as_bytes());

    let id = match kastellan_db::pairings::insert_pairing_with_token(
        &pool, &channel, &peer, "operator", Some(&hash),
    ).await {
        Ok(id) => id,
        Err(e) => { eprintln!("pair issue-token: {e}"); return ExitCode::from(1); }
    };

    // Audit: hash only — NEVER the plaintext token.
    let _ = kastellan_db::audit::insert(
        &pool,
        "cli",
        "pairing.token_issued",
        serde_json::json!({"id": id, "channel": channel, "peer": peer, "token_sha256": hash}),
    )
    .await;

    println!("Paired {channel}/{peer}. Token (shown once):\n");
    println!("    {token}\n");
    println!("Include this token in the body of every message you send from that address.");
    println!("Revoke with: kastellan-cli pair revoke {channel} {peer}");
    ExitCode::from(0)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --bin kastellan-cli pair -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/src/bin/kastellan-cli/pair.rs
git commit -m "feat(cli): pair issue-token for operator-only email pairing

Creates the pairing row outright and mints a long-lived token, storing only its
SHA-256 and printing the plaintext once. A separate subcommand rather than flags
on 'pair issue': that mints a single-use code for an in-channel handshake, and
one command meaning two things depending on flags is a footgun.

The peer is lowercased at mint time to match the normalization the email channel
applies to a From header, so a case mismatch cannot silently fail to authorize."
```

---

> ⚠️ **The code in this task was WRONG and has been superseded in-branch.** Review
> found three Criticals in it, all originating here, not in the implementation:
> (1) the `Authentication-Results` parse was blind to quoted strings and comments,
> so `dmarc=fail` could be read as pass via a crafted property value — `;` is legal
> inside an RFC 5321 quoted local-part; (2) a legal authserv-id carrying a version
> or comment was skipped, letting a forged header below it decide; (3)
> `trimmed[..TOKEN_PREFIX.len()]` byte-sliced without a char-boundary check, so a
> CJK/emoji body panicked — and release builds are `panic = "abort"`, making it an
> unauthenticated remote DoS. Two Importants followed: a quoted reply leaked the
> token, and the "topmost *matching* header" rule let a typo'd authserv-id hand the
> decision to a forgery. **The shipped implementation is the authority**; the rules
> it follows are recorded in §4.3 of the design spec. Read the code, not this task.

### Task 4: pure gate — DMARC verdict + token extraction

**Files:**
- Create: `core/src/channel/email/gate.rs`, `core/src/channel/email/mod.rs`
- Modify: `core/src/channel/mod.rs` (add `pub mod email;`)
- Test: inline `#[cfg(test)] mod tests` in `gate.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub fn trusted_dmarc_pass(headers: &[(String, String)], authserv_id: &str) -> bool`
  - `pub fn extract_token(body: &str) -> (Option<String>, String)`
  - `pub const TOKEN_PREFIX: &str = "kastellan-token:"`

- [ ] **Step 1: Write the failing tests**

Create `core/src/channel/email/gate.rs` containing only the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn h(name: &str, value: &str) -> (String, String) {
        (name.to_string(), value.to_string())
    }

    #[test]
    fn dmarc_pass_from_our_own_mx_is_accepted() {
        let headers = vec![h("Authentication-Results", "mx.example.net; spf=pass; dkim=pass; dmarc=pass")];
        assert!(trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn dmarc_fail_is_rejected() {
        let headers = vec![h("Authentication-Results", "mx.example.net; dmarc=fail")];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn forged_header_from_another_authserv_is_ignored() {
        // THE attack: the sender writes their own Authentication-Results line.
        // Only our MX's header counts, and ours says fail.
        let headers = vec![
            h("Authentication-Results", "mx.example.net; dmarc=fail"),
            h("Authentication-Results", "evil.example.com; dmarc=pass"),
        ];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn only_the_topmost_matching_header_counts() {
        // A sender can prepend a header claiming our authserv-id, but our MX
        // prepends ITS header last, so ours is topmost. Index 0 wins.
        let headers = vec![
            h("Authentication-Results", "mx.example.net; dmarc=fail"),
            h("Authentication-Results", "mx.example.net; dmarc=pass"),
        ];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn no_matching_authserv_fails_closed() {
        let headers = vec![h("Authentication-Results", "other.mx; dmarc=pass")];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
        assert!(!trusted_dmarc_pass(&[], "mx.example.net"), "no headers at all must fail closed");
    }

    #[test]
    fn authserv_id_match_is_exact_not_prefix() {
        let headers = vec![h("Authentication-Results", "mx.example.net.evil.com; dmarc=pass")];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn header_name_match_is_case_insensitive() {
        let headers = vec![h("authentication-results", "mx.example.net; dmarc=pass")];
        assert!(trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn dmarc_token_must_not_match_a_substring() {
        // "dmarc=pass" must not be satisfied by e.g. "xdmarc=pass".
        let headers = vec![h("Authentication-Results", "mx.example.net; xdmarc=pass; dmarc=fail")];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn token_is_extracted_and_stripped_from_the_body() {
        let (tok, body) = extract_token("kastellan-token: abc123\nwhat is 17*23?");
        assert_eq!(tok.as_deref(), Some("abc123"));
        assert_eq!(body, "what is 17*23?");
        assert!(!body.contains("abc123"), "the secret must not survive into the instruction");
    }

    #[test]
    fn token_may_appear_anywhere_in_the_body() {
        let (tok, body) = extract_token("what is 17*23?\n\nkastellan-token: abc123\n");
        assert_eq!(tok.as_deref(), Some("abc123"));
        assert_eq!(body.trim(), "what is 17*23?");
    }

    #[test]
    fn every_token_line_is_stripped_even_when_repeated() {
        let (tok, body) = extract_token("kastellan-token: aaa\nhi\nkastellan-token: bbb");
        assert_eq!(tok.as_deref(), Some("aaa"), "the first token is the presented one");
        assert!(!body.contains("aaa") && !body.contains("bbb"),
                "no token line may survive into the instruction");
    }

    #[test]
    fn absent_token_yields_none_and_an_unchanged_body() {
        let (tok, body) = extract_token("just a question");
        assert_eq!(tok, None);
        assert_eq!(body, "just a question");
    }

    #[test]
    fn token_prefix_match_is_case_insensitive_and_tolerates_spacing() {
        let (tok, _) = extract_token("Kastellan-Token:   abc123  ");
        assert_eq!(tok.as_deref(), Some("abc123"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core channel::email::gate -- --nocapture`
Expected: FAIL to compile — `trusted_dmarc_pass` and `extract_token` do not exist.

- [ ] **Step 3: Implement**

Create `core/src/channel/email/mod.rs`:

```rust
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
```

Prepend the implementation to `core/src/channel/email/gate.rs`, above the test module:

```rust
//! Pure inbound gate for the email channel. No I/O, no DB, no clock.
//!
//! Two independent checks, neither sufficient alone:
//! * [`trusted_dmarc_pass`] — did OUR MX say DMARC passed? Anyone can write
//!   `Authentication-Results` lines into a message they send, so only the
//!   topmost header bearing our configured authserv-id is evidence.
//! * [`extract_token`] — did the sender include the per-pairing shared secret?
//!   Defence in depth against a misconfigured or compromised MX.

/// Line prefix carrying the per-pairing token, e.g.
/// `kastellan-token: 9f2a…`. Matched case-insensitively.
pub const TOKEN_PREFIX: &str = "kastellan-token:";

/// Header the MX writes its authentication verdict into (RFC 8601).
const AUTH_RESULTS: &str = "authentication-results";

/// True iff the **topmost** `Authentication-Results` header whose authserv-id
/// equals `authserv_id` reports `dmarc=pass`.
///
/// Fails closed: no matching header (or no headers at all) ⇒ `false`. Only the
/// first match is consulted — a sender may prepend a header claiming our
/// authserv-id, but our own MX prepends its header on receipt, so ours is the
/// topmost one. `headers` must be in wire order, topmost first.
pub fn trusted_dmarc_pass(headers: &[(String, String)], authserv_id: &str) -> bool {
    let want = authserv_id.trim().to_ascii_lowercase();
    if want.is_empty() {
        return false; // Unconfigured authserv-id must never admit.
    }
    for (name, value) in headers {
        if !name.trim().eq_ignore_ascii_case(AUTH_RESULTS) {
            continue;
        }
        // authserv-id is the first token, up to the first ';'.
        let (id, rest) = match value.split_once(';') {
            Some((id, rest)) => (id, rest),
            None => continue,
        };
        if !id.trim().to_ascii_lowercase().eq(&want) {
            continue; // Not our MX — a forged or upstream header. Ignore it.
        }
        // Topmost match decides, pass or fail. Do NOT keep looking: falling
        // through to a later header is exactly how a forged "dmarc=pass"
        // beneath our MX's "dmarc=fail" would win.
        return has_method_result(rest, "dmarc", "pass");
    }
    false
}

/// Whether `ptypes` contains `method=result` as a whole token, so `dmarc=pass`
/// is not satisfied by `xdmarc=pass`.
fn has_method_result(ptypes: &str, method: &str, result: &str) -> bool {
    ptypes
        .split(|c: char| c == ';' || c.is_whitespace())
        .filter_map(|kv| kv.split_once('='))
        .any(|(k, v)| {
            k.trim().eq_ignore_ascii_case(method)
                // The value may carry a comment, e.g. `pass (policy)`.
                && v.trim()
                    .split(|c: char| c.is_whitespace() || c == '(')
                    .next()
                    .unwrap_or("")
                    .eq_ignore_ascii_case(result)
        })
}

/// Split a body into `(presented_token, body_without_any_token_line)`.
///
/// The FIRST token line supplies the presented token; **every** token line is
/// removed, so the shared secret never reaches a task payload, an LLM prompt,
/// or a quoted reply — including a decoy second line an attacker might add.
pub fn extract_token(body: &str) -> (Option<String>, String) {
    let mut token: Option<String> = None;
    let mut kept: Vec<&str> = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.len() >= TOKEN_PREFIX.len()
            && trimmed[..TOKEN_PREFIX.len()].eq_ignore_ascii_case(TOKEN_PREFIX)
        {
            let value = trimmed[TOKEN_PREFIX.len()..].trim();
            if token.is_none() && !value.is_empty() {
                token = Some(value.to_string());
            }
            continue; // Never keep a token line.
        }
        kept.push(line);
    }
    (token, kept.join("\n").trim().to_string())
}
```

Add `pub mod email;` to the module list in `core/src/channel/mod.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core channel::email::gate -- --nocapture`
Expected: PASS, 13 tests.

- [ ] **Step 5: Prove the gate is load-bearing (negative controls)**

Temporarily weaken `trusted_dmarc_pass` by replacing the `return has_method_result(...)` with `continue`, so it keeps scanning past our MX's verdict.

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core channel::email::gate -- --nocapture`
Expected: `forged_header_from_another_authserv_is_ignored` and `only_the_topmost_matching_header_counts` **FAIL**. Then revert the weakening and confirm they pass again. If either still passes while weakened, the test is vacuous — fix the test before continuing.

- [ ] **Step 6: Commit**

```bash
git add core/src/channel/email/mod.rs core/src/channel/email/gate.rs core/src/channel/mod.rs
git commit -m "feat(channel/email): pure DMARC + token gate

Only the topmost Authentication-Results header bearing our configured
authserv-id is evidence: a sender can freely write their own such headers, so
scanning past our MX's verdict is precisely the bug that would admit a spoofed
message. Unconfigured authserv-id, no matching header, and no headers all fail
closed.

extract_token removes EVERY token line while taking the first as presented, so
neither the real secret nor a decoy survives into the instruction.

Both negative controls verified: the forged-header tests fail against a
deliberately weakened implementation."
```

---

> ⚠️ **The code in this task was amended in-branch by review; the shipped code
> is the authority.** Three changes it does not show: (1) the pairing carve-out
> in `bus::handle_inbound` is gated on `msg.evidence.is_none()` — this task's
> `// ... existing carve-out + REJECTED_UNPAIRED block, unchanged ...` is
> **wrong**, because an unpaired email sender resolves to `Rejected` and *does*
> reach the carve-out, where `try_pair` would mint a NULL-token row and disable
> DMARC+token for that address for good (spec D8, corrected); (2)
> `DbPeerAuthorizer`'s `Ok(Some(None))` arm refuses an evidence-bearing
> transport instead of admitting it — a token-less pairing row is misconfigured
> for such a transport, not permissive; (3) `AuthDecision::RejectedUnauthentic`
> carries an `UnauthenticReason`, which the bus writes into the audit payload as
> a stable label, because otherwise every denial arm produces a byte-identical
> `audit_log` row and a wrong `KASTELLAN_EMAIL_AUTHSERV_ID` is
> indistinguishable from a token typo. Read the code, not this task.

### Task 5: evidence plumbing — types, authorizer, bus

**Files:**
- Modify: `core/src/channel/mod.rs`, `core/src/channel/auth.rs`, `core/src/channel/bus.rs`, `core/src/channel/polled_driver.rs`, `core/src/channel/matrix/wire.rs`
- Test: inline test modules in `auth.rs` and `bus.rs`

**Interfaces:**
- Consumes: `kastellan_db::pairings::token_hash_for` (Task 2), `ingest::sha256_hex`.
- Produces:
  - `channel::PeerEvidence { dmarc_pass: bool, presented_token: Option<String> }`
  - `IncomingMessage.evidence: Option<PeerEvidence>`, `PolledEvent.evidence: Option<PeerEvidence>`, `PolledEvent.ack_token: Option<String>`
  - `AuthDecision::RejectedUnauthentic`
  - `PeerAuthorizer::authorize(&self, channel, peer, evidence: Option<&PeerEvidence>) -> AuthDecision`
  - `actions::REJECTED_UNAUTHENTIC = "channel.rejected_unauthentic"`

- [ ] **Step 1: Write the failing tests**

Append to `core/src/channel/auth.rs`'s `mod tests`:

```rust
#[tokio::test]
async fn static_pairings_ignore_evidence() {
    // StaticPairings is the test/legacy authorizer; evidence is a DB concept.
    let a = StaticPairings::from_peers([PeerId("@me:srv".into())]);
    let ev = PeerEvidence { dmarc_pass: false, presented_token: None };
    assert_eq!(a.authorize(&ch(), &PeerId("@me:srv".into()), Some(&ev)).await,
               AuthDecision::Recognised);
}
```

Append to `core/src/channel/bus.rs`'s `mod tests`:

```rust
/// Authorizer that mimics DbPeerAuthorizer's evidence rule without a DB.
struct TokenAuthorizer {
    expected: &'static str,
}

#[async_trait::async_trait]
impl PeerAuthorizer for TokenAuthorizer {
    async fn authorize(
        &self,
        _c: &ChannelId,
        _p: &PeerId,
        evidence: Option<&PeerEvidence>,
    ) -> AuthDecision {
        match evidence {
            Some(e) if e.dmarc_pass
                && e.presented_token.as_deref() == Some(self.expected) => AuthDecision::Recognised,
            Some(_) => AuthDecision::RejectedUnauthentic,
            None => AuthDecision::Rejected,
        }
    }
}

fn email_msg(body: &str, dmarc_pass: bool, token: Option<&str>) -> IncomingMessage {
    IncomingMessage {
        channel: ChannelId("email".into()),
        peer: PeerId("me@example.org".into()),
        conversation: ConversationId("<mid@example.org>".into()),
        body: body.to_string(),
        evidence: Some(PeerEvidence {
            dmarc_pass,
            presented_token: token.map(|s| s.to_string()),
        }),
    }
}

#[tokio::test]
async fn unauthentic_email_audits_its_own_action_and_never_enqueues() {
    let auth = TokenAuthorizer { expected: "good-token" };
    let ev = FakeEvents::default();
    let out = handle_inbound(&auth, None, &ev, &email_msg("hi", false, Some("good-token"))).await;
    assert!(out.is_none());
    assert!(ev.enqueued.lock().unwrap().is_empty(), "a DMARC failure must not enqueue");
    let actions = ev.audited.lock().unwrap().clone();
    assert!(actions.iter().any(|(a, _)| a == actions::REJECTED_UNAUTHENTIC),
            "must audit rejected_unauthentic, got {actions:?}");
}

#[tokio::test]
async fn unauthentic_email_never_reaches_the_pairing_carve_out() {
    // The carve-out compares an unpaired body against a live code. A spoofable
    // transport must not get to attempt that.
    let auth = TokenAuthorizer { expected: "good-token" };
    let pairing = FakePairing { code: Some("SECRET-CODE") };
    let ev = FakeEvents::default();
    let out = handle_inbound(
        &auth, Some(&pairing), &ev, &email_msg("SECRET-CODE", false, None),
    ).await;
    assert!(out.is_none(), "an unauthentic message must not be able to pair");
    let actions = ev.audited.lock().unwrap().clone();
    assert!(!actions.iter().any(|(a, _)| a == actions::PAIRED),
            "carve-out must be unreachable for unauthentic input");
}

#[tokio::test]
async fn unauthentic_audit_payload_carries_no_body_and_no_token() {
    let auth = TokenAuthorizer { expected: "good-token" };
    let ev = FakeEvents::default();
    let secret_body = "my private question";
    handle_inbound(&auth, None, &ev, &email_msg(secret_body, false, Some("good-token"))).await;
    let audited = ev.audited.lock().unwrap().clone();
    let (_, payload) = audited.iter().find(|(a, _)| a == actions::REJECTED_UNAUTHENTIC).unwrap();
    let rendered = payload.to_string();
    assert!(!rendered.contains(secret_body), "audit must never carry the body");
    assert!(!rendered.contains("good-token"), "audit must never carry the token");
}

#[tokio::test]
async fn authentic_email_enqueues_normally() {
    let auth = TokenAuthorizer { expected: "good-token" };
    let ev = FakeEvents::default();
    handle_inbound(&auth, None, &ev, &email_msg("what is 17*23?", true, Some("good-token"))).await;
    assert_eq!(ev.enqueued.lock().unwrap().len(), 1, "a gated-pass email must become a task");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core channel:: -- --nocapture`
Expected: FAIL to compile — `PeerEvidence`, the third `authorize` parameter, and `RejectedUnauthentic` do not exist.

- [ ] **Step 3: Implement**

In `core/src/channel/mod.rs`:

```rust
/// Transport-supplied evidence that an inbound message really came from the
/// claimed peer.
///
/// `IncomingMessage.evidence` is `None` when the transport authenticates its
/// own peers (Matrix: E2E + homeserver auth) — the bus then applies no extra
/// check, which is what keeps Matrix behaviour byte-identical. `Some` means the
/// transport cannot vouch for the sender and the bus must decide.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerEvidence {
    /// Our own MX reported `dmarc=pass` (see `email::gate::trusted_dmarc_pass`).
    pub dmarc_pass: bool,
    /// The per-pairing token the sender presented, already stripped from the body.
    pub presented_token: Option<String>,
}
```

Add `pub evidence: Option<PeerEvidence>,` to `IncomingMessage`, and to `actions`:

```rust
/// A message failed transport authenticity (DMARC and/or token) — dropped
/// before authorization, so it never reaches the pairing carve-out.
pub const REJECTED_UNAUTHENTIC: &str = "channel.rejected_unauthentic";
```

In `core/src/channel/auth.rs`: add the `RejectedUnauthentic` variant with a doc
comment, widen the trait method to take `evidence: Option<&PeerEvidence>`,
ignore it in `StaticPairings`, and implement the rule in `DbPeerAuthorizer`:

```rust
#[async_trait::async_trait]
impl PeerAuthorizer for DbPeerAuthorizer {
    async fn authorize(
        &self,
        channel: &ChannelId,
        peer: &PeerId,
        evidence: Option<&PeerEvidence>,
    ) -> AuthDecision {
        match kastellan_db::pairings::token_hash_for(&self.pool, &channel.0, &peer.0).await {
            // No active pairing.
            Ok(None) => AuthDecision::Rejected,
            // Paired, no token required (Matrix) — evidence is not consulted.
            Ok(Some(None)) => AuthDecision::Recognised,
            // Paired WITH a token: the transport must supply evidence, DMARC
            // must pass, and the token must match.
            Ok(Some(Some(expected))) => {
                let Some(ev) = evidence else {
                    tracing::warn!(channel = %channel.0,
                        "pairing requires a token but the transport supplied no evidence");
                    return AuthDecision::RejectedUnauthentic;
                };
                if !ev.dmarc_pass {
                    return AuthDecision::RejectedUnauthentic;
                }
                let presented = match ev.presented_token.as_deref() {
                    Some(t) => crate::channel::ingest::sha256_hex(t.as_bytes()),
                    None => return AuthDecision::RejectedUnauthentic,
                };
                if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
                    AuthDecision::Recognised
                } else {
                    AuthDecision::RejectedUnauthentic
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, channel = %channel.0,
                    "pairing lookup failed; failing closed");
                AuthDecision::Rejected
            }
        }
    }
}

/// Length-independent byte comparison, so a token check cannot be narrowed by
/// timing. Both inputs here are fixed-length hex digests, but comparing them
/// with `==` would still short-circuit on the first differing byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
```

In `core/src/channel/bus.rs`'s `handle_inbound`, replace the opening
authorization block so the three decisions are distinct:

```rust
match authorizer.authorize(&msg.channel, &msg.peer, msg.evidence.as_ref()).await {
    AuthDecision::Recognised => {}
    AuthDecision::RejectedUnauthentic => {
        // Deliberately BEFORE and WITHOUT the pairing carve-out: the carve-out
        // compares unpaired input against a live code, and a transport that
        // cannot authenticate its sender must not get to attempt that.
        // Payload carries the peer only — never the body, never the token.
        events
            .audit(
                actions::REJECTED_UNAUTHENTIC,
                serde_json::json!({"channel": msg.channel.0, "peer": msg.peer.0}),
            )
            .await;
        return None;
    }
    AuthDecision::Rejected => {
        // ... existing carve-out + REJECTED_UNPAIRED block, unchanged ...
    }
}
```

Add `evidence: Option<PeerEvidence>` and `ack_token: Option<String>` to
`PolledEvent` in `polled_driver.rs`, carry `evidence` through when the driver
builds an `IncomingMessage`, and set both to `None` in `parse_matrix_poll`
(`matrix/wire.rs`). Fix every remaining construction site the compiler flags.

- [ ] **Step 4: Run the whole channel suite**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core channel:: -- --nocapture`
Expected: PASS, including the pre-existing Matrix and bus tests unchanged.

- [ ] **Step 5: Prove the carve-out skip is load-bearing**

Temporarily change the `RejectedUnauthentic` arm to fall through into the `Rejected` arm.

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core channel::bus -- --nocapture`
Expected: `unauthentic_email_never_reaches_the_pairing_carve_out` **FAILS**. Revert and confirm it passes.

- [ ] **Step 6: Commit**

```bash
git add core/src/channel/mod.rs core/src/channel/auth.rs core/src/channel/bus.rs core/src/channel/polled_driver.rs core/src/channel/matrix/wire.rs
git commit -m "feat(channel): transport authenticity evidence at the auth chokepoint

Adds PeerEvidence, AuthDecision::RejectedUnauthentic, and an evidence parameter
on PeerAuthorizer. A pairing row with NULL token_sha256 ignores evidence
entirely, so Matrix stays byte-identical — pinned by test.

RejectedUnauthentic is audited as its own action and returns BEFORE the pairing
carve-out: that carve-out compares unpaired input against a live code, and a
transport that cannot authenticate its sender must not get to attempt it.
Verified load-bearing by falling the arm through and watching the test fail.

Audit payloads carry the peer only, never the body and never the token."
```

---

### Task 6: driver ack support

**Files:**
- Modify: `core/src/channel/polled_driver.rs`, `core/src/channel/matrix/wire.rs`
- Test: `core/src/channel/polled_driver/tests.rs`

**Interfaces:**
- Consumes: `PolledEvent.ack_token` (Task 5).
- Produces: `PolledWorkerSpec.ack_method: Option<&'static str>`, `pub type EncodeAck = fn(&str) -> serde_json::Value`, `PolledWorkerDriver::spawn(spec, calls, parse_poll, encode_send, encode_ack: Option<EncodeAck>, cid)`.

- [ ] **Step 1: Write the failing tests**

Append to `core/src/channel/polled_driver/tests.rs` (reuse the existing fake `WorkerCalls` recorder in that file):

```rust
#[test]
fn ack_is_called_after_the_event_reaches_the_bus() {
    let calls = RecordingCalls::with_poll_events(vec![
        serde_json::json!({"peer": "me@example.org", "conversation": "<a>",
                           "body": "hi", "ack_token": "42"}),
    ]);
    let log = calls.log();
    let (driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(), Box::new(calls), parse_test_poll, encode_test_send,
        Some(encode_test_ack), ChannelId("email".into()),
    )
    .unwrap();

    let mut rx = driver.inbound_rx;
    let msg = rx.blocking_recv().expect("one inbound event");
    assert_eq!(msg.peer.0, "me@example.org");

    wait_until(|| log.lock().unwrap().iter().any(|(m, _)| m == "email.ack"));
    let entry = log.lock().unwrap().iter().find(|(m, _)| m == "email.ack").cloned().unwrap();
    assert_eq!(entry.1["cursor"], "42", "ack must carry the event's own cursor");
}

#[test]
fn no_ack_method_means_no_ack_call() {
    // Matrix must be untouched: its spec has ack_method: None.
    let calls = RecordingCalls::with_poll_events(vec![
        serde_json::json!({"peer": "@me:srv", "conversation": "!r", "body": "hi"}),
    ]);
    let log = calls.log();
    let (driver, _identity) = PolledWorkerDriver::spawn(
        spec_without_ack(), Box::new(calls), parse_test_poll, encode_test_send,
        None, ChannelId("matrix".into()),
    )
    .unwrap();
    let mut rx = driver.inbound_rx;
    rx.blocking_recv().expect("one inbound event");
    assert!(!log.lock().unwrap().iter().any(|(m, _)| m.ends_with(".ack")),
            "a spec without ack_method must never call ack");
}

#[test]
fn an_event_without_an_ack_token_is_not_acked() {
    let calls = RecordingCalls::with_poll_events(vec![
        serde_json::json!({"peer": "me@example.org", "conversation": "<a>", "body": "hi"}),
    ]);
    let log = calls.log();
    let (driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(), Box::new(calls), parse_test_poll, encode_test_send,
        Some(encode_test_ack), ChannelId("email".into()),
    )
    .unwrap();
    let mut rx = driver.inbound_rx;
    rx.blocking_recv().expect("one inbound event");
    assert!(!log.lock().unwrap().iter().any(|(m, _)| m == "email.ack"));
}
```

Add the helpers in the same test file:

```rust
fn spec_with_ack() -> PolledWorkerSpec {
    PolledWorkerSpec {
        label: "email", init_method: "email.init", poll_method: "email.poll",
        send_method: "email.send", ack_method: Some("email.ack"), poll_timeout_ms: 50,
    }
}

fn spec_without_ack() -> PolledWorkerSpec {
    PolledWorkerSpec {
        label: "matrix", init_method: "matrix.init", poll_method: "matrix.poll",
        send_method: "matrix.send", ack_method: None, poll_timeout_ms: 50,
    }
}

fn encode_test_ack(cursor: &str) -> serde_json::Value {
    serde_json::json!({ "cursor": cursor })
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core channel::polled_driver -- --nocapture`
Expected: FAIL to compile — `ack_method` and the sixth `spawn` parameter do not exist.

- [ ] **Step 3: Implement**

In `polled_driver.rs`: add `pub ack_method: Option<&'static str>` to
`PolledWorkerSpec`, `pub type EncodeAck = fn(&str) -> serde_json::Value;`, take
`encode_ack: Option<EncodeAck>` in `spawn` and thread it into `run`. In `run`,
immediately after a successful `inbound_tx.blocking_send(msg)`:

```rust
// Ack only after the bus has accepted the event, so a worker death between
// poll and hand-off redelivers rather than silently drops.
//
// Known residual (documented in the spec): if the bus later fails to
// ENQUEUE, the message is acked but lost. Matrix has the identical property
// (`channel enqueue failed; message dropped`), so this matches existing
// semantics rather than inventing a receipt protocol for one channel.
if let (Some(method), Some(enc), Some(tok)) = (spec.ack_method, encode_ack, ack_token.as_deref()) {
    if let Err(e) = calls.call(method, enc(tok)) {
        // Not fatal: the cursor simply does not advance, so the message is
        // redelivered on the next poll. At-least-once, by design.
        tracing::warn!(label = spec.label, error = %e, "ack failed; event will be redelivered");
    }
}
```

Capture `ack_token` from the `PolledEvent` before it is moved into the
`IncomingMessage`. Update `MATRIX_POLLED_SPEC` with `ack_method: None` and the
`PolledWorkerDriver::spawn` call in `channel/matrix.rs` to pass `None`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core channel:: -- --nocapture`
Expected: PASS, including all pre-existing driver and matrix tests.

- [ ] **Step 5: Commit**

```bash
git add core/src/channel/polled_driver.rs core/src/channel/polled_driver/tests.rs core/src/channel/matrix/wire.rs core/src/channel/matrix.rs
git commit -m "feat(channel): optional ack_method on the polled driver

Symmetric with the existing init/poll/send fields. The driver acks only after
the bus has accepted an event, so a worker death between poll and hand-off
redelivers rather than drops; a failed ack is non-fatal and simply leaves the
cursor unadvanced. ack_method: None keeps Matrix byte-identical, pinned by test."
```

---

### Task 7: `email-in` worker

**Files:**
- Create: `workers/email-in/Cargo.toml`, `workers/email-in/src/main.rs`, `workers/email-in/src/client.rs`, `workers/email-in/src/handler.rs`
- Modify: `Cargo.toml` (workspace members)
- Test: inline `#[cfg(test)] mod tests` in `handler.rs` and `client.rs`

**Interfaces:**
- Consumes: `kastellan_worker_web_common::http::{make_get, HttpGet, RawResponse}`, `kastellan_worker_prelude::serve_stdio`, Task 1's endpoints.
- Produces: JSON-RPC methods `email.init` → `{"address": "<agent addr>", "subscription": "<name>"}`; `email.poll {timeout_ms}` → `{"events": [{"peer", "conversation", "body", "ack_token", "auth_results": ["…"]}]}`; `email.ack {cursor}` → `{"ok": true}`.

The worker makes **no security decisions** (spec D6): it returns `auth_results` verbatim and never inspects them.

- [ ] **Step 1: Write the failing tests**

Create `workers/email-in/src/handler.rs` with the test module first (mirror `workers/mail/src/handler.rs`'s use of `web_common::testing::FakeGet`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_maps_changes_and_detail_into_events() {
        let h = handler_with_canned();
        let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
        let ev = &out["events"][0];
        assert_eq!(ev["peer"], "me@example.org", "peer is the From address");
        assert_eq!(ev["conversation"], "<mid-1@example.org>", "conversation is the Message-ID");
        assert_eq!(ev["ack_token"], "7", "ack_token is the localmail message id");
        assert!(ev["body"].as_str().unwrap().contains("what is 17*23"));
    }

    #[test]
    fn from_address_is_lowercased_so_it_matches_the_paired_peer() {
        let h = handler_with_from("Me@Example.ORG");
        let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
        assert_eq!(out["events"][0]["peer"], "me@example.org");
    }

    #[test]
    fn reply_to_is_never_used_as_the_peer() {
        // Honouring Reply-To would let a sender who passes the gate redirect
        // the agent's reply to a third party.
        let h = handler_with_reply_to("attacker@evil.example");
        let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
        assert_eq!(out["events"][0]["peer"], "me@example.org");
    }

    #[test]
    fn auth_results_are_returned_verbatim_and_in_order() {
        let h = handler_with_auth_results(vec![
            "mx.example.net; dmarc=pass".to_string(),
            "evil.example.com; dmarc=pass".to_string(),
        ]);
        let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
        let ar = out["events"][0]["auth_results"].as_array().unwrap();
        assert_eq!(ar.len(), 2, "every header is surfaced; core decides which counts");
        assert_eq!(ar[0], "mx.example.net; dmarc=pass", "wire order must be preserved");
    }

    #[test]
    fn empty_changes_yields_no_events() {
        let h = handler_with_empty_changes();
        let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
        assert_eq!(out["events"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn ack_posts_the_cursor_upstream() {
        let (h, recorder) = handler_recording_requests();
        h.call("email.ack", serde_json::json!({"cursor": "7"})).unwrap();
        assert!(recorder.lock().unwrap().iter().any(|r| r.contains("/v1/changes/ack")));
    }

    #[test]
    fn unknown_method_is_rejected() {
        let h = handler_with_canned();
        assert!(h.call("email.nope", serde_json::json!({})).is_err());
    }
}
```

Write the small helper constructors (`handler_with_canned`, `handler_with_from`,
`handler_with_reply_to`, `handler_with_auth_results`, `handler_with_empty_changes`,
`handler_recording_requests`) over `FakeGet`, following the pattern already in
`workers/mail/src/handler.rs`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-email-in -- --nocapture`
Expected: FAIL — the crate does not exist yet.

- [ ] **Step 3: Create the crate**

`workers/email-in/Cargo.toml` — copy `workers/mail/Cargo.toml` verbatim, changing
`name`/`description`/`[[bin]].name` to `kastellan-worker-email-in`. **No new
dependency.** Add `"workers/email-in"` to the workspace `members` list in the
root `Cargo.toml`.

`workers/email-in/src/main.rs`:

```rust
//! email-in: polls a localmail subscription and surfaces new messages as
//! channel events. Returns RAW MATERIAL ONLY — the DMARC verdict and the
//! per-pairing token are judged in core (`channel/email/gate.rs`), never here,
//! so that every rejection is auditable.
//! Design: docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md

mod client;
mod handler;

use kastellan_worker_prelude::serve_stdio;

fn main() -> anyhow::Result<()> {
    let mut handler = handler::EmailInHandler::from_env()?;
    serve_stdio(&mut handler)?;
    Ok(())
}
```

`client.rs` — model on `workers/mail/src/client.rs` (same `make_get` transport,
same bearer-token file, same `check`/`get_json` helpers), exposing:

```rust
/// `GET /v1/changes?subscription=<name>` — messages newer than the server-side cursor.
pub fn changes(&self, subscription: &str) -> Result<serde_json::Value, MailError>;
/// `GET /v1/messages/{id}?headers=full` — full headers, needed for
/// Authentication-Results and Message-ID.
pub fn message_detail(&self, id: &str) -> Result<serde_json::Value, MailError>;
/// `POST /v1/changes/ack {"subscription": …, "cursor": …}`.
pub fn ack(&self, subscription: &str, cursor: &str) -> Result<(), MailError>;
```

`handler.rs` — `email.init` returns the configured address and subscription
name; `email.poll` long-polls by calling `changes` and, when empty, sleeping
250 ms until `timeout_ms` elapses, then for each new id calls `message_detail`
and builds an event:

```rust
/// Build one event from a localmail message detail. Pure so it is unit-testable
/// without a transport.
///
/// `peer` is the From address, lowercased to match the normalization
/// `pair issue-token` applies — a case mismatch would silently never authorize.
/// `Reply-To` is deliberately ignored: honouring it would let a sender who
/// passes the gate redirect the agent's reply to a third party.
/// `auth_results` is every `Authentication-Results` header **in wire order**;
/// this worker never inspects them, because core decides which one counts.
pub fn build_event(detail: &serde_json::Value, message_id: &str) -> Option<serde_json::Value> {
    // localmail returns `from` as {"address": …, "name": …}.
    let from = detail
        .get("from")
        .and_then(|f| f.get("address"))
        .and_then(|a| a.as_str())?
        .trim()
        .to_ascii_lowercase();
    if from.is_empty() {
        return None; // No sender to attribute the message to; drop it.
    }

    let headers = detail.get("headers").and_then(|h| h.as_object());

    // Conversation = the RFC 5322 Message-ID, so slice 2's reply can set
    // In-Reply-To/References and thread. Fall back to the localmail id, which
    // is stable and unique, when the header is absent.
    let conversation = headers
        .and_then(|h| h.get("Message-ID").or_else(|| h.get("Message-Id")))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| format!("localmail:{message_id}"));

    // Every Authentication-Results header, wire order preserved. A single
    // header may arrive as a string, repeated headers as an array.
    let auth_results: Vec<String> = match headers.and_then(|h| {
        h.get("Authentication-Results").or_else(|| h.get("authentication-results"))
    }) {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(a)) => {
            a.iter().filter_map(|v| v.as_str()).map(String::from).collect()
        }
        _ => Vec::new(),
    };

    let body = detail.get("body_text").and_then(|b| b.as_str()).unwrap_or("").to_string();

    Some(serde_json::json!({
        "peer": from,
        "conversation": conversation,
        "body": body,
        "ack_token": message_id,
        "auth_results": auth_results,
    }))
}
```

Confirm against a real localmail response before relying on the field names
(`from.address`, `headers`, `body_text`): run
`curl -sk -H "Authorization: Bearer $TOKEN" "$ENDPOINT/v1/messages/<id>?headers=full" | jq 'keys'`
against the live instance, and adjust both this function and the Task 9 mock to
match. The #487 fix was exactly this class of mock-versus-real drift.

- [ ] **Step 4: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-email-in -- --nocapture`
Expected: PASS, 7 tests.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml workers/email-in/
git commit -m "feat(worker): email-in polls a localmail subscription

Surfaces new messages as channel events, returning raw material only: every
Authentication-Results header in wire order, never a verdict. The gate lives in
core so rejections are auditable (spec D6).

The peer is the From address lowercased to match pair issue-token's
normalization; Reply-To is deliberately ignored, since honouring it would let a
sender who passes the gate redirect the agent's reply to a third party.

No new dependency: reuses web-common's make_get transport, so force-routing and
the #492 extra-CA path work unchanged."
```

---

### Task 8: core-side `EmailChannel`

**Files:**
- Create: `core/src/channel/email/wire.rs`, `core/src/channel/email/config.rs`, `core/src/channel/email/policy.rs`
- Modify: `core/src/channel/email/mod.rs`
- Test: inline test modules in `wire.rs` and `config.rs`

**Interfaces:**
- Consumes: `gate::{trusted_dmarc_pass, extract_token}` (Task 4), `PolledEvent`/`PeerEvidence` (Task 5), `PolledWorkerSpec.ack_method` (Task 6), Task 7's wire shapes.
- Produces:
  - `EMAIL_POLLED_SPEC: PolledWorkerSpec`
  - `parse_email_poll_with(v, authserv_id) -> anyhow::Result<Vec<PolledEvent>>`
  - `encode_email_ack(cursor: &str) -> serde_json::Value`
  - `EmailConfig::from_env() -> anyhow::Result<Option<EmailConfig>>` (`Ok(None)` = not configured)
  - `build_email_policy(worker_bin, endpoint_host, endpoint_port, token_file) -> SandboxPolicy`
  - `EmailChannel` implementing `Channel`; `spawn_email_worker(backend, id, cfg, egress) -> anyhow::Result<SpawnedEmailWorker>`

`parse_email_poll_with` takes the authserv-id, but `ParsePoll` is a bare `fn`
pointer with no room for state. Resolve it by reading the authserv-id from a
process-global set once at channel construction (`OnceLock<String>`), with the
`_with` form as the pure, directly-testable core.

- [ ] **Step 1: Write the failing tests**

Create `core/src/channel/email/wire.rs` with the tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn poll_result(auth_results: Vec<&str>, body: &str) -> serde_json::Value {
        serde_json::json!({"events": [{
            "peer": "me@example.org",
            "conversation": "<mid-1@example.org>",
            "body": body,
            "ack_token": "7",
            "auth_results": auth_results,
        }]})
    }

    #[test]
    fn parse_builds_evidence_from_our_mx_and_strips_the_token() {
        let v = poll_result(vec!["mx.example.net; dmarc=pass"],
                            "kastellan-token: abc123\nwhat is 17*23?");
        let events = parse_email_poll_with(v, "mx.example.net").unwrap();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.body, "what is 17*23?", "the token must not reach the instruction");
        assert_eq!(ev.ack_token.as_deref(), Some("7"));
        let evidence = ev.evidence.as_ref().expect("email always supplies evidence");
        assert!(evidence.dmarc_pass);
        assert_eq!(evidence.presented_token.as_deref(), Some("abc123"));
    }

    #[test]
    fn forged_auth_results_do_not_produce_a_passing_verdict() {
        let v = poll_result(
            vec!["mx.example.net; dmarc=fail", "evil.example.com; dmarc=pass"],
            "kastellan-token: abc123\nhi",
        );
        let events = parse_email_poll_with(v, "mx.example.net").unwrap();
        assert!(!events[0].evidence.as_ref().unwrap().dmarc_pass);
    }

    #[test]
    fn evidence_is_always_some_for_email_even_when_everything_fails() {
        // None would mean "the transport authenticates its own peers", which
        // for email would skip the gate entirely.
        let v = poll_result(vec![], "no token here");
        let events = parse_email_poll_with(v, "mx.example.net").unwrap();
        let ev = events[0].evidence.as_ref().expect("must be Some, never None");
        assert!(!ev.dmarc_pass);
        assert_eq!(ev.presented_token, None);
    }

    #[test]
    fn malformed_poll_result_is_an_error_not_a_silent_empty() {
        assert!(parse_email_poll_with(serde_json::json!({"nope": 1}), "mx").is_err());
    }

    #[test]
    fn ack_encodes_the_cursor() {
        assert_eq!(encode_email_ack("42"), serde_json::json!({"cursor": "42"}));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core channel::email::wire -- --nocapture`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

`wire.rs`:

```rust
//! Email wire codecs + the polled-driver spec. Pure: this is where the raw
//! material from `email-in` becomes gated evidence.

use std::sync::OnceLock;

use crate::channel::email::gate::{extract_token, trusted_dmarc_pass};
use crate::channel::polled_driver::{PolledEvent, PolledWorkerSpec};
use crate::channel::PeerEvidence;

/// Long-poll wait inside one `email.poll`. Longer than Matrix's 2 s: email is
/// an async fallback, not an interactive chat, and each poll is an HTTP round
/// trip to localmail.
pub const POLL_MS: u64 = 15_000;

pub const EMAIL_POLLED_SPEC: PolledWorkerSpec = PolledWorkerSpec {
    label: "email",
    init_method: "email.init",
    poll_method: "email.poll",
    send_method: "email.send",
    ack_method: Some("email.ack"),
    poll_timeout_ms: POLL_MS,
};

/// Configured authserv-id of our own MX. Set once at channel construction:
/// `ParsePoll` is a bare fn pointer with nowhere to carry state.
static AUTHSERV_ID: OnceLock<String> = OnceLock::new();

/// Record the authserv-id the parser will trust. Called by
/// `spawn_email_worker` before the driver starts. Idempotent; a second call
/// with a different value is ignored, which is correct for a single-daemon
/// process and avoids a mid-flight trust change.
pub fn set_authserv_id(id: &str) {
    let _ = AUTHSERV_ID.set(id.to_string());
}

/// `ParsePoll` entry point. An unset authserv-id yields `""`, which
/// `trusted_dmarc_pass` treats as fail-closed.
pub fn parse_email_poll(v: serde_json::Value) -> anyhow::Result<Vec<PolledEvent>> {
    parse_email_poll_with(v, AUTHSERV_ID.get().map(String::as_str).unwrap_or(""))
}

/// Pure core: decode one `email.poll` result into driver events, computing
/// evidence and stripping the token from every body.
pub fn parse_email_poll_with(
    v: serde_json::Value,
    authserv_id: &str,
) -> anyhow::Result<Vec<PolledEvent>> {
    let events = v
        .get("events")
        .and_then(|e| e.as_array())
        .ok_or_else(|| anyhow::anyhow!("poll result missing 'events' array"))?;
    let mut out = Vec::with_capacity(events.len());
    for e in events {
        let peer = str_field(e, "peer")?;
        let conversation = str_field(e, "conversation")?;
        let raw_body = str_field(e, "body")?;
        let ack_token = e.get("ack_token").and_then(|v| v.as_str()).map(String::from);
        let headers: Vec<(String, String)> = e
            .get("auth_results")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| ("Authentication-Results".to_string(), s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let (presented_token, body) = extract_token(&raw_body);
        out.push(PolledEvent {
            peer,
            conversation,
            body,
            // ALWAYS Some for email: None would tell the bus this transport
            // authenticates its own peers, skipping the gate entirely.
            evidence: Some(PeerEvidence {
                dmarc_pass: trusted_dmarc_pass(&headers, authserv_id),
                presented_token,
            }),
            ack_token,
        });
    }
    Ok(out)
}

fn str_field(v: &serde_json::Value, key: &str) -> anyhow::Result<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("poll event missing '{key}'"))
}

/// Encode an ack cursor for `email.ack`.
pub fn encode_email_ack(cursor: &str) -> serde_json::Value {
    serde_json::json!({ "cursor": cursor })
}

/// Slice 1 has no outbound worker, so sending is not configured. Slice 2
/// replaces this with a real `email.send` encoding.
pub fn encode_email_send(_msg: &crate::channel::OutgoingMessage) -> serde_json::Value {
    serde_json::json!({})
}
```

`config.rs` — `EmailConfig { endpoint: String, subscription: String, address: String, authserv_id: String, token_file: PathBuf, worker_bin: PathBuf }` with
`from_env()` returning `Ok(None)` when `KASTELLAN_EMAIL_ENDPOINT` is unset, and
`Err` when it is set but any of the others is missing or blank — a partially
configured channel must not start. Unit-test both arms and the blank-string case.

`policy.rs` — `build_email_policy` mirroring `matrix/policy.rs`:
`Net::Allowlist(vec![format!("{host}:{port}")])`, `Profile::WorkerNetClient`,
`fs_read` containing the token file, `fs_write` empty, `proxy_uds: None` (set at
spawn by force-routing).

`mod.rs` — add `pub mod config; pub mod policy; pub mod wire;`, the
`EmailChannel` struct (copy `MatrixChannel`'s shape: `from_driver`, `Channel`
impl whose `send` returns `anyhow::bail!("email outbound not configured (slice 2)")`),
and `spawn_email_worker`, which calls `wire::set_authserv_id(&cfg.authserv_id)`
before `PolledWorkerDriver::spawn`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core channel::email -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/src/channel/email/
git commit -m "feat(channel/email): EmailChannel over the polled driver

Wire codecs turn email-in's raw material into PeerEvidence: the DMARC verdict
comes from gate::trusted_dmarc_pass over the Authentication-Results headers in
wire order, and the token is stripped from every body before it becomes an
instruction.

Evidence is ALWAYS Some for email — None means 'this transport authenticates its
own peers', which would skip the gate entirely. Pinned by test.

Config is all-or-nothing: unset endpoint means the channel is absent, but a
partially configured channel is an error rather than a quietly degraded one.
Channel::send bails until slice 2."
```

---

### Task 9: hermetic end-to-end test

**Files:**
- Modify: `tests-common/src/mock_localmail.rs`
- Create: `core/tests/email_channel_e2e.rs`, `core/examples/fake_email_worker.rs`
- Test: the e2e itself

**Interfaces:**
- Consumes: everything from Tasks 4–8.
- Produces: proof that a gated email becomes a task and an ungated one does not.

- [ ] **Step 1: Write the failing test**

Create `core/tests/email_channel_e2e.rs`, modelled on `core/tests/matrix_channel_e2e.rs`:

```rust
//! Hermetic email-channel e2e: a fake worker process feeds canned poll results
//! through the real EmailChannel → real ChannelBus → fake events sink. No
//! localmail, no sandbox, no PG, no network.

#[tokio::test(flavor = "multi_thread")]
async fn gated_email_becomes_a_task_and_the_token_never_reaches_it() {
    let h = spawn_email_channel(vec![event_json(
        "me@example.org",
        vec!["mx.example.net; dmarc=pass"],
        "kastellan-token: good-token\nwhat is 17*23?",
        "7",
    )])
    .await;

    let payload = h.next_enqueued().await.expect("a gated email must enqueue a task");
    let instruction = payload["instruction"].as_str().unwrap();
    assert_eq!(instruction, "what is 17*23?");
    assert!(!instruction.contains("good-token"), "the token must never reach the task");
    assert_eq!(payload["kind"], "channel");
}

#[tokio::test(flavor = "multi_thread")]
async fn email_with_a_forged_auth_results_header_is_rejected_unauthentic() {
    let h = spawn_email_channel(vec![event_json(
        "me@example.org",
        vec!["mx.example.net; dmarc=fail", "evil.example.com; dmarc=pass"],
        "kastellan-token: good-token\nwhat is 17*23?",
        "7",
    )])
    .await;

    assert!(h.no_task_within(std::time::Duration::from_secs(2)).await,
            "a forged pass must not enqueue");
    assert!(h.audited(kastellan_core::channel::actions::REJECTED_UNAUTHENTIC).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn email_with_a_wrong_token_is_rejected_unauthentic() {
    let h = spawn_email_channel(vec![event_json(
        "me@example.org",
        vec!["mx.example.net; dmarc=pass"],
        "kastellan-token: WRONG\nwhat is 17*23?",
        "7",
    )])
    .await;

    assert!(h.no_task_within(std::time::Duration::from_secs(2)).await);
    assert!(h.audited(kastellan_core::channel::actions::REJECTED_UNAUTHENTIC).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delivered_event_is_acked() {
    let h = spawn_email_channel(vec![event_json(
        "me@example.org",
        vec!["mx.example.net; dmarc=pass"],
        "kastellan-token: good-token\nhi",
        "7",
    )])
    .await;
    h.next_enqueued().await.expect("task enqueued");
    assert_eq!(h.acked_cursors().await, vec!["7".to_string()]);
}
```

Plus the harness in the same file:

```rust
/// One canned `email.poll` event, in the shape `email-in` produces.
fn event_json(peer: &str, auth_results: Vec<&str>, body: &str, ack: &str) -> serde_json::Value {
    serde_json::json!({
        "peer": peer, "conversation": "<mid-1@example.org>",
        "body": body, "ack_token": ack, "auth_results": auth_results,
    })
}

/// Bus-side recorder: captures enqueued payloads and audited actions.
#[derive(Default, Clone)]
struct RecordingEvents {
    enqueued: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    audited: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

/// Mirrors DbPeerAuthorizer's rule without a DB: the pairing requires a token,
/// so evidence must be present, DMARC must pass, and the token must match.
struct TokenAuthorizer;

#[async_trait::async_trait]
impl PeerAuthorizer for TokenAuthorizer {
    async fn authorize(
        &self, _c: &ChannelId, _p: &PeerId, evidence: Option<&PeerEvidence>,
    ) -> AuthDecision {
        match evidence {
            Some(e) if e.dmarc_pass
                && e.presented_token.as_deref() == Some("good-token") => AuthDecision::Recognised,
            Some(_) => AuthDecision::RejectedUnauthentic,
            None => AuthDecision::Rejected,
        }
    }
}
```

`spawn_email_channel(events: Vec<serde_json::Value>)` then:
1. serialises `events` into `KASTELLAN_FAKE_EMAIL_EVENTS` and an ack-log path
   into `KASTELLAN_FAKE_EMAIL_ACK_LOG` (a temp file);
2. spawns `core/examples/fake_email_worker` via `ClientTransport::spawn` with a
   `Net::Deny` policy — no sandbox backend needed, this is a plain child process;
3. calls `PolledWorkerDriver::spawn(EMAIL_POLLED_SPEC, …, parse_email_poll,
   encode_email_send, Some(encode_email_ack), ChannelId("email".into()))` after
   `wire::set_authserv_id("mx.example.net")`;
4. wraps it in `EmailChannel::from_driver` and starts a `ChannelBus` over
   `TokenAuthorizer`, `RecordingEvents`, and `pairing: None`;
5. returns a handle exposing `next_enqueued()`, `no_task_within(dur)`,
   `audited(action)`, and `acked_cursors()` (reading the ack log).

Because `set_authserv_id` writes a process-global `OnceLock`, **all four tests
must use the same authserv-id** — they do.

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test email_channel_e2e -- --nocapture`
Expected: FAIL — `fake_email_worker` does not exist.

- [ ] **Step 3: Implement the fake worker and mock routes**

Create `core/examples/fake_email_worker.rs`, following
`core/examples/fake_matrix_worker.rs`:

```rust
//! Stdio JSON-RPC stub standing in for `kastellan-worker-email-in` in the
//! hermetic channel e2e. Serves canned poll events from an env var and appends
//! every acked cursor to a log file so the test can assert on acks. No network,
//! no localmail, no sandbox.

fn main() -> anyhow::Result<()> {
    let events: Vec<serde_json::Value> = std::env::var("KASTELLAN_FAKE_EMAIL_EVENTS")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let ack_log = std::env::var("KASTELLAN_FAKE_EMAIL_ACK_LOG").ok();
    let mut served = false;

    // Line-delimited JSON-RPC on stdin/stdout, same framing as the real worker.
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        if std::io::BufRead::read_line(&mut stdin.lock(), &mut line)? == 0 {
            return Ok(());
        }
        let req: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let result = match req.get("method").and_then(|m| m.as_str()) {
            Some("email.init") => {
                serde_json::json!({"address": "kastellan@example.org", "subscription": "test"})
            }
            // Serve the canned batch once, then empty batches forever, so the
            // driver's poll loop keeps running without redelivering.
            Some("email.poll") => {
                let batch = if served { vec![] } else { served = true; events.clone() };
                serde_json::json!({ "events": batch })
            }
            Some("email.ack") => {
                if let (Some(path), Some(cursor)) =
                    (ack_log.as_deref(), req["params"]["cursor"].as_str())
                {
                    use std::io::Write as _;
                    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
                    writeln!(f, "{cursor}")?;
                }
                serde_json::json!({ "ok": true })
            }
            _ => serde_json::json!(null),
        };
        let resp = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
        println!("{resp}");
        use std::io::Write as _;
        std::io::stdout().flush()?;
    }
}
```

Then add the subscription + ack routes to
`tests-common/src/mock_localmail.rs`'s `route` fn, keeping every existing route
untouched: `GET /v1/changes?subscription=<name>` returning
`{"new_messages": [...], "next_cursor": "7"}` and `POST /v1/changes/ack`
returning 204. These keep the mock faithful to Task 1 for the worker-level tests.

- [ ] **Step 4: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test email_channel_e2e -- --nocapture`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add core/tests/email_channel_e2e.rs core/examples/fake_email_worker.rs tests-common/src/mock_localmail.rs
git commit -m "test(email): hermetic channel e2e through the real bus

Drives a fake worker process through the real EmailChannel and ChannelBus: a
gated email becomes a task with the token absent from the instruction, while a
forged Authentication-Results header and a wrong token are both rejected as
unauthentic and audited. Also pins that a delivered event is acked.

mock_localmail gains the subscription + ack routes so its shapes stay faithful
to the real API."
```

---

> ⚠️ **The code and prose in this task were amended in-branch by review; the
> shipped code is the authority.** Three corrections: (1) `spawn_email_channel`
> returns `Option<ChannelBus>`, **not** `anyhow::Result<Option<...>>`, and
> `main.rs` does **not** `?` it — a partial config or spawn failure logs a loud
> `error!` and leaves the daemon running, because a *fallback* channel must not
> be able to take Matrix, the scheduler, and the graceful-shutdown path down
> with it (design §6: the daemon refuses to start *the email channel*, not the
> daemon); (2) the env-help draft here repeats the refuted D8 claim that "the
> token lives on the pairing row, so an unpaired sender can never present a
> valid one" — the **shipped** `render_email_help` correctly drops it, and
> operator-only pairing is enforced by explicit guards instead; (3) the TRAP 3
> draft still assumes the extra-CA path applies to this channel — it does not
> (the channel sidecar is a transparent tunnel with no MITM leg), and the
> shipped TRAP 3 says so. Read the code, not this task.

### Task 10: daemon wiring, operator docs, roadmap

**Files:**
- Create: `core/src/main/email_boot.rs` (sibling of the existing `core/src/main/matrix_boot.rs`)
- Modify: `core/src/main.rs` (register the module + call the boot fn), `core/src/install/plan.rs` (the operator `kastellan.env` renderer), `docs/devel/ROADMAP.md`, `docs/devel/handovers/HANDOVER.md`
- Test: `core/tests/email_channel_e2e.rs` (append), inline tests in `core/src/install/plan.rs`

**Interfaces:**
- Consumes: `EmailConfig::from_env`, `spawn_email_worker` (Task 8).
- Produces: a daemon that starts the email channel when configured and is byte-identical when not.

- [ ] **Step 1: Write the failing test**

Append to `core/tests/email_channel_e2e.rs`:

```rust
use kastellan_tests_common::env::{env_lock, EnvVarGuard};

#[test]
fn unset_email_config_yields_no_channel() {
    // The whole byte-identical-when-unset guarantee in one assertion.
    let _lock = env_lock();
    let _e = EnvVarGuard::unset("KASTELLAN_EMAIL_ENDPOINT");
    let _s = EnvVarGuard::unset("KASTELLAN_EMAIL_SUBSCRIPTION");
    let _a = EnvVarGuard::unset("KASTELLAN_EMAIL_ADDRESS");
    let _i = EnvVarGuard::unset("KASTELLAN_EMAIL_AUTHSERV_ID");
    let _t = EnvVarGuard::unset("KASTELLAN_EMAIL_TOKEN_FILE");
    let cfg = kastellan_core::channel::email::config::EmailConfig::from_env().unwrap();
    assert!(cfg.is_none(), "no email env must mean no email channel");
}

#[test]
fn partial_email_config_is_an_error_not_a_silent_skip() {
    let _lock = env_lock();
    let _s = EnvVarGuard::unset("KASTELLAN_EMAIL_SUBSCRIPTION");
    let _a = EnvVarGuard::unset("KASTELLAN_EMAIL_ADDRESS");
    // authserv-id missing: starting without it would fail every message closed
    // and look like a delivery bug rather than a misconfiguration.
    let _i = EnvVarGuard::unset("KASTELLAN_EMAIL_AUTHSERV_ID");
    let _t = EnvVarGuard::unset("KASTELLAN_EMAIL_TOKEN_FILE");
    let _e = EnvVarGuard::set("KASTELLAN_EMAIL_ENDPOINT", "https://10.0.0.3:8443");
    assert!(kastellan_core::channel::email::config::EmailConfig::from_env().is_err());
}
```

`EnvVarGuard` restores the prior value on drop and `env_lock()` serialises
against other env-mutating tests — a manual `set_var`/restore leaks the value
into whatever runs next when an assertion fails between the two.

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test email_channel_e2e config -- --nocapture --test-threads=1`
Expected: FAIL — the daemon does not yet consult `EmailConfig`.

- [ ] **Step 3: Wire the daemon and document the operator surface**

Create `core/src/main/email_boot.rs` mirroring `core/src/main/matrix_boot.rs`, containing:

```rust
// Email fallback channel (Phase 2 slice #5). Absent unless configured, so an
// unconfigured daemon is byte-identical. A PARTIAL config is a startup error,
// not a silent skip: a missing authserv-id would reject every message and look
// like a delivery bug.
match kastellan_core::channel::email::config::EmailConfig::from_env() {
    Ok(None) => {}
    Ok(Some(cfg)) => match kastellan_core::channel::email::spawn_email_worker(
        backend.clone(), ChannelId("email".into()), &cfg, email_egress,
    ) {
        Ok(spawned) => channels.push(Box::new(spawned.channel) as Box<dyn Channel>),
        Err(e) => anyhow::bail!("email channel failed to start: {e}"),
    },
    Err(e) => anyhow::bail!("email channel misconfigured: {e}"),
}
```

In `core/src/install/plan.rs`, add a pure `render_email_help() -> String`
beside the existing `render_upstream_ca_help()` and include it from
`render_env_file`. Mirror its two tests (`help_block_names_the_env_var_and_both_traps`,
`env_file_includes_the_help_block`) with email equivalents; the existing
all-lines-start-with-`#` assertion at `plan.rs:613` then covers the new block
too, so **every line below must stay a comment**:

```sh
# --- Email fallback channel (Phase 2 slice #5) -------------------------------
# Inbound only in this slice: the agent can receive and act on email, but
# replies still go out over Matrix until slice 2 ships the SMTP worker.
#
# All five are required together — a partial config aborts startup.
#KASTELLAN_EMAIL_ENDPOINT=https://10.0.0.3:8443
#KASTELLAN_EMAIL_SUBSCRIPTION=kastellan
#KASTELLAN_EMAIL_ADDRESS=kastellan@example.org
#KASTELLAN_EMAIL_TOKEN_FILE=/home/hherb/.config/kastellan/localmail-channel.token
#
# TRAP 1: the authserv-id MUST be your own MX's identifier, exactly as it
# appears in the Authentication-Results headers it writes. Anyone can put
# Authentication-Results lines in a message they send; only the topmost header
# bearing THIS id is treated as evidence. Get it wrong and every message fails
# closed.
#KASTELLAN_EMAIL_AUTHSERV_ID=mx.example.net
#
# TRAP 2: pair the sender explicitly and give them the printed token:
#   kastellan-cli pair issue-token --channel email --peer you@example.org
# There is no in-channel pairing over email by design — the token lives on the
# pairing row, so an unpaired sender can never present a valid one.
#
# TRAP 3: because localmail is a private IP literal with a self-signed cert,
# KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA must key that literal, and per #492 the
# worker's allowlist must resolve to that SINGLE private origin.
```

Update the ROADMAP Phase-2 line 251 (the IMAP inbound item) to point at the new
spec and record slice 1, and refresh HANDOVER's "Next up" and current-state
blocks.

- [ ] **Step 4: Run the full workspace suite**

Run: `source "$HOME/.cargo/env" && cargo test --workspace -- --nocapture 2>&1 | tail -40`
Expected: PASS, no new failures. Check for `[SKIP]` lines: a green run with skips means the sandbox tests did not run, not that they passed.

Then: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean, exit 0.

- [ ] **Step 5: Commit**

```bash
git add core/src/main/email_boot.rs core/src/main.rs core/src/install/plan.rs
git add core/tests/email_channel_e2e.rs docs/devel/ROADMAP.md docs/devel/handovers/HANDOVER.md
git commit -m "feat(daemon): wire the email fallback channel behind config

Absent unless configured, so an unconfigured daemon is byte-identical. A
PARTIAL config aborts startup rather than skipping: a missing authserv-id would
reject every message and present as a delivery bug rather than a
misconfiguration.

The commented env block carries all three operator traps — the authserv-id must
match your MX exactly, pairing is operator-only via pair issue-token, and the
#492 single-private-origin rule applies to the localmail endpoint."
```

---

## Verification before opening the PR

- [ ] `cargo test --workspace -- --nocapture` green on the Mac, with `[SKIP]` lines checked rather than assumed.
- [ ] `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` green on the **DGX** (`ssh dgx '<cmd>'`). The DGX is authoritative for `cfg(linux)` code and runs the live-PG suites; the Mac is authoritative for macOS-gated items.
- [ ] Test count rises by exactly the number of new tests — nothing existing moved, which is the Matrix-parity claim.
- [ ] Both negative controls re-verified (Task 4 Step 5, Task 5 Step 5).
- [ ] Localmail PR merged first, since Task 7 targets its endpoints.
- [ ] PR body links #492 (the constraint that forced the two-worker split) and the ROADMAP Phase-2 slice #5 item.
