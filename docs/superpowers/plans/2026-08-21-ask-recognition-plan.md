# Ask Recognition Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **In THIS repo, override that default: implement as controller, dispatch subagents only for REVIEW.** Implementer subagents background `cargo` and wedge, and the explicit foreground instruction does not fix it (re-proved 2026-08-21, three stalls). See the memory note `subagent-foreground-cargo-tests`.

**Goal:** Make ask containment exact and peer-scoped, narrow the shape check to the job it is good at, drop the `<token>` metasyntax, and make `channel.ask_answer_rejected` say which arm refused.

**Architecture:** `handle_inbound` grows from two ask arms to four. The broad shape predicate survives as a cheap DB-free *gate* on a new containment arm that asks the exact question — does any whitespace token in this body hash to a live nonce owned by this peer's own asks? The shape check itself narrows to first-token-only and is re-aimed at the usage hint. One shared SQL `WHERE` fragment is bound by both `resolve_with_nonce` and the new existence query, so containment cannot drift narrower than resolution.

**Tech Stack:** Rust 2021, `sqlx` (Postgres), `async_trait`, `serde_json`, `tokio`. Tests are `cargo test`; PG-backed suites use `kastellan-tests-common`'s `PgCluster`.

**Spec:** [`docs/superpowers/specs/2026-08-21-ask-recognition-design.md`](../specs/2026-08-21-ask-recognition-design.md)

## Global Constraints

- **AGPL-3.0; AGPL-compatible dependencies only.** This slice adds **no** dependency.
- **No migration, no schema change.** `asks.nonce_sha256` and its index already exist.
- **Cross-platform.** No `cfg(target_os)` code in this slice. Every test must compile and run on both macOS and Linux.
- **Clippy is enforced:** `cargo clippy --workspace --all-targets -- -D warnings` must stay at exit 0. Local rust is 1.96.0 and CI is 1.97.0, so **treat CI as the authority on lints** and expect a possible one-line follow-up.
- **`cargo` is not on the non-interactive `PATH`:** every command below assumes `source "$HOME/.cargo/env"` has been run.
- **Files stay under 500 lines where feasible.** `core/src/channel/bus.rs` is at **658** and this slice adds to it; Task 5 keeps the addition small by putting the decision in one helper, and the file-split stays on the backlog rather than being folded in here (this repo splits *before* the change that grows a file, and that boat has sailed for this one).
- **Never echo a token.** No ack body, audit payload, or `tracing` field may carry a nonce or the message body. `Nonce`'s `Debug` prints `Nonce(<redacted>)`; keep it that way.
- **Exact copy for the ack**, verbatim from spec D9:
  `Usage: /approve TOKEN or /deny TOKEN — exactly the verb and the token, nothing else.`
  (the dash is U+2014, written `\u{2014}` in source, matching the existing constant.)
- **The four `reason` values, verbatim:** `unresolvable`, `carries_live_token`, `unscannable`, `malformed`.

---

### Task 1: Drop the `<token>` metasyntax (#583)

Smallest and fully independent — no other task depends on it, and it fixes a live UX trap on its own.

**Files:**
- Modify: `core/src/channel/ask_message.rs:58-60` (the `ACK_MALFORMED_COMMAND` constant and its doc)
- Test: `core/src/channel/ask_message.rs` (the existing `#[cfg(test)] mod tests` in the same file)

**Interfaces:**
- Consumes: nothing.
- Produces: `ACK_MALFORMED_COMMAND` keeps its name, type (`&'static str`) and every existing call site. Only the text changes.

- [ ] **Step 1: Write the failing test**

Add to the test module in `core/src/channel/ask_message.rs`:

```rust
/// Element parses `<token>` as an unknown HTML tag and **drops it from the
/// sender's own timeline**, so an operator who transcribes this hint
/// literally sends `/approve <token>` — two tokens, so it parses, so it
/// resolves nothing, so they get the deliberately vague
/// `ACK_NOT_ANSWERABLE` while their own screen shows only `/approve`.
/// The reply contradicts the message they can read back (#583).
///
/// A plain uppercase word survives HTML rendering intact and still reads as
/// a placeholder. Pinned here because the failure is invisible from inside
/// the process: every test passes, and only a real client shows it.
#[test]
fn the_usage_hint_carries_no_html_metasyntax() {
    assert!(
        !ACK_MALFORMED_COMMAND.contains('<'),
        "a `<...>` placeholder is eaten by Matrix clients: {ACK_MALFORMED_COMMAND}"
    );
    assert!(!ACK_MALFORMED_COMMAND.contains('>'));
    // Still teaches both verbs, or it is not a usage hint any more.
    assert!(ACK_MALFORMED_COMMAND.contains("/approve"));
    assert!(ACK_MALFORMED_COMMAND.contains("/deny"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kastellan-core --lib the_usage_hint_carries_no_html_metasyntax`
Expected: **FAIL** — `a `<...>` placeholder is eaten by Matrix clients: Usage: /approve <token> or /deny <token> — exactly the verb and the token, nothing else.`

- [ ] **Step 3: Write minimal implementation**

Replace the constant's value in `core/src/channel/ask_message.rs`:

```rust
pub const ACK_MALFORMED_COMMAND: &str =
    "Usage: /approve TOKEN or /deny TOKEN \u{2014} exactly the verb and the token, \
     nothing else.";
```

Append to that constant's existing doc comment (do not delete what is there):

```rust
/// **`TOKEN`, not `<token>`, and that is load-bearing (#583).** Element
/// parses an angle-bracketed placeholder as an unknown HTML tag and drops
/// it from the sender's own timeline. An operator who copied the old hint
/// literally sent a two-token command that parsed, resolved nothing, and
/// came back as the deliberately vague [`ACK_NOT_ANSWERABLE`] — while
/// their screen showed a one-token `/approve` that by the documented
/// design should have produced *this* sentence. The reply contradicted the
/// message they could read back, and the cause was invisible on both ends.
/// Pinned by `the_usage_hint_carries_no_html_metasyntax`.
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kastellan-core --lib ask_message`
Expected: PASS, including the pre-existing `ACK_MALFORMED_COMMAND` assertions around line 544 (`!contains("tok9")`, `!contains("thanks")`).

- [ ] **Step 5: Commit**

```bash
git add core/src/channel/ask_message.rs
git commit -m "fix(channel): the usage hint's <token> placeholder is eaten by Matrix clients

Closes #583."
```

---

### Task 2: The pure vocabulary — bounded candidates and a narrowed shape check

Pure functions only. No DB, no bus, no async. This is where everything decidable without I/O gets decided, matching the module's stated purpose.

**Files:**
- Modify: `core/src/channel/ask_message.rs` (add two consts and two functions; amend `looks_like_ask_command`'s doc)
- Test: `core/src/channel/ask_message.rs` (same file's test module)

**Interfaces:**
- Consumes: nothing.
- Produces, and Tasks 4 and 5 call these by exactly these names:
  - `pub const CANDIDATE_BYTE_CAP: usize = 65_536;`
  - `pub const CANDIDATE_TOKEN_CAP: usize = 1024;`
  - `pub fn candidate_tokens(body: &str) -> Option<Vec<String>>`
  - `pub fn is_command_shaped(body: &str) -> bool`
  - `looks_like_ask_command(body: &str) -> bool` — **unchanged behaviour**, doc amended.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `core/src/channel/ask_message.rs`:

```rust
/// The containment arm hashes every candidate, so the candidate set has to
/// be bounded — an inbound body is NOT bounded before enqueue
/// (`build_channel_task_payload` stores `msg.body` whole; `SCAN_BYTE_CAP`
/// bounds only screening).
#[test]
fn candidate_tokens_dedups_and_keeps_every_distinct_token() {
    let got = candidate_tokens("/approve ab ab cd").expect("under cap");
    assert_eq!(got.len(), 3, "duplicates collapse: {got:?}");
    for t in ["/approve", "ab", "cd"] {
        assert!(got.iter().any(|g| g == t), "missing {t}: {got:?}");
    }
}

/// No shape filter on candidates. Slice-2 D7 bans coupling to the nonce
/// encoding, and here a wrong filter yields a false NEGATIVE — an uncaught
/// live token — which is the dangerous direction. Hash everything.
#[test]
fn candidate_tokens_filters_nothing_by_shape() {
    let got = candidate_tokens("> **7f3a9c2e1b**, ok?").expect("under cap");
    assert!(
        got.iter().any(|g| g == "**7f3a9c2e1b**,"),
        "a token must reach the hash exactly as it arrived: {got:?}"
    );
}

#[test]
fn candidate_tokens_returns_none_over_the_token_cap() {
    let body = (0..CANDIDATE_TOKEN_CAP + 1).map(|i| format!("t{i}")).collect::<Vec<_>>().join(" ");
    assert!(candidate_tokens(&body).is_none(), "over cap must fail closed");
}

/// A body is arbitrary UTF-8, so the prefix cut must land on a char
/// boundary: both `String::truncate` and `&s[..n]` PANIC on a non-boundary
/// index, and a multi-byte character straddling the cap is exactly where
/// that lands.
#[test]
fn candidate_tokens_cuts_the_prefix_on_a_char_boundary() {
    let body = "\u{00e9}".repeat(CANDIDATE_BYTE_CAP); // 2 bytes each
    let got = candidate_tokens(&body);
    assert!(got.is_some(), "one long token is one candidate, not over the token cap");
}

/// The narrowed check. First token only — which is what a person TYPING a
/// command produces. A quoted reply or a mention pill is not someone typing
/// a command; containment catches the token in those regardless of shape.
#[test]
fn is_command_shaped_matches_a_typed_command_and_nothing_else() {
    for body in ["/approve x", "/deny x", "  /APPROVE  x  ", "/Deny", "/approve tok9 thanks!"] {
        assert!(is_command_shaped(body), "should be command-shaped: {body:?}");
    }
    for body in [
        "should I /approve the PR?",
        "> /approve 7f3a9c2e1b",
        "kastellan: /approve 7f3a9c2e1b",
        "",
        "hello",
    ] {
        assert!(!is_command_shaped(body), "should NOT be command-shaped: {body:?}");
    }
}

/// #582's whole point, at the pure layer: the two predicates now DISAGREE
/// on the false-positive body, and that disagreement is the fix. The broad
/// one still gates the containment check; only the narrow one reaches for
/// the usage hint.
#[test]
fn the_two_predicates_disagree_exactly_on_the_false_positive() {
    let body = "should I /approve the PR?";
    assert!(looks_like_ask_command(body), "still gates the containment check");
    assert!(!is_command_shaped(body), "but no longer earns the usage hint");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p kastellan-core --lib ask_message::tests::candidate_tokens`
Expected: **FAIL to compile** — `cannot find function 'candidate_tokens' in this scope`, and the same for `is_command_shaped`, `CANDIDATE_TOKEN_CAP`, `CANDIDATE_BYTE_CAP`.

- [ ] **Step 3: Write minimal implementation**

Add to `core/src/channel/ask_message.rs`, immediately after `looks_like_ask_command`:

```rust
/// How much of a body the containment arm reads when collecting candidate
/// tokens (spec D7).
///
/// Deliberately **not** `injection_guard::SCAN_BYTE_CAP`, even though both
/// are 64 KiB today: that one is the guard's document budget and answers a
/// different question, and sharing a constant couples two caps that should
/// be free to move independently.
pub const CANDIDATE_BYTE_CAP: usize = 65_536;

/// How many *distinct* candidate tokens the containment arm will hash
/// before refusing to answer (spec D7).
///
/// An inbound body is not bounded before enqueue, so without this a large
/// paste would hash unboundedly and ship a huge array to Postgres. Over the
/// cap the arm fails **closed** — the alternative, scanning a prefix and
/// enqueueing, is a silent false negative on a token past the cap, and a
/// silent miss is the failure this whole arm exists to prevent.
pub const CANDIDATE_TOKEN_CAP: usize = 1024;

/// The distinct whitespace tokens of a bounded prefix of `body`, or `None`
/// when there are more than [`CANDIDATE_TOKEN_CAP`] of them.
///
/// `None` means *"the containment question cannot be answered"* and the
/// caller must fail closed — it does not mean "no candidates".
///
/// **No shape filter, deliberately.** The slice-2 spec's D7 bans coupling
/// to the nonce encoding, and the argument is stronger here than there: a
/// filter that is wrong produces a false *negative*, i.e. a live token the
/// containment arm never hashes and therefore never catches. Tokens are
/// hashed exactly as they arrived; the index decides.
pub fn candidate_tokens(body: &str) -> Option<Vec<String>> {
    let prefix = bounded_prefix(body, CANDIDATE_BYTE_CAP);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for tok in prefix.split_whitespace() {
        if seen.insert(tok) {
            if out.len() == CANDIDATE_TOKEN_CAP {
                return None;
            }
            out.push(tok.to_string());
        }
    }
    Some(out)
}

/// The largest prefix of `body` that is at most `cap` bytes and ends on a
/// char boundary.
///
/// A body is arbitrary UTF-8 from a transport. Both `String::truncate` and
/// a bare `&body[..cap]` **panic** on a non-boundary index, and a
/// multi-byte character straddling the cap is precisely where that index
/// lands. The straddling character is dropped whole.
fn bounded_prefix(body: &str, cap: usize) -> &str {
    if body.len() <= cap {
        return body;
    }
    let mut end = cap;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    &body[..end]
}

/// True when the body's **first** whitespace token is `/approve` or
/// `/deny`, case-insensitively — i.e. when someone *typed a command*.
///
/// This is the narrowed successor to the UX half of
/// [`looks_like_ask_command`], and narrowing it is only safe because
/// containment no longer rests on it (spec D2). The two now answer
/// different questions:
///
/// - [`looks_like_ask_command`] — *might this body carry a live token?*
///   Broad, and only a **gate** on the exact check.
/// - `is_command_shaped` — *did a human just fumble a command?* Narrow,
///   and it decides the usage hint alone.
///
/// First-token-only is right for the second question because it is what a
/// person typing a command produces. A quoted reply, a mention pill or
/// prose mentioning `/approve` is not someone typing a command — and the
/// live token such a body may carry is caught by the containment arm
/// regardless of its shape.
///
/// Being wrong here now costs a missing usage hint, never a leaked token.
/// That is why widening *this* predicate is safe in a way widening the old
/// one was not.
pub fn is_command_shaped(body: &str) -> bool {
    body.split_whitespace().next().is_some_and(|first| {
        first.eq_ignore_ascii_case("/approve") || first.eq_ignore_ascii_case("/deny")
    })
}
```

Then **amend** `looks_like_ask_command`'s doc: delete the paragraph beginning
`**This whole predicate is a guess, and #582 replaces it with the exact
question**` and put this in its place —

```rust
/// **Demoted, not deleted (#582, spec D3).** This predicate no longer
/// decides anything on its own. It is the cheap, DB-free **gate** on the
/// containment arm: when it fires, `handle_inbound` asks the exact question
/// — does any token in the body hash to a live, peer-scoped nonce? — and
/// that answer, not this guess, decides whether the body may be enqueued.
///
/// Keeping it is what makes the exact check affordable: ordinary traffic
/// never reaches the database. And its false positive stopped costing
/// anything, which was #582's complaint — `should I /approve the PR?` fires
/// this gate, matches no live nonce, is not [`is_command_shaped`], and is
/// enqueued normally.
///
/// **Do not delete this function** believing #582 replaced it, and do not
/// widen it a third time. If a new *typed* shape needs recognising, widen
/// [`is_command_shaped`], which is the one that is safe to be wrong about.
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kastellan-core --lib ask_message`
Expected: PASS. The pre-existing `looks_like_ask_command` tables (around lines 482, 504, 529, 537) must stay green — its behaviour is unchanged.

- [ ] **Step 5: Commit**

```bash
git add core/src/channel/ask_message.rs
git commit -m "feat(channel): bounded candidate tokens and a narrowed command-shape check

The pure half of #582's split (spec D2/D3): looks_like_ask_command is
demoted to a gate, is_command_shaped takes over the usage hint."
```

---

### Task 3: The DB seam — one predicate, bound twice

**Files:**
- Modify: `db/src/asks.rs` (add the shared const; re-number `resolve_with_nonce`'s binds; add the new query)
- Test: `db/tests/asks_e2e.rs` (the dedicated PG-gated asks suite — **not** `postgres_e2e.rs`)

**Interfaces:**
- Consumes: `Nonce`, `Claimant`, `sha256_hex` — all already `pub` in `db/src/asks.rs`.
- Produces, called by Task 4 with exactly this signature:
  ```rust
  pub async fn any_live_nonce_for_claimant(
      pool: &PgPool,
      nonces: &[Nonce],
      claimant: &Claimant,
  ) -> Result<bool, DbError>
  ```

- [ ] **Step 1: Write the failing test**

Add to `db/tests/asks_e2e.rs`, using that file's own helpers — `harness(tag)`, `h.migrated_pool(purpose)`, and `channel_payload(peer)`, which builds the `kind: "channel"` payload the D16 `EXISTS` predicate reads:

```rust
/// D6's agreement test, and the reason it is PG-backed rather than a unit
/// test: the thing that can drift is the SQL, and only Postgres can say
/// whether two `WHERE` clauses select the same rows.
///
/// If `any_live_nonce_for_claimant` ever became NARROWER than
/// `resolve_with_nonce`, containment would miss a token that resolution
/// still accepts — the fail-open the containment arm exists to prevent,
/// reached through a copy-paste rather than through a logic error.
#[test]
fn containment_sees_exactly_what_resolution_accepts() {
    let Some(h) = harness("askcnt") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-containment").await;
        let pool = &pool;
        use kastellan_db::asks;
        use kastellan_db::tasks::{self, Lane};

        let owner = asks::Claimant::new("matrix", "@horst:kastellan.dev");
        let stranger = asks::Claimant::new("matrix", "@mallory:kastellan.dev");

        let task_id = tasks::insert_pending(
            pool, Lane::Fast, channel_payload("@horst:kastellan.dev"),
        ).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve", "deny"]), Some("digest1"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600), None,
        ).await.unwrap();
        let live = std::slice::from_ref(&raised.nonce);

        // Seen by containment while it is live...
        assert!(
            asks::any_live_nonce_for_claimant(pool, live, &owner).await.unwrap(),
            "a live nonce of this peer's own ask must be seen",
        );

        // ...and NOT seen when scoped to another peer. This is D5: an
        // unscoped check is the existence oracle D9 and D16 refuse to be,
        // and the nonce is five bytes.
        assert!(
            !asks::any_live_nonce_for_claimant(pool, live, &stranger).await.unwrap(),
            "peer scoping must hold, or the check becomes a token-guessing oracle",
        );

        // An unissued nonce is invisible even to its would-be owner.
        let unissued = asks::Nonce::from_wire("0".repeat(64));
        assert!(
            !asks::any_live_nonce_for_claimant(pool, std::slice::from_ref(&unissued), &owner)
                .await.unwrap(),
        );

        // Resolution accepts it for the owner — the agreement half.
        let resolved = asks::resolve_with_nonce(
            pool, &raised.nonce, &owner, &asks::resolution("approve", None),
        ).await.unwrap();
        assert!(resolved.is_some(), "resolution must accept what containment saw");

        // A SPENT token is not a capability, so containment stops seeing it
        // and a body carrying it is free to enqueue (spec D4).
        assert!(
            !asks::any_live_nonce_for_claimant(pool, live, &owner).await.unwrap(),
            "a resolved nonce is spent and must no longer be contained",
        );
    });
}

/// An empty candidate list must not issue a query at all, and must be
/// `false` rather than an error or a vacuous `true` — `EXISTS` over an
/// empty array would be `false` anyway, but the early return is what makes
/// "ordinary traffic pays nothing" true.
#[test]
fn containment_of_no_candidates_is_false() {
    let Some(h) = harness("askcn0") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-containment-empty").await;
        let who = kastellan_db::asks::Claimant::new("matrix", "@horst:kastellan.dev");
        assert!(
            !kastellan_db::asks::any_live_nonce_for_claimant(&pool, &[], &who).await.unwrap()
        );
    });
}

/// An EXPIRED ask is not live, so its token is not contained. Same
/// `deadline_at > now()` clause resolution carries, which is the point of
/// sharing the fragment.
#[test]
fn containment_ignores_an_expired_nonce() {
    let Some(h) = harness("askcnx") else {
        return;
    };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-containment-expired").await;
        let pool = &pool;
        use kastellan_db::asks;
        use kastellan_db::tasks::{self, Lane};

        let owner = asks::Claimant::new("matrix", "@horst:kastellan.dev");
        let task_id = tasks::insert_pending(
            pool, Lane::Fast, channel_payload("@horst:kastellan.dev"),
        ).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve", "deny"]), Some("digest1"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600), None,
        ).await.unwrap();

        // Push the deadline into the past rather than sleeping.
        sqlx::query("UPDATE asks SET deadline_at = now() - interval '1 second' WHERE id = $1")
            .bind(raised.ask_id)
            .execute(pool)
            .await
            .unwrap();

        assert!(
            !asks::any_live_nonce_for_claimant(pool, std::slice::from_ref(&raised.nonce), &owner)
                .await.unwrap(),
            "an expired ask's token is not a capability and must not be contained",
        );
    });
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kastellan-db --test asks_e2e containment_ -- --nocapture`
Expected: **FAIL to compile** — `cannot find function 'any_live_nonce_for_claimant' in module 'kastellan_db::asks'`.

> If it instead prints a `[SKIP]` line, Postgres is not reachable. On this Mac that means setting `KASTELLAN_PG_BIN_DIR` to the Postgres.app v18 bin dir; a skip is **not** a pass.

- [ ] **Step 3: Write minimal implementation**

In `db/src/asks.rs`, add the shared const above `resolve_with_nonce`:

```rust
/// The `WHERE` tail shared by [`resolve_with_nonce`] and
/// [`any_live_nonce_for_claimant`]: the ask is live, and its task belongs
/// to the claimant's own `(channel, peer)`.
///
/// **One fragment bound twice, never two hand-typed copies.** If
/// containment's predicate drifted narrower than resolution's, a token
/// resolution still accepts would stop being contained — the fail-open the
/// containment arm exists to prevent, arriving through a copy-paste. This
/// repo has already paid for that shape twice in one month
/// (`Confusion::is_valid` vs `invalidity`; `confusion_at` re-writing
/// `p >= tau` instead of calling `decide`).
///
/// **Both consumers must bind the claimant's channel to `$2` and peer to
/// `$3`.** That is why `resolve_with_nonce`'s other binds start at `$4`;
/// do not renumber one without the other.
const LIVE_ASK_FOR_CLAIMANT: &str = "state = 'pending' AND deadline_at > now() \
     AND EXISTS (SELECT 1 FROM tasks t \
                  WHERE t.id = asks.task_id \
                    AND t.payload->>'kind' = 'channel' \
                    AND t.payload->>'channel' = $2 \
                    AND t.payload->>'peer' = $3)";
```

Rewrite `resolve_with_nonce`'s query and binds (the SQL text changes, the behaviour does not — channel/peer move from `$4`/`$5` to `$2`/`$3` so the fragment can be shared):

```rust
    let row = sqlx::query(&format!(
        "UPDATE asks \
         SET state = 'resolved', \
             resolved_at = now(), \
             resolved_by = $4, \
             resolution = $5 \
         WHERE nonce_sha256 = $1 AND {LIVE_ASK_FOR_CLAIMANT} \
         RETURNING id, task_id, options"
    ))
    .bind(&nonce_hash)
    .bind(claimant.channel())
    .bind(claimant.peer())
    .bind(claimant.attribution())
    .bind(resolution)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| DbError::Query(format!("asks resolve_with_nonce: {e}")))?;
```

Add the new query beside it:

```rust
/// Does any of `nonces` hash to a **live** ask belonging to `claimant`'s
/// own task?
///
/// The containment question the channel bus asks before enqueueing a body
/// that mentions an ask verb (spec D5). One indexed `SELECT EXISTS` on
/// `asks.nonce_sha256`.
///
/// **Peer-scoped, and that is not an optimisation.** An unscoped existence
/// check is exactly the oracle D9 and D16 refuse to be: a paired peer could
/// probe five-byte token guesses and read the answer off whether their
/// message was refused or enqueued. The scope reuses
/// [`resolve_with_nonce`]'s own predicate — see [`LIVE_ASK_FOR_CLAIMANT`]
/// for why it is one fragment and not two.
///
/// Returns `false` for an empty slice without touching the database. A
/// **spent** or **expired** nonce is not live and therefore not contained,
/// which is deliberate: neither is a capability.
pub async fn any_live_nonce_for_claimant(
    pool: &PgPool,
    nonces: &[Nonce],
    claimant: &Claimant,
) -> Result<bool, DbError> {
    if nonces.is_empty() {
        return Ok(false);
    }
    let hashes: Vec<String> = nonces.iter().map(|n| sha256_hex(n.expose())).collect();

    let found: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM asks \
                         WHERE nonce_sha256 = ANY($1) AND {LIVE_ASK_FOR_CLAIMANT})"
    ))
    .bind(&hashes)
    .bind(claimant.channel())
    .bind(claimant.peer())
    .fetch_one(pool)
    .await
    .map_err(|e| DbError::Query(format!("asks any_live_nonce_for_claimant: {e}")))?;

    Ok(found)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kastellan-db -- --nocapture`
Expected: PASS, and **every pre-existing `resolve_with_nonce` test must stay green** — the bind renumbering is the risky part of this task, and those tests are what prove it was done right. Confirm zero `[SKIP]` lines for the asks suites.

- [ ] **Step 5: Commit**

```bash
git add db/src/asks.rs db/tests/asks_e2e.rs
git commit -m "feat(db): peer-scoped live-nonce existence, sharing resolve_with_nonce's predicate"
```

---

### Task 4: The trait seam

Keeps `bus/tests.rs` PG-free, which slice-2's D12 established and this slice does not revisit.

**Files:**
- Modify: `core/src/channel/bus.rs:69-112` (`AskResolver` trait + `PgAskResolver` impl)
- Modify: `core/src/channel/bus/tests.rs:55-97` (`RecordingResolver`, `FailingResolver`)

**Interfaces:**
- Consumes: `kastellan_db::asks::any_live_nonce_for_claimant` from Task 3.
- Produces, called by Task 5:
  ```rust
  async fn any_live_nonce(
      &self,
      nonces: &[kastellan_db::asks::Nonce],
      claimant: &kastellan_db::asks::Claimant,
  ) -> anyhow::Result<bool>;
  ```
  plus a test-only `RecordingResolver.live_nonces: std::collections::HashSet<String>` field controlling what it reports live.

- [ ] **Step 1: Write the failing test**

The compiler is the test here — a new required trait method breaks every implementor. Extend the fakes in `core/src/channel/bus/tests.rs` first, so the failure is the *production* impl missing:

```rust
#[derive(Default)]
struct RecordingResolver {
    calls: std::sync::Mutex<Vec<(String, String, String)>>, // (token, choice, attribution)
    reply: Option<kastellan_db::asks::ResolvedAsk>,
    /// Plaintext tokens this resolver reports as live, peer-scoped nonces.
    /// A `HashSet` rather than a bool so a test can prove the containment
    /// arm hashed the *body's own* tokens and not something else.
    live_nonces: std::collections::HashSet<String>,
    /// Records what the containment arm actually asked about.
    nonce_queries: std::sync::Mutex<Vec<Vec<String>>>,
}
```

and add to its `impl AskResolver`:

```rust
    async fn any_live_nonce(
        &self,
        nonces: &[kastellan_db::asks::Nonce],
        _claimant: &kastellan_db::asks::Claimant,
    ) -> anyhow::Result<bool> {
        let seen: Vec<String> = nonces.iter().map(|n| n.expose().to_string()).collect();
        let hit = seen.iter().any(|t| self.live_nonces.contains(t));
        self.nonce_queries.lock().unwrap().push(seen);
        Ok(hit)
    }
```

and to `FailingResolver`:

```rust
    async fn any_live_nonce(
        &self,
        _nonces: &[kastellan_db::asks::Nonce],
        _claimant: &kastellan_db::asks::Claimant,
    ) -> anyhow::Result<bool> {
        anyhow::bail!("simulated db outage")
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p kastellan-core --lib channel::bus`
Expected: **FAIL to compile** — `not all trait items implemented, missing 'any_live_nonce'` for `PgAskResolver` (and `method 'any_live_nonce' is not a member of trait 'AskResolver'` until the trait declares it).

- [ ] **Step 3: Write minimal implementation**

Add to the `AskResolver` trait in `core/src/channel/bus.rs`:

```rust
    /// Does any of `nonces` hash to a live ask owned by `claimant`'s own
    /// task? The containment question, asked before a body that mentions
    /// an ask verb is allowed anywhere near `screen_and_classify`.
    ///
    /// On the trait rather than reached for directly because
    /// `bus/tests.rs` is deliberately PG-free (spec D12) — and because an
    /// `Err` here must be distinguishable from `Ok(false)`, since the two
    /// take opposite arms: an unanswered question refuses, a definite
    /// "no" enqueues.
    async fn any_live_nonce(
        &self,
        nonces: &[kastellan_db::asks::Nonce],
        claimant: &kastellan_db::asks::Claimant,
    ) -> anyhow::Result<bool>;
```

and to `impl AskResolver for PgAskResolver`:

```rust
    async fn any_live_nonce(
        &self,
        nonces: &[kastellan_db::asks::Nonce],
        claimant: &kastellan_db::asks::Claimant,
    ) -> anyhow::Result<bool> {
        Ok(kastellan_db::asks::any_live_nonce_for_claimant(&self.pool, nonces, claimant).await?)
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kastellan-core --lib channel::bus`
Expected: PASS — behaviour is unchanged so far; nothing calls the new method yet.

- [ ] **Step 5: Commit**

```bash
git add core/src/channel/bus.rs core/src/channel/bus/tests.rs
git commit -m "feat(channel): AskResolver gains the containment question"
```

---

### Task 5: The four arms and the `reason` field

The behavioural change. Everything before this was scaffolding.

**Files:**
- Modify: `core/src/channel/mod.rs:205-232` (the `ASK_ANSWER_REJECTED` doc, which currently documents three producers with no way to tell them apart; add the four reason consts)
- Modify: `core/src/channel/bus.rs:303-381` (arm 1's audit payload; replace the `looks_like_ask_command` block with the containment + hint arms)
- Test: `core/src/channel/bus/tests.rs`

**Interfaces:**
- Consumes: Task 2's `candidate_tokens` / `is_command_shaped` / `looks_like_ask_command`; Task 4's `AskResolver::any_live_nonce`.
- Produces: `channel.ask_answer_rejected` payloads now carry `reason`. Task 6 asserts on them.

- [ ] **Step 1: Write the failing tests**

Add to `core/src/channel/bus/tests.rs`, using that file's existing helpers — do **not** write new setup. The exact shapes, read from the file:

```rust
handle_inbound(&auth, pairing, asks, &events, &msg) -> Option<OutgoingMessage>
//             ^&dyn PeerAuthorizer
//                    ^Option<&dyn PairingService>
//                             ^Option<&AskWiring>
fn msg(peer: &str, body: &str) -> IncomingMessage      // channel "matrix", conversation "!room:srv"
struct FakeEvents { enqueued: Mutex<Vec<(Lane, Value)>>, audited: Mutex<Vec<(String, Value)>> }
let auth = StaticPairings::from_peers([PeerId("@me:srv".into())]);
fn wiring(resolver: Arc<RecordingResolver>) -> Arc<AskWiring>
```

```rust
/// #582's point, and the test that fails before this task: an ordinary
/// instruction that merely MENTIONS a verb carries no live token, so it is
/// a task, not an answer. Before the split it was refused with the usage
/// ack — and on email that refusal is dropped silently, so the message
/// simply vanished.
#[tokio::test]
async fn an_ordinary_message_mentioning_a_verb_is_enqueued() {
    let resolver = Arc::new(RecordingResolver::default()); // no live nonces
    let ev = FakeEvents::default();
    let auth = StaticPairings::from_peers([PeerId("@me:srv".into())]);
    let out = handle_inbound(
        &auth,
        None,
        Some(&*wiring(resolver)),
        &ev,
        &msg("@me:srv", "should I /approve the PR?"),
    )
    .await;

    assert!(out.is_none(), "an enqueued message gets no ack");
    assert_eq!(ev.enqueued.lock().unwrap().len(), 1, "must become a task");
    assert!(
        !ev.audited.lock().unwrap().iter().any(|(a, _)| a == actions::ASK_ANSWER_REJECTED),
        "no rejection row: nothing was rejected"
    );
}

/// The containment arm, catching a shape the narrow check cannot see. A
/// quoted reply is the most natural way to answer an ask, and Element's
/// rich-reply fallback quotes the rendered ask INCLUDING both command
/// lines — so the live token is in the body while the first token is `>`.
#[tokio::test]
async fn a_quoted_reply_carrying_a_live_token_is_contained() {
    let mut r = RecordingResolver::default();
    r.live_nonces.insert("7f3a9c2e1b".to_string());
    let resolver = Arc::new(r);
    let ev = FakeEvents::default();
    let auth = StaticPairings::from_peers([PeerId("@me:srv".into())]);

    let ack = handle_inbound(
        &auth,
        None,
        Some(&*wiring(resolver.clone())),
        &ev,
        &msg("@me:srv", "> Approval needed\n> /approve 7f3a9c2e1b\nyes please"),
    )
    .await
    .expect("an ack is returned");

    assert_eq!(ack.body, crate::channel::ask_message::ACK_MALFORMED_COMMAND);
    assert!(
        ev.enqueued.lock().unwrap().is_empty(),
        "a live token must never reach tasks.payload"
    );
    let audited = ev.audited.lock().unwrap();
    assert_eq!(audited[0].0, actions::ASK_ANSWER_REJECTED);
    assert_eq!(audited[0].1["reason"], actions::ASK_REASON_CARRIES_LIVE_TOKEN);
    // It hashed the body's OWN tokens — not, say, only the first one.
    let asked = &resolver.nonce_queries.lock().unwrap()[0];
    assert!(asked.iter().any(|t| t == "7f3a9c2e1b"), "asked about: {asked:?}");
}

/// The UX arm. `/deny` alone is the second message of the 2026-08-20 live
/// test, and it is exactly the body a pure exact-nonce check would have
/// enqueued silently (spec D2).
#[tokio::test]
async fn a_typed_command_with_no_token_gets_the_usage_hint() {
    let resolver = Arc::new(RecordingResolver::default()); // no live nonces
    let ev = FakeEvents::default();
    let auth = StaticPairings::from_peers([PeerId("@me:srv".into())]);
    let ack = handle_inbound(&auth, None, Some(&*wiring(resolver)), &ev, &msg("@me:srv", "/deny"))
        .await
        .expect("an ack is returned");

    assert_eq!(ack.body, crate::channel::ask_message::ACK_MALFORMED_COMMAND);
    assert!(ev.enqueued.lock().unwrap().is_empty(), "a fumbled command must not become a task");
    assert_eq!(ev.audited.lock().unwrap()[0].1["reason"], actions::ASK_REASON_MALFORMED);
}

/// Containment precedes the hint, so a live token in a malformed command
/// audits the security-relevant cause. Both give the same ack; only the row
/// differs, and `carries_live_token` is the only row that ever shows the
/// containment guard doing its job.
#[tokio::test]
async fn containment_outranks_the_usage_hint() {
    let mut r = RecordingResolver::default();
    r.live_nonces.insert("tok9".to_string());
    let ev = FakeEvents::default();
    let auth = StaticPairings::from_peers([PeerId("@me:srv".into())]);
    handle_inbound(
        &auth,
        None,
        Some(&*wiring(Arc::new(r))),
        &ev,
        &msg("@me:srv", "/approve tok9 thanks!"),
    )
    .await;

    assert_eq!(
        ev.audited.lock().unwrap()[0].1["reason"],
        actions::ASK_REASON_CARRIES_LIVE_TOKEN
    );
}

/// Fail closed on a question we could not answer (spec D7). `Ok(false)` and
/// `Err` take OPPOSITE arms, which is why the trait returns a `Result` and
/// not a bool.
#[tokio::test]
async fn a_failed_containment_check_refuses_rather_than_enqueues() {
    let ev = FakeEvents::default();
    let auth = StaticPairings::from_peers([PeerId("@me:srv".into())]);
    let w = AskWiring {
        outbox: Arc::new(ChannelOutbox::new()),
        resolver: Arc::new(FailingResolver),
    };
    let ack = handle_inbound(&auth, None, Some(&w), &ev, &msg("@me:srv", "please /approve this"))
        .await
        .expect("an ack is returned");

    assert_eq!(ack.body, crate::channel::ask_message::ACK_MALFORMED_COMMAND);
    assert!(
        ev.enqueued.lock().unwrap().is_empty(),
        "an unanswered containment question must never enqueue"
    );
    assert_eq!(ev.audited.lock().unwrap()[0].1["reason"], actions::ASK_REASON_UNSCANNABLE);
}

/// Over the candidate cap, same posture, same reason — and the database is
/// never asked, since the point of the cap is not to ask it.
#[tokio::test]
async fn a_body_over_the_candidate_cap_refuses() {
    use crate::channel::ask_message::CANDIDATE_TOKEN_CAP;
    let body = format!(
        "/approve {}",
        (0..CANDIDATE_TOKEN_CAP + 1).map(|i| format!("t{i}")).collect::<Vec<_>>().join(" ")
    );
    let resolver = Arc::new(RecordingResolver::default());
    let ev = FakeEvents::default();
    let auth = StaticPairings::from_peers([PeerId("@me:srv".into())]);
    handle_inbound(&auth, None, Some(&*wiring(resolver.clone())), &ev, &msg("@me:srv", &body)).await;

    assert!(ev.enqueued.lock().unwrap().is_empty());
    assert_eq!(ev.audited.lock().unwrap()[0].1["reason"], actions::ASK_REASON_UNSCANNABLE);
    assert!(resolver.nonce_queries.lock().unwrap().is_empty(), "must not query over cap");
}

/// D7's no-wiring fallback: containment cannot be answered without a
/// resolver, so the broad predicate alone decides and the row says
/// `unscannable` — NOT `malformed`, which would claim a syntax judgement
/// nothing made.
#[tokio::test]
async fn an_unwired_bus_still_refuses_on_the_broad_predicate() {
    let ev = FakeEvents::default();
    let auth = StaticPairings::from_peers([PeerId("@me:srv".into())]);
    let ack = handle_inbound(&auth, None, None, &ev, &msg("@me:srv", "> /approve 7f3a9c2e1b"))
        .await
        .expect("an ack is returned");

    assert_eq!(ack.body, crate::channel::ask_message::ACK_MALFORMED_COMMAND);
    assert!(ev.enqueued.lock().unwrap().is_empty());
    assert_eq!(ev.audited.lock().unwrap()[0].1["reason"], actions::ASK_REASON_UNSCANNABLE);
}

/// Arm 1 keeps its deliberate collapse and gains only the field.
#[tokio::test]
async fn an_unresolvable_answer_says_so_without_saying_why() {
    let resolver = Arc::new(RecordingResolver::default()); // reply: None
    let ev = FakeEvents::default();
    let auth = StaticPairings::from_peers([PeerId("@me:srv".into())]);
    let ack = handle_inbound(
        &auth,
        None,
        Some(&*wiring(resolver)),
        &ev,
        &msg("@me:srv", "/approve 7f3a9c2e1b"),
    )
    .await
    .expect("an ack is returned");

    assert_eq!(ack.body, crate::channel::ask_message::ACK_NOT_ANSWERABLE);
    assert_eq!(ev.audited.lock().unwrap()[0].1["reason"], actions::ASK_REASON_UNRESOLVABLE);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p kastellan-core --lib channel::bus`
Expected: **FAIL** — `ASK_REASON_*` unresolved; and once those exist, `an_ordinary_message_mentioning_a_verb_is_enqueued` fails on `assert!(ev.enqueued())` because today's broad predicate refuses it.

- [ ] **Step 3: Write minimal implementation**

Add the four consts to the `actions` module in `core/src/channel/mod.rs`, and **replace** `ASK_ANSWER_REJECTED`'s "Deliberately does not say which" paragraph, which is no longer true:

```rust
    /// Why an answer was refused. A **closed** four-value vocabulary on
    /// [`ASK_ANSWER_REJECTED`], added by #584.
    ///
    /// **One action with a field, not four actions** — observation SQL
    /// grouping on `action` must keep seeing one population by default.
    ///
    /// **This leaks nothing.** The row lands in `audit_log`, which is
    /// role-gated and operator-queried; the peer sees only the ack body,
    /// and the containment and malformed arms deliberately share one ack
    /// so the peer cannot tell them apart.
    pub const ASK_REASON_UNRESOLVABLE: &str = "unresolvable";
    /// A token in the body is a live nonce of one of this peer's own asks,
    /// so the body was kept out of the enqueue path. **The only row that
    /// ever shows the containment guard firing**, which makes it the most
    /// operationally valuable of the four.
    pub const ASK_REASON_CARRIES_LIVE_TOKEN: &str = "carries_live_token";
    /// The containment question could not be answered — over the candidate
    /// cap, a database error, or no resolver wired — so the body was
    /// refused. Fail-closed; the daemon log carries which trigger fired.
    pub const ASK_REASON_UNSCANNABLE: &str = "unscannable";
    /// The body's first token is one of the two verbs but it did not
    /// parse. A syntax error by a human, carrying no live token.
    pub const ASK_REASON_MALFORMED: &str = "malformed";
```

Replace that stale paragraph with:

```rust
    /// **Which arm refused is now recorded** in the payload's `reason`
    /// (#584) — see [`ASK_REASON_UNRESOLVABLE`] and its three siblings.
    /// Before that field existed, all producers wrote an identical payload
    /// and the row could not answer the one question anybody asked of it:
    /// diagnosing #583 needed `strings` on the deployed binary plus a
    /// second hand-run experiment in Element.
    ///
    /// **What stays collapsed, deliberately:** within
    /// `ASK_REASON_UNRESOLVABLE`, a wrong token, an already-answered ask,
    /// one past its deadline, "not this peer's ask", and a resolver `Err`
    /// are ONE outcome by construction (`db::asks::resolve_with_nonce`).
    /// Splitting them hands a token-guessing peer an existence oracle, and
    /// an error path that looks different to the peer is that same oracle
    /// by another door. Do not add a fifth value to separate them.
```

In `core/src/channel/bus.rs`, add `"reason": actions::ASK_REASON_UNRESOLVABLE` to arm 1's existing rejection payload, then **replace** the whole `if super::ask_message::looks_like_ask_command(&msg.body) { … }` block (lines ~355-381) with:

```rust
    // Arms 2 and 3 (spec D4). Containment first, so a live token in a
    // malformed command audits the security-relevant cause; both arms give
    // the same ack, so the peer cannot tell them apart.
    //
    // Deliberately OUTSIDE the `if let Some(wiring)` above: containment is
    // a property of the inbound path, not of this bus's configuration.
    let refusal = containment_refusal(msg, asks).await.or_else(|| {
        super::ask_message::is_command_shaped(&msg.body)
            .then_some(actions::ASK_REASON_MALFORMED)
    });
    if let Some(reason) = refusal {
        events
            .audit(
                actions::ASK_ANSWER_REJECTED,
                serde_json::json!({"channel": msg.channel.0, "peer": msg.peer.0, "reason": reason}),
            )
            .await;
        return Some(OutgoingMessage {
            channel: msg.channel.clone(),
            peer: msg.peer.clone(),
            conversation: msg.conversation.clone(),
            body: super::ask_message::ACK_MALFORMED_COMMAND.to_string(),
        });
    }
```

and add the helper beside `handle_inbound`:

```rust
/// The containment arm: may this body be enqueued, or does it carry a live
/// approval token?
///
/// `Some(reason)` refuses; `None` means the question was asked and the
/// answer was no. Enqueueing a live token writes it verbatim into
/// `tasks.payload` — a durable column with no DELETE grant — and hands it
/// to the planner as an instruction.
///
/// **Three fail-closed edges** (spec D7), all reported as `unscannable`
/// because all three mean *we could not answer*, not *we judged the
/// syntax*: no resolver wired, more distinct tokens than the cap, and a
/// resolver error. The last is why `any_live_nonce` returns a `Result` —
/// `Ok(false)` enqueues and `Err` refuses, so collapsing them into a bool
/// would silently pick the wrong arm on every database hiccup.
async fn containment_refusal(
    msg: &IncomingMessage,
    asks: Option<&AskWiring>,
) -> Option<&'static str> {
    // The cheap DB-free gate (spec D3): ordinary traffic never reaches the
    // database, which is what makes the exact check affordable.
    if !super::ask_message::looks_like_ask_command(&msg.body) {
        return None;
    }
    // NOT `asks?` and NOT `candidate_tokens(..)?`. Both would return
    // `None`, and `None` here means ENQUEUE — the fail-open this arm
    // exists to prevent, wearing idiomatic Rust. Explicit `let else`, so
    // the refusing branch is written out and cannot be mistaken for
    // shorthand.
    let Some(wiring) = asks else {
        return Some(actions::ASK_REASON_UNSCANNABLE);
    };
    let Some(tokens) = super::ask_message::candidate_tokens(&msg.body) else {
        return Some(actions::ASK_REASON_UNSCANNABLE);
    };
    let nonces: Vec<_> =
        tokens.into_iter().map(kastellan_db::asks::Nonce::from_wire).collect();
    let claimant = kastellan_db::asks::Claimant::new(msg.channel.0.clone(), msg.peer.0.clone());
    match wiring.resolver.any_live_nonce(&nonces, &claimant).await {
        Ok(true) => Some(actions::ASK_REASON_CARRIES_LIVE_TOKEN),
        Ok(false) => None,
        Err(e) => {
            // Logged, never audited: `DbError` renders query context, and
            // a durable operator-queried row is the wrong place for it.
            warn!(error = %e, "ask containment check failed; refusing");
            Some(actions::ASK_REASON_UNSCANNABLE)
        }
    }
}
```

> **Why the two `let else` blocks are spelled out above rather than written as `?`.** In a function returning `Option<&'static str>`, `asks?` and `candidate_tokens(..)?` both propagate `None` — and `None` here means **enqueue**. The `?` is shorter, reads as idiomatic, and silently inverts both fail-closed edges. This is the exact defect shape the last three review waves on this feature kept finding: a one-token change that disables a control while the suite stays green. Two of Step 5's mutations exist to catch it.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kastellan-core --lib channel`
Expected: PASS. Two pre-existing tests will need their expectations updated rather than deleted — the ones around `bus/tests.rs:781` and `:855` that assert `ACK_MALFORMED_COMMAND` for bodies that are no longer command-shaped. **Update them to assert the new arm and reason; do not delete a test to make a suite green.**

- [ ] **Step 5: Run the mutation checks**

Each must fail a named test. Revert by **copying the file back**, never `git checkout --` (that restores the committed version and eats uncommitted edits in the same file).

```
drop `.bind(claimant.peer())` scoping from the SQL  -> containment_sees_exactly_what_resolution_accepts
swap arms 2 and 3                                    -> containment_outranks_the_usage_hint
`Err(_) => None` in containment_refusal              -> a_failed_containment_check_refuses_rather_than_enqueues
`asks?` instead of the `let else`                    -> an_unwired_bus_still_refuses_on_the_broad_predicate
is_command_shaped -> looks_like_ask_command          -> an_ordinary_message_mentioning_a_verb_is_enqueued
```

- [ ] **Step 6: Commit**

```bash
git add core/src/channel/bus.rs core/src/channel/bus/tests.rs core/src/channel/mod.rs
git commit -m "feat(channel): exact peer-scoped containment, and a reason on every rejection

Closes #582, closes #584."
```

---

### Task 6: The PG-backed end-to-end leg

`bus/tests.rs` proves the arms against a fake. This proves them against real Postgres, through the real `PgAskResolver` — the only place the trait impl and the SQL meet.

**Files:**
- Modify: `core/tests/channel_bus_pg_e2e.rs` (it already asserts `ACK_NOT_ANSWERABLE` and `ASK_ANSWER_REJECTED` around lines 583-595)

**Interfaces:**
- Consumes: everything from Tasks 1-5.
- Produces: nothing further depends on this.

- [ ] **Step 1: Write the failing test**

Add to `core/tests/channel_bus_pg_e2e.rs`, modelled on the D16 bystander test already in that file (`probe_and_pool`, `StaticPairings::from_peers`, `PgChannelEvents`, `build_channel_task_payload`, `audit::fetch_since`):

```rust
/// The whole slice through the real resolver against real Postgres. The
/// unit tests use a fake, so this is the only leg where `PgAskResolver`,
/// the shared SQL predicate and the four arms meet.
#[tokio::test]
async fn containment_holds_end_to_end_against_real_postgres() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return; // skip-as-pass
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ac-d",
        "ac-l",
        &format!("kastellan-supervisor-test-pg-ac-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    let channel = ChannelId("matrix".into());
    let owner = PeerId("@me:srv".into());
    let conversation = ConversationId("!room:srv".into());
    let payload = build_channel_task_payload(&IncomingMessage {
        channel: channel.clone(),
        peer: owner.clone(),
        conversation: conversation.clone(),
        body: "book the flight".into(),
        evidence: None,
    });
    let task_id = tasks::insert_pending(&pool, Lane::Fast, payload).await.expect("insert");
    tasks::claim_one(&pool, Lane::Fast, 60).await.expect("claim").expect("a task");

    let raised = kastellan_db::asks::raise(
        &pool,
        task_id,
        "plan_approval",
        "sends money to a stranger",
        &serde_json::json!(["approve", "deny"]),
        Some("digest"),
        time::OffsetDateTime::now_utc() + time::Duration::seconds(600),
        None,
    )
    .await
    .expect("raise");
    let token = raised.nonce.expose().to_string();

    let events = PgChannelEvents::new(pool.clone());
    let authorizer = StaticPairings::from_peers([owner.clone()]);
    let wiring = AskWiring {
        outbox: std::sync::Arc::new(ChannelOutbox::new()),
        resolver: std::sync::Arc::new(PgAskResolver::new(pool.clone())),
    };
    let deliver = |body: String| {
        let (channel, peer, conversation) =
            (channel.clone(), owner.clone(), conversation.clone());
        IncomingMessage { channel, peer, conversation, body, evidence: None }
    };

    // (a) A quoted reply carrying the LIVE token. The first token is `>`,
    // so the narrow shape check cannot see it — only the exact check can.
    let quoted = deliver(format!("> Approval needed\n> /approve {token}\nyes, go ahead"));
    let ack = handle_inbound(&authorizer, None, Some(&wiring), &events, &quoted)
        .await
        .expect("a refusal is acknowledged");
    assert_eq!(ack.body, kastellan_core::channel::ask_message::ACK_MALFORMED_COMMAND);

    let audits = kastellan_db::audit::fetch_since(&pool, 0, 500).await.expect("audit fetch");
    let row = audits
        .iter()
        .rev()
        .find(|r| r.action == actions::ASK_ANSWER_REJECTED)
        .expect("a rejection row");
    assert_eq!(
        row.payload["reason"],
        actions::ASK_REASON_CARRIES_LIVE_TOKEN,
        "the only row that ever shows the containment guard firing",
    );

    // The token must not exist anywhere in `tasks` — the durable,
    // DELETE-less column this whole arm exists to keep it out of.
    let leaked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tasks WHERE payload::text LIKE $1")
            .bind(format!("%{token}%"))
            .fetch_one(&pool)
            .await
            .expect("scan tasks");
    assert_eq!(leaked, 0, "a live token must never reach tasks.payload");

    // (b) #582: an ordinary message that merely MENTIONS a verb carries no
    // live token, so it is a task. This is the arm that was refused before
    // the split — and on email, dropped silently.
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks")
        .fetch_one(&pool)
        .await
        .expect("count");
    let ordinary = deliver("should I /approve the PR?".to_string());
    let out = handle_inbound(&authorizer, None, Some(&wiring), &events, &ordinary).await;
    assert!(out.is_none(), "an enqueued message gets no ack");
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(after, before + 1, "#582: no live token, so it is a task");

    pool.close().await;
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kastellan-core --test channel_bus_pg_e2e -- --nocapture`
Expected: FAIL. A `[SKIP]` is **not** a pass — set `KASTELLAN_PG_BIN_DIR` on the Mac, or run it on the DGX.

- [ ] **Step 3: Implementation**

None. Tasks 1-5 already implement the behaviour; if this test fails, the defect is in them, not here.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kastellan-core --test channel_bus_pg_e2e -- --nocapture`
Expected: PASS, zero `[SKIP]`.

- [ ] **Step 5: Commit**

```bash
git add core/tests/channel_bus_pg_e2e.rs
git commit -m "test(channel): containment end-to-end through the real resolver"
```

---

## Final gate (not a task — do this before opening the PR)

- [ ] `cargo test --workspace -- --nocapture` on the **DGX** (authoritative; real bwrap + live PG 18). Predict the count from the diff's new `#[test]` count first, then reconcile the delta **exactly** — a miss means a test is not being compiled, which is the failure the platform split produces. Baseline is **3599 / 0 / 54**.
- [ ] Confirm the only `[SKIP]` lines are the four `KASTELLAN_GLINER_RELEX_ENABLE` ones. A bwrap-userns skip means containment did not run.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` from a **fresh** `CARGO_TARGET_DIR`, and **count the `Checking` lines** — a warm dir reports a full-workspace pass it never ran. Honest is ~217 crates; the affected closure here is `db`, `core`, `tests-common`.
- [ ] Mac leg: `cargo test -p kastellan-core --lib channel` plus the two PG-backed suites under `KASTELLAN_PG_BIN_DIR`. No `cfg(target_os)` code in this diff, so a full Mac sweep is not required.
- [ ] `wc -l core/src/channel/bus.rs` — report it in the PR body. It was 658 before this slice.
