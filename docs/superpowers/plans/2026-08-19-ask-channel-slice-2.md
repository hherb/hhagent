# The ask channel (#564 slice 2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An escalated plan's question reaches the operator in their Matrix room, and they answer it there with `/approve <token>` or `/deny <token>`.

**Architecture:** A shared `Arc<ChannelOutbox>` registry, created in `main` before both the scheduler and the channel supervisors, is the core-initiated-outbound primitive: the bus registers its per-channel sender into it, the scheduler pushes into it from the ask-raise path. Inbound, a strict pure parser recognises the two commands after peer authorization and before injection screening, and resolves through `db::asks::resolve_with_nonce`, whose guarded UPDATE now also requires the claimant to be the task's own peer. Delivery is best-effort and never fails an already-committed ask.

**Tech Stack:** Rust 2021, tokio, sqlx/Postgres, `async_trait`. No new dependencies. No migration.

**Spec:** [`docs/superpowers/specs/2026-08-19-ask-channel-slice-2-design.md`](../specs/2026-08-19-ask-channel-slice-2-design.md) — read it before Task 1; every task below cites the decision (D1…D17) it implements.

## Global Constraints

- **Cargo is not on the non-interactive `PATH`.** Every task starts with `source "$HOME/.cargo/env"`.
- **Run cargo in the FOREGROUND.** Never background a `cargo test`/`cargo clippy` and poll it.
- **Clippy is enforced:** `cargo clippy --workspace --all-targets -- -D warnings` must stay at zero.
- **Stage specific files.** `git add <paths>` — never `git add -A`.
- **Cross-platform:** no Linux-only or macOS-only code. Everything in this plan is `cfg`-free.
- **The plaintext nonce must never reach:** an audit payload, a log line, a `Debug` render, or any DB column other than nothing at all. Only `Nonce::expose()` may produce it, and only into a message body.
- **Audit reason strings are fixed labels**, never interpolated from input. Same rule as `auth::UnauthenticReason::as_str`.
- **File-size guidance:** aim under 500 lines. Task 0 exists because `scheduler/asks.rs` is already at 801.
- **Exact vocabulary**, used verbatim throughout: the wire verbs are `/approve` and `/deny`; the stored choices are `"approve"` and `"deny"`; `asks.options` is `["approve","deny"]`.

---

## Task 0: Split `scheduler/asks.rs` — movement only

**Why first:** the file is 801 lines and this slice grows it. This repo's rule is to split *before* the change that grows a file, so the movement diff is reviewable alone (spec D15; `boot_supervisor/tests/` is the worked precedent).

**This task adds no behaviour and no test.** Its verification is that the test count is *identical* before and after.

**Files:**
- Delete: `core/src/scheduler/asks.rs`
- Create: `core/src/scheduler/asks/mod.rs`
- Create: `core/src/scheduler/asks/pure.rs`
- Create: `core/src/scheduler/asks/lifecycle.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: the module path `crate::scheduler::asks::*` must keep resolving **exactly** as it does today. Every existing import (`crate::scheduler::asks::{resolution_choice, Choice}` in `runner/task_exec.rs`; `asks::raise_and_suspend`, `asks::decide`, `asks::AskDecision`, `asks::resume_state_from`, `asks::emit_approval_applied` in `inner_loop.rs`; `super::asks::sweep_expired_and_audit` in `runner.rs`) must compile untouched.

- [ ] **Step 1: Record the baseline test count**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib scheduler::asks:: 2>&1 | tail -3
```

Expected: `test result: ok. 15 passed`. Write the number down; Step 5 must match it.

- [ ] **Step 2: Create the three files by moving lines verbatim**

`core/src/scheduler/asks/pure.rs` gets the current lines 1–252 (module doc adapted, `Choice`, `AskDecision`, the three `pub const`s, `resolution_choice`, `decide`, `ask_deadline_seconds`, `deadline_from_env`, `RestoredRun`, `resume_state_from`, `restore_resume_state`, `string_list`) **and the entire `#[cfg(test)] mod tests` block from line 505 to the end** — every one of those 15 tests exercises the pure half only.

Header for `pure.rs`:

```rust
//! The pure half of the operator-ask path — reading a resolved ask into a
//! decision, and the resume-state codec.
//!
//! Pure and sync, so the rules the `Escalate` arm depends on have unit
//! tests rather than being reachable only through a Postgres e2e.
//!
//! Spec: `docs/superpowers/specs/2026-08-18-ask-path-slice-1b-design.md`.

use kastellan_db::asks::Ask;

use crate::scheduler::inner_loop::{PlanRecord, StepOutcome};
```

`core/src/scheduler/asks/lifecycle.rs` gets the current lines 254–504 (`raise_and_suspend`, `emit_ask_raised`, `emit_approval_applied`, `sweep_expired_and_audit`, `emit_expired_task_rows`) with this header:

```rust
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
use super::super::audit::{
    // keep the existing `use super::audit::{...}` list verbatim, re-rooted
};
```

Note: the current file's `use super::audit::{...}` resolves from `scheduler::asks`; from `scheduler::asks::lifecycle` the same items are `super::super::audit::{...}` — or, clearer, `crate::scheduler::audit::{...}`. Use the `crate::` form.

`core/src/scheduler/asks/mod.rs`:

```rust
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
```

- [ ] **Step 3: Delete the old file**

```bash
git rm core/src/scheduler/asks.rs
```

- [ ] **Step 4: Build**

```bash
source "$HOME/.cargo/env"
cargo build -p kastellan-core 2>&1 | tail -20
```

Expected: clean. If any call site needs an import change, the split is wrong — fix the re-exports in `mod.rs`, not the call sites. The one legitimate exception is `emit_approval_applied`, which is `pub(super)` in `scheduler`; if the visibility does not carry, make it `pub(crate)` in `lifecycle.rs` and re-export as `pub(super)`.

- [ ] **Step 5: Verify the test count is unchanged**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib scheduler::asks:: 2>&1 | tail -3
cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: **the same 15 passed** from Step 1, and clippy exit 0. A different number means tests were lost or duplicated in the move — stop and reconcile before committing.

- [ ] **Step 6: Commit**

```bash
git add core/src/scheduler/asks.rs core/src/scheduler/asks/
git commit -m "refactor(scheduler): split asks.rs into pure + lifecycle (movement only)

801 lines, over the 500-line guidance, and #564 slice 2 grows it. This
repo's rule is to split before the change that grows a file so the
movement diff is reviewable on its own.

Pure/lifecycle rather than by feature: the pure half is unit-tested, the
async half needs a live pool and is e2e-tested, and that boundary is
already how the file was organised internally. All 15 tests are pure and
move as a block; count verified identical before and after. mod.rs
re-exports flat, so every existing scheduler::asks::<item> path resolves
unchanged and no call site is touched.

Refs #564

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

## Task 1: `db::asks` — the entitlement guard and the shorter nonce

Implements spec **D8**, **D16**, **D17**.

**Files:**
- Modify: `db/src/asks.rs` (`NONCE_BYTES`, add `Claimant` + `ResolvedAsk`, rewrite `resolve_with_nonce`)
- Modify: `db/tests/asks_e2e.rs` (4 existing tests + 5 new)

**Interfaces:**
- Consumes: nothing.
- Produces:
  ```rust
  pub struct Claimant { /* private fields */ }
  impl Claimant {
      pub fn new(channel: impl Into<String>, peer: impl Into<String>) -> Self;
      pub fn attribution(&self) -> String;   // "<channel>/<peer>"
  }
  #[derive(Clone, Copy, Debug, Eq, PartialEq)]
  pub struct ResolvedAsk { pub ask_id: i64, pub task_id: i64 }
  pub async fn resolve_with_nonce(
      pool: &PgPool,
      nonce: &Nonce,
      claimant: &Claimant,
      resolution: &serde_json::Value,
  ) -> Result<Option<ResolvedAsk>, DbError>;
  ```

**⚠️ Blast radius — read this before starting.** Four existing tests in `db/tests/asks_e2e.rs` drive `resolve_with_nonce` against tasks whose payload is `{"instruction": "..."}` with **no channel origin**. Under the new guard they must stop resolving, which is correct behaviour and a broken test. Each needs its `insert_pending` payload changed to the channel shape and its `resolved_by` string replaced by a matching `Claimant`. They are at (approximately) lines 671, 733, 810 and 1147 — locate them with `grep -n 'resolve_with_nonce' db/tests/asks_e2e.rs`.

- [ ] **Step 1: Write the failing tests**

Add to `db/tests/asks_e2e.rs`. First a shared fixture next to the existing helpers:

```rust
/// The `tasks.payload` shape a channel-originated task carries — the four
/// keys `channel::ingest::build_channel_task_payload` writes. The D16
/// entitlement guard reads three of them, so every nonce test needs a task
/// that has them.
fn channel_payload(peer: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "channel",
        "instruction": "what is my flight's GST?",
        "channel": "matrix",
        "peer": peer,
        "conversation": "!room:kastellan.dev",
    })
}
```

Then the five new tests. Follow the file's existing harness style exactly (`let Some(h) = harness("<5-char tag>") else { return };` + `h.rt.block_on(async { let pool = h.migrated_pool("<name>").await; … })`).

```rust
/// **The reason D16 exists.** The nonce is delivered as a message into a
/// room, so every peer who can read that room holds it. Possession
/// therefore proves nothing about entitlement, and before this guard a
/// co-present peer could resolve someone else's approval by copying the
/// token out of the scrollback — the exact attack the nonce was chosen to
/// prevent, defeated by reading instead of guessing.
#[test]
fn a_different_peer_holding_the_correct_nonce_cannot_resolve() {
    let Some(h) = harness("askcl") else { return };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-claimant-e2e").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let task_id = tasks::insert_pending(
            pool, Lane::Fast, channel_payload("@horst:kastellan.dev"),
        ).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve", "deny"]), Some("d1"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600), None,
        ).await.unwrap();

        // Correct nonce, wrong peer, same room, same channel.
        let intruder = asks::Claimant::new("matrix", "@someone-else:kastellan.dev");
        let got = asks::resolve_with_nonce(
            pool, &raised.nonce, &intruder, &serde_json::json!({"choice": "approve"}),
        ).await.unwrap();
        assert!(got.is_none(), "a peer who does not own the task must not resolve its ask");

        // And the ask is untouched, so the real owner can still answer.
        assert_eq!(asks::get(pool, raised.ask_id).await.unwrap().unwrap().state, "pending");
        let owner = asks::Claimant::new("matrix", "@horst:kastellan.dev");
        assert!(asks::resolve_with_nonce(
            pool, &raised.nonce, &owner, &serde_json::json!({"choice": "approve"}),
        ).await.unwrap().is_some());
    });
}

/// The channel is half of the identity. Two transports can carry the same
/// peer string (an email address that is also a Matrix localpart), and
/// matching on the peer alone would let one transport answer the other's
/// asks — with only the weaker of the two authentications behind it.
#[test]
fn the_right_peer_on_the_wrong_channel_cannot_resolve() {
    let Some(h) = harness("askch") else { return };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-claimant-channel-e2e").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let task_id = tasks::insert_pending(
            pool, Lane::Fast, channel_payload("@horst:kastellan.dev"),
        ).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve", "deny"]), Some("d1"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600), None,
        ).await.unwrap();

        let wrong_transport = asks::Claimant::new("email", "@horst:kastellan.dev");
        assert!(asks::resolve_with_nonce(
            pool, &raised.nonce, &wrong_transport,
            &serde_json::json!({"choice": "approve"}),
        ).await.unwrap().is_none());
    });
}

/// An ask on a task with no channel origin is answerable only through the
/// local CLI (`asks::resolve`, by id). The `EXISTS` finds no row for any
/// claimant, which pairs with spec D3: such an ask is never delivered to a
/// channel in the first place, so nothing should be able to answer it from
/// one.
#[test]
fn an_ask_on_a_cli_task_resolves_through_no_claimant_at_all() {
    let Some(h) = harness("askcli") else { return };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-cli-task-e2e").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let task_id = tasks::insert_pending(
            pool, Lane::Fast, serde_json::json!({"instruction": "cli task"}),
        ).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve", "deny"]), Some("d1"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600), None,
        ).await.unwrap();

        let anyone = asks::Claimant::new("matrix", "@horst:kastellan.dev");
        assert!(asks::resolve_with_nonce(
            pool, &raised.nonce, &anyone, &serde_json::json!({"choice": "approve"}),
        ).await.unwrap().is_none());

        // The local operator path still works — this is not a wedge.
        assert!(asks::resolve(
            pool, raised.ask_id, "hherb", &serde_json::json!({"choice": "approve"}),
        ).await.unwrap());
    });
}

/// `resolved_by` is composed inside the resolver from the claimant its own
/// guard matched, not supplied by the caller. That is what makes the
/// attribution in the audit trail the identity the entitlement was checked
/// against — and it removes the parameter that used to sit adjacent to the
/// nonce, where transposing the two compiled and wrote the live token into
/// a column on a table with no DELETE grant.
#[test]
fn resolved_by_is_composed_from_the_claimant_the_guard_matched() {
    let Some(h) = harness("askrb") else { return };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-resolved-by-e2e").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let task_id = tasks::insert_pending(
            pool, Lane::Fast, channel_payload("@horst:kastellan.dev"),
        ).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve", "deny"]), Some("d1"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600), None,
        ).await.unwrap();

        let owner = asks::Claimant::new("matrix", "@horst:kastellan.dev");
        let resolved = asks::resolve_with_nonce(
            pool, &raised.nonce, &owner, &serde_json::json!({"choice": "approve"}),
        ).await.unwrap().expect("owner resolves");
        assert_eq!(resolved.ask_id, raised.ask_id);
        assert_eq!(resolved.task_id, task_id);

        let got = asks::get(pool, raised.ask_id).await.unwrap().unwrap();
        assert_eq!(got.resolved_by.as_deref(), Some("matrix/@horst:kastellan.dev"));
    });
}

/// D17's no-migration claim, asserted rather than argued: the column stores
/// a SHA-256 whatever the input length, so an ask raised before the nonce
/// shrank still resolves. Simulated by resolving a nonce of the OLD length
/// that we inject through the same public path a pre-change row took.
#[test]
fn a_long_legacy_nonce_still_resolves_after_the_nonce_shrank() {
    let Some(h) = harness("asklg") else { return };
    h.rt.block_on(async {
        let pool = h.migrated_pool("asks-legacy-nonce-e2e").await;
        let pool = &pool;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let task_id = tasks::insert_pending(
            pool, Lane::Fast, channel_payload("@horst:kastellan.dev"),
        ).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();
        let raised = asks::raise(
            pool, task_id, "plan_approval", "approve?",
            &serde_json::json!(["approve", "deny"]), Some("d1"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(600), None,
        ).await.unwrap();

        // Overwrite the stored hash with the hash of a 64-char nonce, which
        // is exactly the row shape a pre-D17 ask has.
        let legacy = "a".repeat(64);
        sqlx::query("UPDATE asks SET nonce_sha256 = $2 WHERE id = $1")
            .bind(raised.ask_id)
            .bind(asks::sha256_hex(&legacy))
            .execute(pool)
            .await
            .unwrap();

        let owner = asks::Claimant::new("matrix", "@horst:kastellan.dev");
        assert!(asks::resolve_with_nonce(
            pool, &asks::Nonce::from_wire(legacy), &owner,
            &serde_json::json!({"choice": "approve"}),
        ).await.unwrap().is_some());
    });
}
```

Also add this unit test to `db/src/asks.rs`'s existing `mod tests`:

```rust
/// D17: 5 bytes, hex-encoded — 10 characters, short enough to copy off a
/// phone screen. Pinned because the whole point of the change is the
/// LENGTH, and nothing else in the suite would notice it drifting back.
#[test]
fn the_nonce_is_ten_characters() {
    assert_eq!(NONCE_BYTES, 5);
    assert_eq!(generate_nonce().len(), 10);
}

/// The attribution is the two claimant fields joined, and it is the only
/// thing that reaches `resolved_by`. Pure, so it needs no cluster.
#[test]
fn claimant_attribution_is_channel_slash_peer() {
    let c = Claimant::new("matrix", "@horst:kastellan.dev");
    assert_eq!(c.attribution(), "matrix/@horst:kastellan.dev");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-db --lib asks:: 2>&1 | tail -20
```

Expected: FAIL to compile — `cannot find type Claimant in this scope`.

- [ ] **Step 3: Implement**

In `db/src/asks.rs`, change the constant and its doc:

```rust
/// Nonce length in bytes, hex-encoded to twice that on the wire.
///
/// **5 bytes — 10 hex characters (#564 slice 2, spec D17).** It was 32
/// when the nonce was the sole barrier; `resolve_with_nonce` now also
/// requires the claimant to be the task's own peer, so the nonce is
/// correlation plus defence in depth and 64 characters bought nothing but
/// a message no operator would retype at 2 a.m.
///
/// 40 bits, against an attacker who must already be a paired peer
/// answering their own task's ask, gets one attempt per inbound message,
/// leaves a `channel.ask_answer_rejected` audit row on each miss, and has
/// until the 24 h deadline. No migration: the column stores the SHA-256,
/// which is 64 hex characters whatever the input length.
const NONCE_BYTES: usize = 5;
```

Add the two types next to `RaisedAsk`:

```rust
/// Who is claiming the right to answer an ask: a `(channel, peer)` pair
/// that some transport has already authenticated.
///
/// **A struct, not two `&str` parameters.** The two fields are both
/// free-form strings that appear adjacent in the only call, so as separate
/// parameters a transposition compiles and silently checks the peer
/// against the channel — which matches nothing, which fails closed, but
/// fails closed *invisibly*: every approval simply stops working with no
/// error that names the cause. Same reasoning as [`Nonce`], one field over.
///
/// **Construct it from the transport's own view of the sender**, never
/// from anything in a message body. A body-supplied identity hands the
/// entitlement check straight back to the sender.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claimant {
    channel: String,
    peer: String,
}

impl Claimant {
    pub fn new(channel: impl Into<String>, peer: impl Into<String>) -> Self {
        Self { channel: channel.into(), peer: peer.into() }
    }

    /// The `asks.resolved_by` attribution: `"<channel>/<peer>"`.
    ///
    /// Composed here rather than taken as a parameter, so the identity in
    /// the audit trail is by construction the identity the entitlement
    /// guard matched on.
    pub fn attribution(&self) -> String {
        format!("{}/{}", self.channel, self.peer)
    }

    pub(crate) fn channel(&self) -> &str { &self.channel }
    pub(crate) fn peer(&self) -> &str { &self.peer }
}

/// What a successful [`resolve_with_nonce`] hands back. `task_id` is what
/// lets the caller's acknowledgement name the task that is resuming; a
/// `bool` could not, and a second query for it would race the resumption.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedAsk {
    pub ask_id: i64,
    pub task_id: i64,
}
```

Rewrite `resolve_with_nonce`. Keep the entire existing doc comment and **add** the D16 paragraph; change the signature, the SQL and the return type only:

```rust
/// … (keep every existing paragraph) …
///
/// **The claimant must own the task (#564 slice 2, spec D16).** The nonce
/// is delivered as a message into a conversation, so it is a *bearer*
/// token: everyone who can read that conversation holds it. Possession
/// alone therefore never established entitlement, and the guard below adds
/// the half this doc used to defer to the caller — ask N is answerable
/// only by the `(channel, peer)` recorded on the task that raised it.
///
/// It is a predicate in the same guarded UPDATE rather than a check in the
/// caller, because a caller-side check is a TOCTOU: it would establish
/// entitlement against a row read outside the transaction that commits the
/// resolution. In the guard it is atomic, fail-closed, and inherits the
/// no-existence-oracle property — a wrong peer is indistinguishable from a
/// wrong nonce.
pub async fn resolve_with_nonce(
    pool: &PgPool,
    nonce: &Nonce,
    claimant: &Claimant,
    resolution: &serde_json::Value,
) -> Result<Option<ResolvedAsk>, DbError> {
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
           AND EXISTS (SELECT 1 FROM tasks t \
                        WHERE t.id = asks.task_id \
                          AND t.payload->>'kind' = 'channel' \
                          AND t.payload->>'channel' = $4 \
                          AND t.payload->>'peer' = $5) \
         RETURNING id, task_id, options",
    )
    .bind(&nonce_hash)
    .bind(claimant.attribution())
    .bind(resolution)
    .bind(claimant.channel())
    .bind(claimant.peer())
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| DbError::Query(format!("asks resolve_with_nonce: {e}")))?;

    let Some(row) = row else {
        // Wrong/unissued nonce, a claimant who does not own the task, lost
        // the race, or past the deadline. Dropping `tx` rolls back; nothing
        // was written either way, and the four are deliberately one answer.
        return Ok(None);
    };
    let (resolved_ask_id, task_id) = decode_resolved_ids(&row)?;
    reject_choice_outside_options(&row, resolved_ask_id, resolution)?;

    finish_resolve(tx, resolved_ask_id, task_id)
        .await
        .map(|_| Some(ResolvedAsk { ask_id: resolved_ask_id, task_id }))
}
```

Then fix the four existing `resolve_with_nonce` tests: give each `insert_pending` the `channel_payload(...)` shape, and replace the `resolved_by` string argument with `&asks::Claimant::new("matrix", "@horst:kastellan.dev")`. Their assertions on `resolved_by` become `Some("matrix/@horst:kastellan.dev")`, and their `assert!(won)` / `assert!(!lost)` become `assert!(got.is_some())` / `assert!(got.is_none())`.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-db --lib asks:: 2>&1 | tail -5
KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin" \
  cargo test -p kastellan-db --test asks_e2e 2>&1 | tail -8
```

Expected: unit tests pass; e2e reports **32 passed** (27 existing + 5 new), zero `[SKIP]`. If the e2e skips, the PG override is wrong — the count is the only evidence the guard ran.

- [ ] **Step 5: Commit**

```bash
git add db/src/asks.rs db/tests/asks_e2e.rs
git commit -m "feat(db): the answer must come from the task's own peer (#564 slice 2, D16/D17)

The nonce is delivered as a message into a conversation, so everyone who
can read that conversation holds it. It was never protection against a
co-present peer resolving someone else's approval — the threat the
ROADMAP chose it for. Guessing was the wrong threat; reading is the easy
path.

resolve_with_nonce now takes a Claimant and its guarded UPDATE requires
the (channel, peer) recorded on the owning task to match. In the guard,
not the caller: a caller-side check establishes entitlement against a row
read outside the transaction that commits the resolution. It inherits the
no-existence-oracle property, so a wrong peer and a wrong nonce are one
answer.

resolved_by stops being a parameter and is composed from the same
claimant the guard matched, which deletes the adjacent-&str transposition
the Nonce newtype was created to defend against. The return type widens
to Option<ResolvedAsk> so a caller can name the task that is resuming.

NONCE_BYTES 32 -> 5. With entitlement carried by the guard the nonce is
correlation plus defence in depth, and 10 hex characters is a command an
operator will actually run. No migration: the column stores the SHA-256
either way, asserted by a test that resolves a legacy 64-char nonce.

Refs #564

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `channel::ask_message` — the pure wire vocabulary

Implements spec **D7**, **D10**, **D11**, and the parse half of **D5**.

**Files:**
- Create: `core/src/channel/ask_message.rs`
- Modify: `core/src/channel/mod.rs` (add `pub mod ask_message;`)

**Interfaces:**
- Consumes: `ChannelId`, `PeerId`, `ConversationId` from `channel::mod`.
- Produces:
  ```rust
  pub struct AskDestination { pub channel: ChannelId, pub peer: PeerId, pub conversation: ConversationId }
  pub fn destination_from_task_payload(payload: &serde_json::Value) -> Option<AskDestination>;
  #[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum AskChoice { Approve, Deny }
  impl AskChoice { pub fn as_str(self) -> &'static str }
  #[derive(Clone, Debug, Eq, PartialEq)] pub struct AskCommand { pub choice: AskChoice, pub token: String }
  pub fn parse_ask_command(body: &str) -> Option<AskCommand>;
  pub fn render_ask(task_id: i64, concern: &str, token: &str, deadline_at: time::OffsetDateTime) -> String;
  pub const CONCERN_CAP: usize = 512;
  pub fn ack_resolved(choice: AskChoice, task_id: i64) -> String;
  pub const ACK_NOT_ANSWERABLE: &str;
  ```

- [ ] **Step 1: Write the failing tests**

Create `core/src/channel/ask_message.rs` with the test module first (implementation stubs come in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload() -> serde_json::Value {
        json!({
            "kind": "channel", "instruction": "hi",
            "channel": "matrix", "peer": "@horst:srv", "conversation": "!room:srv",
        })
    }

    #[test]
    fn a_channel_payload_yields_its_destination() {
        let d = destination_from_task_payload(&payload()).expect("destination");
        assert_eq!(d.channel.0, "matrix");
        assert_eq!(d.peer.0, "@horst:srv");
        assert_eq!(d.conversation.0, "!room:srv");
    }

    #[test]
    fn a_non_channel_task_has_no_destination() {
        assert!(destination_from_task_payload(&json!({"kind": "ask", "instruction": "hi"})).is_none());
    }

    /// Each of the three routing fields is individually required: a payload
    /// missing any one of them is not routable, and a partial destination
    /// would send a question into a conversation it cannot be answered from.
    #[test]
    fn each_missing_routing_field_defeats_the_destination() {
        for drop_key in ["channel", "peer", "conversation"] {
            let mut p = payload();
            p.as_object_mut().unwrap().remove(drop_key);
            assert!(
                destination_from_task_payload(&p).is_none(),
                "payload without {drop_key} must not be routable",
            );
        }
    }

    #[test]
    fn both_verbs_parse_with_their_token() {
        let a = parse_ask_command("/approve 7f3a9c2e1b").expect("approve");
        assert_eq!(a.choice, AskChoice::Approve);
        assert_eq!(a.token, "7f3a9c2e1b");
        let d = parse_ask_command("/deny 7f3a9c2e1b").expect("deny");
        assert_eq!(d.choice, AskChoice::Deny);
    }

    /// Chat clients capitalise, and trailing whitespace is invisible. Neither
    /// should cost an operator their approval.
    #[test]
    fn the_verb_is_case_insensitive_and_the_body_is_trimmed() {
        assert_eq!(parse_ask_command("  /APPROVE abc \n").unwrap().choice, AskChoice::Approve);
        assert_eq!(parse_ask_command("/Deny abc").unwrap().choice, AskChoice::Deny);
    }

    /// D7: no shape check on the token. A syntactic pre-check would couple
    /// this parser to the nonce ENCODING, so changing `generate_nonce` would
    /// silently stop every answer parsing while every resolver test still
    /// passed. Only the WHERE predicate decides whether a token is real.
    #[test]
    fn a_token_of_any_shape_parses() {
        for token in ["7f3a9c2e1b", "ZZZZ", "not-hex-at-all", "1"] {
            assert_eq!(parse_ask_command(&format!("/approve {token}")).unwrap().token, token);
        }
    }

    /// Everything that is not exactly two tokens is an ordinary message and
    /// must take the normal enqueue path. The three-token case is the sharp
    /// one: accepting it would let `/approve <token> and delete my mail`
    /// resolve an ask *and* look to the operator like it did something else.
    #[test]
    fn anything_that_is_not_exactly_verb_plus_token_is_not_a_command() {
        for body in [
            "/approve",
            "/deny",
            "/approve  ",
            "/approve a b",
            "/approve token trailing prose",
            "approve 7f3a9c2e1b",
            "please /approve 7f3a9c2e1b",
            "what is my flight's GST?",
            "",
            "/approver 7f3a9c2e1b",
        ] {
            assert!(parse_ask_command(body).is_none(), "must not parse as a command: {body:?}");
        }
    }

    /// The two vocabularies must agree: what the wire parser produces is
    /// what `db::asks::resolve_with_nonce` matches against the ask's own
    /// `options`, and a mismatch is a rolled-back transaction that reads as
    /// "the token was wrong".
    #[test]
    fn the_wire_verbs_are_the_stored_choices() {
        assert_eq!(AskChoice::Approve.as_str(), "approve");
        assert_eq!(AskChoice::Deny.as_str(), "deny");
    }

    #[test]
    fn the_rendered_ask_carries_both_copyable_commands() {
        let msg = render_ask(412, "plan writes outside the scratch dir", "7f3a9c2e1b", deadline());
        assert!(msg.contains("/approve 7f3a9c2e1b"), "{msg}");
        assert!(msg.contains("/deny 7f3a9c2e1b"), "{msg}");
        assert!(msg.contains("412"), "the task id orients the operator: {msg}");
        assert!(msg.contains("plan writes outside the scratch dir"), "{msg}");
    }

    /// Each rendered command must round-trip through the parser. Without
    /// this the two halves can drift — the message could print a prefix the
    /// parser does not accept, and every test on each side would still pass.
    #[test]
    fn every_command_the_message_prints_parses_back() {
        let msg = render_ask(1, "c", "tok123", deadline());
        let commands: Vec<&str> =
            msg.lines().map(str::trim).filter(|l| l.starts_with('/')).collect();
        assert_eq!(commands.len(), 2, "exactly two commands offered: {msg}");
        for line in commands {
            let cmd = parse_ask_command(line)
                .unwrap_or_else(|| panic!("rendered command does not parse: {line:?}"));
            assert_eq!(cmd.token, "tok123");
        }
    }

    /// The concern is model-authored (it is the reviewer's `reason`), so it
    /// is clamped. Asserted in BOTH directions: a clamp test that only
    /// bounds the maximum passes when the clamp is so aggressive that
    /// nothing fits, which inverts its own purpose (the #572 lesson).
    #[test]
    fn an_oversized_concern_is_clamped_and_the_commands_survive() {
        let huge = "x".repeat(CONCERN_CAP * 4);
        let msg = render_ask(9, &huge, "tok", deadline());
        assert!(msg.len() < CONCERN_CAP * 2, "upper bound: not clamped at all? {}", msg.len());
        assert!(msg.len() > CONCERN_CAP, "lower bound: clamped so hard nothing fits? {}", msg.len());
        assert!(msg.contains("/approve tok"), "the commands must survive the clamp");
        assert!(msg.contains("/deny tok"));
    }

    /// A clamp that splits a multi-byte character panics on a String slice.
    /// The concern is free text and can be any language.
    #[test]
    fn clamping_a_multibyte_concern_does_not_panic() {
        let msg = render_ask(9, &"é".repeat(CONCERN_CAP), "tok", deadline());
        assert!(msg.contains("/approve tok"));
    }

    #[test]
    fn the_success_ack_names_the_task_and_the_decision() {
        assert!(ack_resolved(AskChoice::Approve, 412).contains("412"));
        assert!(ack_resolved(AskChoice::Approve, 412).to_lowercase().contains("approv"));
        assert!(ack_resolved(AskChoice::Deny, 412).to_lowercase().contains("den"));
    }

    /// D9: the failure ack must not distinguish wrong / expired / already
    /// answered / not-your-ask. `resolve_with_nonce` refuses to leak which,
    /// and re-leaking it in the presentation layer would hand back the
    /// existence oracle the query gives up.
    #[test]
    fn the_failure_ack_names_no_specific_cause() {
        let lowered = ACK_NOT_ANSWERABLE.to_lowercase();
        for leak in ["expired", "already", "wrong peer", "not found", "yours"] {
            assert!(!lowered.contains(leak), "the ack leaks a cause: {leak}");
        }
    }

    fn deadline() -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(1_787_000_000).unwrap()
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib channel::ask_message 2>&1 | tail -20
```

Expected: FAIL to compile — the module isn't declared and nothing in it exists.

- [ ] **Step 3: Write the implementation**

Prepend to `core/src/channel/ask_message.rs`:

```rust
//! The operator-ask wire vocabulary: what an ask looks like when it is sent
//! into a conversation, and what an answer looks like coming back.
//!
//! **Entirely pure** — no DB, no I/O, no clock. The bus and the scheduler
//! each own one direction of the loop, and both of them are hard to test;
//! everything that can be decided without them is decided here.
//!
//! Spec: `docs/superpowers/specs/2026-08-19-ask-channel-slice-2-design.md`.

use serde_json::Value;
use time::OffsetDateTime;

use super::{ChannelId, ConversationId, PeerId};

/// Byte cap on the model-authored concern text in a rendered ask (spec D11).
///
/// A legibility bound, not a containment one: the message goes to a paired
/// operator in their own room. What it protects is the two command lines,
/// which an unbounded concern would push off the visible message — and an
/// approval nobody can see the command for is an approval nobody gives.
pub const CONCERN_CAP: usize = 512;

/// The sentence sent back when an answer resolved nothing.
///
/// Deliberately one sentence for four causes — wrong token, already
/// answered, past its deadline, not this peer's ask (spec D9).
/// `resolve_with_nonce` refuses to distinguish them because splitting them
/// hands a token-guessing peer an existence oracle over ask ids; naming the
/// cause here would give back at the presentation layer exactly what the
/// query gives up.
pub const ACK_NOT_ANSWERABLE: &str =
    "\u{2717} That approval token isn't answerable. It may be mistyped, or the question \
     may no longer be open.";

/// Where an ask is delivered: the channel, peer and conversation of the
/// task that raised it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskDestination {
    pub channel: ChannelId,
    pub peer: PeerId,
    pub conversation: ConversationId,
}

/// The routing metadata on a channel-originated `tasks.payload`, or `None`
/// for a task that did not come from a channel (spec D3).
///
/// **Shared with [`super::route::reply_for_completed_task`] on purpose**
/// (spec D10): the place an ask is *delivered* and the place its task's
/// answer is *replied to* read the same four keys off the same row, and a
/// second copy would drift the first time either grew a field.
pub fn destination_from_task_payload(payload: &Value) -> Option<AskDestination> {
    if payload.get("kind").and_then(Value::as_str) != Some("channel") {
        return None;
    }
    Some(AskDestination {
        channel: ChannelId(payload.get("channel").and_then(Value::as_str)?.to_string()),
        peer: PeerId(payload.get("peer").and_then(Value::as_str)?.to_string()),
        conversation: ConversationId(
            payload.get("conversation").and_then(Value::as_str)?.to_string(),
        ),
    })
}

/// The two answers a `plan_approval` ask offers, in their wire spelling.
///
/// Deliberately distinct from `scheduler::asks::Choice`, which reads a
/// *stored* resolution: these are different layers (the wire vocabulary vs.
/// the record), and coupling them would make the channel module depend on
/// the scheduler for a two-variant enum. `the_wire_verbs_are_the_stored_choices`
/// is the anti-drift guard — [`Self::as_str`] must keep producing exactly the
/// strings that `raise_and_suspend` writes into `asks.options`, because
/// `db::asks::resolve_with_nonce` validates the choice against them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AskChoice {
    Approve,
    Deny,
}

impl AskChoice {
    /// The stored `resolution.choice` value. Must match `asks.options`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Deny => "deny",
        }
    }
}

/// A parsed answer: which verb, and the opaque correlation token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AskCommand {
    pub choice: AskChoice,
    /// Taken verbatim off the wire. Not a `Nonce` yet — this module is pure
    /// and the `db` newtype zeroizes on drop; the conversion happens at the
    /// resolver boundary, which is the only place that should hold one.
    pub token: String,
}

/// Recognise `/approve <token>` or `/deny <token>`, or `None` for anything
/// else — in which case the body is an ordinary message and takes the
/// normal screen-and-enqueue path.
///
/// **Strict: the trimmed body must be exactly two whitespace-separated
/// tokens.** Accepting a trailing tail would let one message both resolve
/// an ask and read, to the operator scrolling past it, as if it had said
/// something else.
///
/// **No shape check on the token** (spec D7). `Nonce::from_wire`'s doc
/// states the rule: `resolve_with_nonce`'s `WHERE` predicate is the only
/// thing entitled to decide whether a token is real. A syntactic pre-check
/// would also couple this parser to the nonce *encoding*, so a change to
/// `generate_nonce` would silently stop every answer from parsing while
/// every test of the resolver still passed.
pub fn parse_ask_command(body: &str) -> Option<AskCommand> {
    let mut parts = body.split_whitespace();
    let verb = parts.next()?;
    let token = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let choice = match verb.to_ascii_lowercase().as_str() {
        "/approve" => AskChoice::Approve,
        "/deny" => AskChoice::Deny,
        _ => return None,
    };
    Some(AskCommand { choice, token: token.to_string() })
}

/// The message an escalated plan sends into its task's conversation.
///
/// The two command lines are printed complete so answering is a copy, not a
/// transcription. **The ask id is deliberately absent**: it is not needed to
/// answer, and putting a small sequential integer in durable room history
/// invites exactly the resolve-by-id thinking `db::asks::resolve`'s doc
/// reserves for the local CLI.
pub fn render_ask(
    task_id: i64,
    concern: &str,
    token: &str,
    deadline_at: OffsetDateTime,
) -> String {
    format!(
        "\u{26a0}\u{fe0f} Approval needed \u{2014} task {task_id}\n\
         \n\
         An operator decision is required before I continue:\n\
         {concern}\n\
         \n\
         This expires {deadline}. Reply with one of:\n\
         \n\
         /approve {token}\n\
         /deny {token}",
        concern = clamp(concern, CONCERN_CAP),
        deadline = deadline_at,
    )
}

/// The acknowledgement for an answer that resolved an ask.
pub fn ack_resolved(choice: AskChoice, task_id: i64) -> String {
    match choice {
        AskChoice::Approve => format!("\u{2713} Approved \u{2014} task {task_id} is resuming."),
        AskChoice::Deny => format!("\u{2713} Denied \u{2014} task {task_id} will not proceed."),
    }
}

/// Truncate to at most `cap` bytes on a char boundary, marking the cut.
///
/// Char-boundary aware because the concern is free text in any language and
/// slicing a `String` mid-character panics. The marker is what keeps a
/// clamped concern visibly clamped rather than a sentence that just stops.
fn clamp(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\u{2026} (clamped)", &s[..end])
}
```

Add to `core/src/channel/mod.rs`, keeping the module list alphabetical:

```rust
pub mod ask_message;
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib channel::ask_message 2>&1 | tail -5
cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: **14 passed**, clippy exit 0.

- [ ] **Step 5: Commit**

```bash
git add core/src/channel/ask_message.rs core/src/channel/mod.rs
git commit -m "feat(channel): the pure ask wire vocabulary — render, parse, destination

Everything about the ask loop that can be decided without a bus or a pool.
The parser is strict (exactly verb plus token) so one message cannot both
resolve an ask and read as something else, and it applies no shape check
to the token: only resolve_with_nonce's WHERE predicate is entitled to
judge a nonce, and a syntactic pre-check would couple the parser to the
nonce encoding.

destination_from_task_payload is the extractor route.rs will share in the
next commit, so the place an ask is delivered and the place its answer is
replied to cannot disagree.

Two tests are the anti-drift guards worth naming: every command the
message prints must parse back, and the wire verbs must equal the stored
choices that resolve_with_nonce validates against asks.options.

Refs #564

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `channel::outbox` — the core-initiated-outbound primitive

Implements spec **D1**, **D4**, **D12**.

**Files:**
- Create: `core/src/channel/outbox.rs`
- Modify: `core/src/channel/mod.rs`

**Interfaces:**
- Consumes: `ChannelId`, `OutgoingMessage`.
- Produces:
  ```rust
  #[derive(Default)] pub struct ChannelOutbox;
  impl ChannelOutbox {
      pub fn new() -> Self;
      pub fn register(&self, id: ChannelId, tx: tokio::sync::mpsc::Sender<OutgoingMessage>);
      pub fn deregister(&self, id: &ChannelId);
      pub fn try_deliver(&self, msg: OutgoingMessage) -> Result<(), OutboxError>;
  }
  #[derive(Clone, Copy, Debug, Eq, PartialEq)]
  pub enum OutboxError { NoSuchChannel, QueueFull, QueueClosed }
  impl OutboxError { pub fn as_str(self) -> &'static str }
  ```

- [ ] **Step 1: Write the failing tests**

Create `core/src/channel/outbox.rs` containing only this test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn msg(channel: &str) -> OutgoingMessage {
        OutgoingMessage {
            channel: ChannelId(channel.to_string()),
            peer: PeerId("@horst:srv".to_string()),
            conversation: ConversationId("!room:srv".to_string()),
            body: "hello".to_string(),
        }
    }

    #[tokio::test]
    async fn a_registered_channel_receives_what_was_delivered() {
        let outbox = ChannelOutbox::new();
        let (tx, mut rx) = mpsc::channel(4);
        outbox.register(ChannelId("matrix".into()), tx);

        outbox.try_deliver(msg("matrix")).expect("delivered");
        assert_eq!(rx.recv().await.expect("received").body, "hello");
    }

    #[test]
    fn delivering_to_an_unregistered_channel_is_an_error_not_a_silent_drop() {
        let outbox = ChannelOutbox::new();
        assert_eq!(outbox.try_deliver(msg("matrix")), Err(OutboxError::NoSuchChannel));
    }

    #[test]
    fn a_deregistered_channel_stops_accepting() {
        let outbox = ChannelOutbox::new();
        let (tx, _rx) = mpsc::channel(4);
        outbox.register(ChannelId("matrix".into()), tx);
        outbox.deregister(&ChannelId("matrix".into()));
        assert_eq!(outbox.try_deliver(msg("matrix")), Err(OutboxError::NoSuchChannel));
    }

    /// The bus is supervised and restarts, so a sender can outlive the pump
    /// that drained it. That must be a reported failure, not a message that
    /// vanishes: the whole delivery contract is best-effort *and audited*.
    #[test]
    fn a_sender_whose_receiver_is_gone_reports_closed() {
        let outbox = ChannelOutbox::new();
        let (tx, rx) = mpsc::channel(4);
        outbox.register(ChannelId("matrix".into()), tx);
        drop(rx);
        assert_eq!(outbox.try_deliver(msg("matrix")), Err(OutboxError::QueueClosed));
    }

    /// try_send, not send (spec D4): the raise path must never block on a
    /// channel whose consumer has stopped draining. A blocking send here
    /// parks the scheduler's escalation path behind a wedged transport.
    #[test]
    fn a_full_queue_is_refused_immediately_rather_than_awaited() {
        let outbox = ChannelOutbox::new();
        let (tx, _rx) = mpsc::channel(1);
        outbox.register(ChannelId("matrix".into()), tx);
        outbox.try_deliver(msg("matrix")).expect("first fits");
        assert_eq!(outbox.try_deliver(msg("matrix")), Err(OutboxError::QueueFull));
    }

    /// A restarted bus registers a fresh sender under the same id; the stale
    /// one must be replaced, or every delivery after the first restart goes
    /// into a queue nobody drains.
    #[tokio::test]
    async fn re_registering_replaces_the_stale_sender() {
        let outbox = ChannelOutbox::new();
        let (old_tx, old_rx) = mpsc::channel(4);
        outbox.register(ChannelId("matrix".into()), old_tx);
        drop(old_rx);

        let (new_tx, mut new_rx) = mpsc::channel(4);
        outbox.register(ChannelId("matrix".into()), new_tx);

        outbox.try_deliver(msg("matrix")).expect("delivered to the new sender");
        assert_eq!(new_rx.recv().await.expect("received").body, "hello");
    }

    /// Routing is per channel: an ask for a channel this outbox does not
    /// serve must not be delivered to whatever else happens to be registered.
    #[test]
    fn delivery_is_routed_by_channel_id() {
        let outbox = ChannelOutbox::new();
        let (tx, _rx) = mpsc::channel(4);
        outbox.register(ChannelId("matrix".into()), tx);
        assert_eq!(outbox.try_deliver(msg("email")), Err(OutboxError::NoSuchChannel));
    }

    /// The audit labels are a fixed set, and the payloads that carry them
    /// are durable. Pinned so a rename is a deliberate act.
    #[test]
    fn every_error_has_a_stable_audit_label() {
        assert_eq!(OutboxError::NoSuchChannel.as_str(), "no_such_channel");
        assert_eq!(OutboxError::QueueFull.as_str(), "queue_full");
        assert_eq!(OutboxError::QueueClosed.as_str(), "queue_closed");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib channel::outbox 2>&1 | tail -20
```

Expected: FAIL to compile — `ChannelOutbox` undefined.

- [ ] **Step 3: Write the implementation**

Prepend to `core/src/channel/outbox.rs`:

```rust
//! Core-initiated outbound: the seam that lets code outside the bus send a
//! message on a channel.
//!
//! The bus is otherwise strictly *inbound message → task → reply on
//! completion*, so nothing in core could start a conversation — which is
//! why `Verdict::Escalate` could raise a durable question that reached only
//! `kastellan-cli inbox`.
//!
//! **Why a registry and not a `Sender`.** The scheduler is spawned before
//! the channel supervisors, and each supervisor *restarts* its bus (#514,
//! #517). So the scheduler cannot hold a sender the bus owns: it does not
//! exist yet at scheduler spawn, and any sender it held would go stale on
//! the next respawn. This is the indirection both sides share — `main`
//! creates it before either, the bus registers into it on every bring-up,
//! and a stale entry surfaces as [`OutboxError::QueueClosed`] rather than a
//! message that quietly disappears.
//!
//! Spec: `docs/superpowers/specs/2026-08-19-ask-channel-slice-2-design.md`.

use std::collections::HashMap;
use std::sync::RwLock;

use tokio::sync::mpsc;

use super::{ChannelId, ConversationId, OutgoingMessage, PeerId};

/// Why a delivery did not reach a channel's queue.
///
/// Every variant is a **fixed label** (see [`Self::as_str`]) because it is
/// written verbatim into a durable audit payload. Same rule as
/// `auth::UnauthenticReason`: never derive one from input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboxError {
    /// No channel is registered under that id — it is not configured, or it
    /// is between bring-ups.
    NoSuchChannel,
    /// The channel's outbound queue is full; its pump is not draining.
    QueueFull,
    /// A sender is registered but its receiver is gone — a bus that ended
    /// without deregistering.
    QueueClosed,
}

impl OutboxError {
    /// Stable audit label. These strings land in `audit_log` payloads that
    /// operators query: add freely, never rename.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoSuchChannel => "no_such_channel",
            Self::QueueFull => "queue_full",
            Self::QueueClosed => "queue_closed",
        }
    }
}

/// The registry of live per-channel outbound queues.
///
/// **Synchronous** (spec D4): [`try_deliver`](Self::try_deliver) uses
/// `try_send`, so the raise path never blocks on a wedged transport, and no
/// lock is ever held across an `await` — which forecloses the whole family
/// of deadlocks a lock plus async invites.
#[derive(Default)]
pub struct ChannelOutbox {
    senders: RwLock<HashMap<ChannelId, mpsc::Sender<OutgoingMessage>>>,
}

impl ChannelOutbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a channel's outbound queue. Called by
    /// `ChannelBus::spawn` with the *same* sender its own reply pump uses,
    /// so there is one queue per channel and no second delivery path.
    pub fn register(&self, id: ChannelId, tx: mpsc::Sender<OutgoingMessage>) {
        self.senders.write().expect("outbox lock not poisoned").insert(id, tx);
    }

    /// Drop a channel's queue. Called by `ChannelBus::shutdown`, so a bus
    /// that is going away stops being a delivery target immediately rather
    /// than after its first failed send.
    pub fn deregister(&self, id: &ChannelId) {
        self.senders.write().expect("outbox lock not poisoned").remove(id);
    }

    /// Queue `msg` for the channel it names. Never blocks; never panics.
    pub fn try_deliver(&self, msg: OutgoingMessage) -> Result<(), OutboxError> {
        let senders = self.senders.read().expect("outbox lock not poisoned");
        let tx = senders.get(&msg.channel).ok_or(OutboxError::NoSuchChannel)?;
        tx.try_send(msg).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => OutboxError::QueueFull,
            mpsc::error::TrySendError::Closed(_) => OutboxError::QueueClosed,
        })
    }
}
```

Add `pub mod outbox;` to `core/src/channel/mod.rs`.

Note: the test module's `msg()` helper uses `PeerId`/`ConversationId`, which the `use super::{...}` line above imports — keep them even though the non-test code does not name them, or move them into the test module's own `use`. Prefer the latter to avoid an unused-import warning in non-test builds: drop `ConversationId` and `PeerId` from the module-level `use` and add `use super::{ConversationId, PeerId};` inside `mod tests`.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib channel::outbox 2>&1 | tail -5
cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: **8 passed**, clippy exit 0.

- [ ] **Step 5: Commit**

```bash
git add core/src/channel/outbox.rs core/src/channel/mod.rs
git commit -m "feat(channel): ChannelOutbox — the core-initiated-outbound primitive

The bus is strictly inbound-task then reply-on-completion, so nothing in
core could start a conversation. A registry rather than a Sender because
the scheduler spawns before the channel supervisors and each supervisor
restarts its bus: a held sender does not exist yet at spawn and goes
stale on respawn. main creates the registry before both.

Synchronous and try_send, so the escalation path never blocks on a wedged
transport and no lock is held across an await. Every failure is a typed
error with a fixed audit label — a delivery that reached nobody must be
distinguishable from one that landed, which is the property that makes
best-effort delivery honest.

Refs #564

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: `route.rs` — share the extractor, and say what `ask_timeout` means

Implements spec **D10**, **D14**.

**Files:**
- Modify: `core/src/channel/route.rs`

**Interfaces:**
- Consumes: `ask_message::destination_from_task_payload` (Task 2), `kastellan_db::asks::ASK_TIMEOUT_DETAIL`.
- Produces: no signature changes. `reply_for_completed_task` and `reply_body` keep their shapes.

- [ ] **Step 1: Write the failing tests**

Add to `core/src/channel/route.rs`'s existing `mod tests`:

```rust
/// D14. An expired ask already reaches the room — `notify_task_completed`
/// is an `AFTER UPDATE OF state` trigger and `awaiting_operator → failed`
/// crosses into its terminal set — so the only question is what it says.
/// "Sorry — that failed: ask_timeout" is true and useless; the user's
/// question stalled because nobody answered a question about it.
#[test]
fn an_ask_timeout_reads_as_an_unanswered_question_not_a_crash() {
    let body = reply_body(Some(&json!({"kind": "error", "detail": "ask_timeout"})));
    assert!(!body.contains("ask_timeout"), "the raw detail string is not user-facing: {body}");
    let lowered = body.to_lowercase();
    assert!(lowered.contains("answer"), "must say nobody answered: {body}");
}

/// Every other error detail keeps the existing generic rendering — the new
/// arm must be exactly one detail string wide, not a prefix match that
/// swallows unrelated failures.
#[test]
fn other_error_details_are_unchanged_by_the_timeout_arm() {
    let body = reply_body(Some(&json!({"kind": "error", "detail": "ask_timeout_but_not_really"})));
    assert!(body.contains("ask_timeout_but_not_really"), "{body}");
}

/// D10: the ask's destination and the reply's routing are read off the same
/// payload by the same function. Asserted directly, because the failure
/// mode is silent — the two drift, and an ask is delivered to a
/// conversation the answer never returns to.
#[test]
fn the_reply_route_and_the_ask_destination_agree() {
    use crate::channel::ask_message::destination_from_task_payload;
    let p = channel_payload();
    let reply = reply_for_completed_task(&p, Some(&json!({"kind": "completed"}))).expect("reply");
    let dest = destination_from_task_payload(&p).expect("destination");
    assert_eq!(reply.channel, dest.channel);
    assert_eq!(reply.peer, dest.peer);
    assert_eq!(reply.conversation, dest.conversation);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib channel::route 2>&1 | tail -20
```

Expected: `an_ask_timeout_reads_as_an_unanswered_question_not_a_crash` FAILS on the `contains("ask_timeout")` assertion.

- [ ] **Step 3: Write the implementation**

Replace the body of `reply_for_completed_task` so it delegates the extraction:

```rust
pub fn reply_for_completed_task(payload: &Value, result: Option<&Value>) -> Option<OutgoingMessage> {
    // The same four keys the ask-delivery path reads, through the same
    // function (spec D10) — so where an ask is asked and where its task's
    // answer is delivered cannot drift apart.
    let dest = super::ask_message::destination_from_task_payload(payload)?;
    Some(OutgoingMessage {
        channel: dest.channel,
        peer: dest.peer,
        conversation: dest.conversation,
        body: reply_body(result),
    })
}
```

And add the timeout arm to `reply_body`, **before** the general `Some("error")` arm:

```rust
        // An operator ask timed out (#564 slice 2, spec D14). The generic
        // error arm below renders this as "Sorry — that failed:
        // ask_timeout", which is true and tells the user nothing: their
        // request stalled because a question *about* it went unanswered.
        // Matched on the exact detail string `db::asks` defines, so a
        // different error carrying a similar-looking detail is unaffected.
        Some("error")
            if result.get("detail").and_then(Value::as_str)
                == Some(kastellan_db::asks::ASK_TIMEOUT_DETAIL) =>
        {
            "I needed an operator to approve something before continuing, and nobody \
             answered in time, so I stopped."
                .to_string()
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib channel::route 2>&1 | tail -5
cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: all `channel::route` tests pass (the existing ones plus 3), clippy exit 0.

- [ ] **Step 5: Commit**

```bash
git add core/src/channel/route.rs
git commit -m "feat(channel): share the routing extractor, and explain an ask timeout

reply_for_completed_task now reads its routing through
ask_message::destination_from_task_payload, the same function the ask
delivery path uses. Two copies of the same four-key extraction would
drift the first time either grew a field, and the failure is silent: an
ask delivered to a conversation the answer never returns to.

An expired ask already reaches the room — notify_task_completed is a
state trigger and awaiting_operator to failed crosses into its terminal
set — so the only question was what it said, and 'Sorry, that failed:
ask_timeout' told the user nothing. Matched on the exact detail string
db::asks defines, so a similar-looking detail is unaffected.

Refs #564

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: `scheduler::asks::delivery` — the pure delivery decision

Implements spec **D2**, **D3**, and the audit half of the delivery path.

**Files:**
- Create: `core/src/scheduler/asks/delivery.rs`
- Modify: `core/src/scheduler/asks/mod.rs` (add `pub mod delivery;`)
- Modify: `core/src/scheduler/audit.rs` (three action constants)

**Interfaces:**
- Consumes: `channel::outbox::{ChannelOutbox, OutboxError}` (Task 3), `channel::ask_message::{AskDestination, render_ask}` (Task 2).
- Produces:
  ```rust
  #[derive(Clone, Debug, Eq, PartialEq)]
  pub enum DeliveryOutcome {
      Delivered { channel: String, peer: String },
      Undelivered { reason: &'static str },
      Failed { channel: String, reason: &'static str },
  }
  pub const REASON_NO_ORIGIN: &str = "task_has_no_channel_origin";
  pub const REASON_NO_CHANNEL: &str = "no_channel_configured";
  pub fn deliver_ask(
      outbox: Option<&ChannelOutbox>,
      dest: Option<&AskDestination>,
      task_id: i64,
      concern: &str,
      token: &str,
      deadline_at: time::OffsetDateTime,
  ) -> DeliveryOutcome;
  pub fn delivery_audit_row(
      ask_id: i64, task_id: i64, outcome: &DeliveryOutcome,
  ) -> (&'static str, serde_json::Value);
  ```
  And in `scheduler::audit`: `ACTION_ASK_DELIVERED`, `ACTION_ASK_UNDELIVERED`, `ACTION_ASK_DELIVERY_FAILED`.

- [ ] **Step 1: Write the failing tests**

Create `core/src/scheduler/asks/delivery.rs` with this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    use crate::channel::{ChannelId, ConversationId, PeerId};

    fn dest() -> AskDestination {
        AskDestination {
            channel: ChannelId("matrix".into()),
            peer: PeerId("@horst:srv".into()),
            conversation: ConversationId("!room:srv".into()),
        }
    }

    fn deadline() -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(1_787_000_000).unwrap()
    }

    #[tokio::test]
    async fn a_channel_task_gets_the_rendered_ask_on_its_own_channel() {
        let outbox = ChannelOutbox::new();
        let (tx, mut rx) = mpsc::channel(4);
        outbox.register(ChannelId("matrix".into()), tx);

        let outcome =
            deliver_ask(Some(&outbox), Some(&dest()), 412, "writes outside scratch", "tok9", deadline());
        assert_eq!(
            outcome,
            DeliveryOutcome::Delivered { channel: "matrix".into(), peer: "@horst:srv".into() }
        );

        let sent = rx.recv().await.expect("message queued");
        assert_eq!(sent.conversation.0, "!room:srv");
        assert!(sent.body.contains("/approve tok9"), "{}", sent.body);
        assert!(sent.body.contains("writes outside scratch"), "{}", sent.body);
    }

    /// D3: a `kastellan-cli ask` or scheduled task has no peer to ask. That
    /// is not an error — the ask is durable and the CLI answers it — but it
    /// must leave a row, or an escalation nobody was told about is
    /// indistinguishable from one that was delivered.
    #[test]
    fn a_task_with_no_channel_origin_is_undelivered_and_says_so() {
        let outbox = ChannelOutbox::new();
        let outcome = deliver_ask(Some(&outbox), None, 412, "c", "tok", deadline());
        assert_eq!(outcome, DeliveryOutcome::Undelivered { reason: REASON_NO_ORIGIN });
    }

    /// A daemon with no channel configured at all — the Matrix-less build.
    /// Distinguished from the no-origin case because they mean different
    /// things to an operator reading the trail: one is "this task came from
    /// the CLI", the other is "this host has no way to reach you".
    #[test]
    fn no_outbox_at_all_is_a_distinct_undelivered_reason() {
        let outcome = deliver_ask(None, Some(&dest()), 412, "c", "tok", deadline());
        assert_eq!(outcome, DeliveryOutcome::Undelivered { reason: REASON_NO_CHANNEL });
    }

    /// The bus restarts under the scheduler, so a registered-but-dead queue
    /// is a real state. It must be reported, not swallowed.
    #[test]
    fn a_dead_queue_is_a_failure_carrying_the_transport_reason() {
        let outbox = ChannelOutbox::new();
        let (tx, rx) = mpsc::channel(4);
        outbox.register(ChannelId("matrix".into()), tx);
        drop(rx);

        let outcome = deliver_ask(Some(&outbox), Some(&dest()), 412, "c", "tok", deadline());
        assert_eq!(
            outcome,
            DeliveryOutcome::Failed { channel: "matrix".into(), reason: "queue_closed" }
        );
    }

    #[test]
    fn a_channel_that_is_not_up_yet_is_a_failure_not_a_panic() {
        let outbox = ChannelOutbox::new();
        let outcome = deliver_ask(Some(&outbox), Some(&dest()), 412, "c", "tok", deadline());
        assert_eq!(
            outcome,
            DeliveryOutcome::Failed { channel: "matrix".into(), reason: "no_such_channel" }
        );
    }

    /// The nonce is a live approval token and `audit_log` is readable by
    /// every role that can read the trail. Same rule as `ask.raised`, which
    /// omits it for the same reason — and this is the path that actually
    /// holds the plaintext, so the omission has to be asserted.
    #[test]
    fn no_audit_payload_carries_the_token_the_concern_or_the_body() {
        let cases = [
            DeliveryOutcome::Delivered { channel: "matrix".into(), peer: "@horst:srv".into() },
            DeliveryOutcome::Undelivered { reason: REASON_NO_ORIGIN },
            DeliveryOutcome::Failed { channel: "matrix".into(), reason: "queue_closed" },
        ];
        for outcome in cases {
            let (_action, payload) = delivery_audit_row(7, 412, &outcome);
            let rendered = serde_json::to_string(&payload).unwrap();
            assert!(!rendered.contains("tok9"), "token leaked: {rendered}");
            assert!(!rendered.contains("writes outside scratch"), "concern leaked: {rendered}");
            assert!(!rendered.contains("/approve"), "body leaked: {rendered}");
            assert_eq!(payload["ask_id"], 7);
            assert_eq!(payload["task_id"], 412);
        }
    }

    #[test]
    fn each_outcome_maps_to_its_own_action_and_keeps_its_reason() {
        let (a, _) = delivery_audit_row(
            7, 412,
            &DeliveryOutcome::Delivered { channel: "matrix".into(), peer: "@p".into() },
        );
        assert_eq!(a, ACTION_ASK_DELIVERED);

        let (a, p) =
            delivery_audit_row(7, 412, &DeliveryOutcome::Undelivered { reason: REASON_NO_ORIGIN });
        assert_eq!(a, ACTION_ASK_UNDELIVERED);
        assert_eq!(p["reason"], REASON_NO_ORIGIN);

        let (a, p) = delivery_audit_row(
            7, 412,
            &DeliveryOutcome::Failed { channel: "matrix".into(), reason: "queue_full" },
        );
        assert_eq!(a, ACTION_ASK_DELIVERY_FAILED);
        assert_eq!(p["reason"], "queue_full");
        assert_eq!(p["channel"], "matrix");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib scheduler::asks::delivery 2>&1 | tail -20
```

Expected: FAIL to compile — module not declared.

- [ ] **Step 3: Write the implementation**

Add the three constants to `core/src/scheduler/audit.rs`, next to `ACTION_ASK_APPROVAL_APPLIED`:

```rust
/// `action` written when a raised ask was queued to a channel for delivery
/// (#564 slice 2). `actor='scheduler'`. Payload: `{ask_id, task_id,
/// channel, peer}`.
///
/// "Queued", not "delivered to the human": the transport attempt happens in
/// the channel's own pump afterwards and can still fail, exactly as
/// `channel.replied` means routed rather than delivered.
///
/// The plaintext nonce, the rendered body and the concern text are all
/// deliberately absent — this is the one path that holds the live token.
pub const ACTION_ASK_DELIVERED: &str = "ask.delivered";

/// `action` written when a raised ask was not delivered anywhere, and that
/// is expected (#564 slice 2). `actor='scheduler'`. Payload: `{ask_id,
/// task_id, reason}`.
///
/// Two reasons: the task has no channel origin (a `kastellan-cli ask` or
/// scheduled task — the CLI inbox is its surface), or no channel is
/// configured on this host at all. Distinct from
/// [`ACTION_ASK_DELIVERY_FAILED`], which means a channel existed and
/// refused: without the split, "this task came from the CLI" and "this host
/// cannot reach you" are one row.
pub const ACTION_ASK_UNDELIVERED: &str = "ask.undelivered";

/// `action` written when a raised ask's channel refused the message (#564
/// slice 2). `actor='scheduler'`. Payload: `{ask_id, task_id, channel,
/// reason}`, where `reason` is a fixed `OutboxError` label.
///
/// **Never fails the ask.** The row is already committed and the task is
/// already suspended; the CLI still answers it. This row is the only trace
/// that the human was not told.
pub const ACTION_ASK_DELIVERY_FAILED: &str = "ask.delivery_failed";
```

Prepend to `core/src/scheduler/asks/delivery.rs`:

```rust
//! Delivering a raised ask to the conversation its task came from.
//!
//! **Pure and sync**, deliberately: the decision (where does this go, and
//! did it get there?) and the audit row it produces are both separated from
//! the `await`ing emitter in [`super::lifecycle`]. That is what lets every
//! branch below — including all three failure branches — have a unit test,
//! on a path whose async half needs a live Postgres.
//!
//! **Delivery never fails the ask** (spec D2). By the time anything here
//! runs, `db::asks::raise` has committed: the ask is durable and the task
//! is suspended. A Matrix outage must not turn into a task failure on the
//! one path where the reviewer said a human must decide.
//!
//! Spec: `docs/superpowers/specs/2026-08-19-ask-channel-slice-2-design.md`.

use time::OffsetDateTime;

use crate::channel::ask_message::{render_ask, AskDestination};
use crate::channel::outbox::ChannelOutbox;
use crate::channel::OutgoingMessage;
use crate::scheduler::audit::{
    ACTION_ASK_DELIVERED, ACTION_ASK_DELIVERY_FAILED, ACTION_ASK_UNDELIVERED,
};

/// The task did not come from a channel, so there is nobody to send to.
pub const REASON_NO_ORIGIN: &str = "task_has_no_channel_origin";

/// No channel is configured on this host at all.
pub const REASON_NO_CHANNEL: &str = "no_channel_configured";

/// What happened to one delivery attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeliveryOutcome {
    /// Queued to the channel's outbound pump.
    Delivered { channel: String, peer: String },
    /// Not sent, and expected — see [`REASON_NO_ORIGIN`] / [`REASON_NO_CHANNEL`].
    Undelivered { reason: &'static str },
    /// A channel existed and refused it; `reason` is an `OutboxError` label.
    Failed { channel: String, reason: &'static str },
}

/// Render the ask and queue it to its task's own channel.
///
/// Both `Option`s are "absent is normal, not an error": no destination means
/// a non-channel task (spec D3), no outbox means a daemon built or
/// configured without channels.
pub fn deliver_ask(
    outbox: Option<&ChannelOutbox>,
    dest: Option<&AskDestination>,
    task_id: i64,
    concern: &str,
    token: &str,
    deadline_at: OffsetDateTime,
) -> DeliveryOutcome {
    let Some(dest) = dest else {
        return DeliveryOutcome::Undelivered { reason: REASON_NO_ORIGIN };
    };
    let Some(outbox) = outbox else {
        return DeliveryOutcome::Undelivered { reason: REASON_NO_CHANNEL };
    };

    let msg = OutgoingMessage {
        channel: dest.channel.clone(),
        peer: dest.peer.clone(),
        conversation: dest.conversation.clone(),
        body: render_ask(task_id, concern, token, deadline_at),
    };
    match outbox.try_deliver(msg) {
        Ok(()) => DeliveryOutcome::Delivered {
            channel: dest.channel.0.clone(),
            peer: dest.peer.0.clone(),
        },
        Err(e) => DeliveryOutcome::Failed {
            channel: dest.channel.0.clone(),
            reason: e.as_str(),
        },
    }
}

/// The `(action, payload)` for one delivery outcome.
///
/// Split from [`deliver_ask`] so the mapping is testable without a pool —
/// and so the rule that no payload carries the token, the concern or the
/// rendered body is asserted in one place rather than trusted at three
/// call sites.
pub fn delivery_audit_row(
    ask_id: i64,
    task_id: i64,
    outcome: &DeliveryOutcome,
) -> (&'static str, serde_json::Value) {
    match outcome {
        DeliveryOutcome::Delivered { channel, peer } => (
            ACTION_ASK_DELIVERED,
            serde_json::json!({
                "ask_id": ask_id, "task_id": task_id,
                "channel": channel, "peer": peer,
            }),
        ),
        DeliveryOutcome::Undelivered { reason } => (
            ACTION_ASK_UNDELIVERED,
            serde_json::json!({"ask_id": ask_id, "task_id": task_id, "reason": reason}),
        ),
        DeliveryOutcome::Failed { channel, reason } => (
            ACTION_ASK_DELIVERY_FAILED,
            serde_json::json!({
                "ask_id": ask_id, "task_id": task_id,
                "channel": channel, "reason": reason,
            }),
        ),
    }
}
```

Add `pub mod delivery;` to `core/src/scheduler/asks/mod.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib scheduler::asks::delivery 2>&1 | tail -5
cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: **7 passed**, clippy exit 0.

- [ ] **Step 5: Commit**

```bash
git add core/src/scheduler/asks/delivery.rs core/src/scheduler/asks/mod.rs core/src/scheduler/audit.rs
git commit -m "feat(scheduler): the pure ask-delivery decision and its audit rows

Pure and sync so every branch — including all three failure branches —
has a unit test, on a path whose async half needs a live Postgres. The
async caller does nothing but await the audit insert.

Delivery never fails the ask: by the time this runs, raise has committed
and the task is suspended, so a Matrix outage must not become a task
failure on the one path where the reviewer said a human must decide.

Three rows, not two: 'this task came from the CLI' and 'this host has no
way to reach you' are different facts for an operator reading the trail,
and 'a channel existed and refused' is a third. The token, the concern
and the rendered body are absent from all of them — asserted, because
this is the one path that holds the live approval token.

Refs #564

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: The bus recognises an answer

Implements spec **D5**, **D6**, **D9**, **D12**.

**Files:**
- Modify: `core/src/channel/bus.rs` (`AskResolver`, `PgAskResolver`, `AskWiring`, the `handle_inbound` arm, registration in `spawn`/`shutdown`)
- Modify: `core/src/channel/mod.rs` (`actions::ASK_ANSWER_REJECTED`)
- Modify: `core/src/channel/bus/tests.rs`
- Modify (mechanical, add `None`): `core/tests/channel_bus_e2e.rs`, `core/tests/matrix_channel_e2e.rs` (2 sites), `core/tests/email_channel_e2e.rs`

**Interfaces:**
- Consumes: `ChannelOutbox` (Task 3), `parse_ask_command`/`AskChoice`/`ack_resolved`/`ACK_NOT_ANSWERABLE` (Task 2), `Claimant`/`ResolvedAsk`/`resolve_with_nonce` (Task 1).
- Produces:
  ```rust
  #[async_trait::async_trait]
  pub trait AskResolver: Send + Sync {
      async fn resolve(
          &self, nonce: &kastellan_db::asks::Nonce, choice: &str,
          claimant: &kastellan_db::asks::Claimant,
      ) -> anyhow::Result<Option<kastellan_db::asks::ResolvedAsk>>;
  }
  pub struct PgAskResolver; impl PgAskResolver { pub fn new(pool: sqlx::PgPool) -> Self }
  pub struct AskWiring { pub outbox: Arc<ChannelOutbox>, pub resolver: Arc<dyn AskResolver> }
  // handle_inbound gains a 3rd parameter: asks: Option<&AskWiring>
  // ChannelBus::spawn gains a 6th parameter: asks: Option<AskWiring>
  ```

- [ ] **Step 1: Write the failing tests**

Add to `core/src/channel/bus/tests.rs`. First a recording resolver fake (put it next to the existing fakes):

```rust
/// Records every call so a test can assert the resolver was **not**
/// reached, which is a different claim from "it returned false".
#[derive(Default)]
struct RecordingResolver {
    calls: std::sync::Mutex<Vec<(String, String, String)>>, // (token, choice, attribution)
    reply: Option<kastellan_db::asks::ResolvedAsk>,
}

#[async_trait::async_trait]
impl AskResolver for RecordingResolver {
    async fn resolve(
        &self,
        nonce: &kastellan_db::asks::Nonce,
        choice: &str,
        claimant: &kastellan_db::asks::Claimant,
    ) -> anyhow::Result<Option<kastellan_db::asks::ResolvedAsk>> {
        self.calls.lock().unwrap().push((
            nonce.expose().to_string(),
            choice.to_string(),
            claimant.attribution(),
        ));
        Ok(self.reply)
    }
}

fn wiring(resolver: Arc<RecordingResolver>) -> AskWiring {
    AskWiring { outbox: Arc::new(ChannelOutbox::new()), resolver }
}

fn command_msg(peer: &str, body: &str) -> IncomingMessage {
    IncomingMessage {
        channel: ChannelId("matrix".into()),
        peer: PeerId(peer.into()),
        conversation: ConversationId("!room:srv".into()),
        body: body.into(),
        evidence: None,
    }
}
```

Then the tests:

```rust
/// The mainline: a paired peer's answer resolves the ask, acknowledges it,
/// and — the load-bearing half — **never becomes a task**. A command that
/// fell through to the enqueue path would be handed to the planner as an
/// instruction (spec D5).
#[tokio::test]
async fn an_answer_from_a_paired_peer_resolves_and_never_enqueues() {
    let resolver = Arc::new(RecordingResolver {
        reply: Some(kastellan_db::asks::ResolvedAsk { ask_id: 7, task_id: 412 }),
        ..Default::default()
    });
    let events = FakeEvents::default();
    let ack = handle_inbound(
        &StaticPairings::with(&[("matrix", "@horst:srv")]),
        None,
        Some(&wiring(resolver.clone())),
        &events,
        &command_msg("@horst:srv", "/approve tok9"),
    )
    .await
    .expect("an ack is returned");

    assert!(ack.body.contains("412"), "the ack names the resuming task: {}", ack.body);
    assert_eq!(ack.conversation.0, "!room:srv");
    assert!(events.enqueued().is_empty(), "an answer must never become a task");

    let calls = resolver.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "tok9");
    assert_eq!(calls[0].1, "approve");
    assert_eq!(calls[0].2, "matrix/@horst:srv", "the claimant is the transport's own sender");
}

/// **The load-bearing negative.** An unpaired peer's command must die at
/// `authorize` and never reach the resolver at all. Asserted as "zero
/// calls" rather than "returned None", because a resolver that is reached
/// and refuses is a completely different security posture from one that is
/// never consulted — and only the second is what D5's ordering claims.
#[tokio::test]
async fn an_answer_from_an_unpaired_peer_never_reaches_the_resolver() {
    let resolver = Arc::new(RecordingResolver::default());
    let events = FakeEvents::default();
    let ack = handle_inbound(
        &StaticPairings::with(&[]),
        None,
        Some(&wiring(resolver.clone())),
        &events,
        &command_msg("@stranger:srv", "/approve tok9"),
    )
    .await;

    assert!(ack.is_none());
    assert!(resolver.calls.lock().unwrap().is_empty(), "the resolver must not be consulted");
    assert!(events.actions().contains(&actions::REJECTED_UNPAIRED.to_string()));
}

/// A token that resolves nothing gets the indistinguishable sentence and
/// leaves a countable row — repeated rejections from a paired peer are a
/// signal — but still does not become a task.
#[tokio::test]
async fn an_unanswerable_token_is_acknowledged_without_naming_a_cause() {
    let resolver = Arc::new(RecordingResolver::default()); // reply: None
    let events = FakeEvents::default();
    let ack = handle_inbound(
        &StaticPairings::with(&[("matrix", "@horst:srv")]),
        None,
        Some(&wiring(resolver)),
        &events,
        &command_msg("@horst:srv", "/deny nope"),
    )
    .await
    .expect("an ack is returned");

    assert_eq!(ack.body, ACK_NOT_ANSWERABLE);
    assert!(events.enqueued().is_empty());
    assert!(events.actions().contains(&actions::ASK_ANSWER_REJECTED.to_string()));
}

/// An ordinary message from the same peer must be unaffected — the arm is
/// a narrow recognition, not a new gate on the inbound path.
#[tokio::test]
async fn an_ordinary_message_still_enqueues_with_the_wiring_present() {
    let resolver = Arc::new(RecordingResolver::default());
    let events = FakeEvents::default();
    let ack = handle_inbound(
        &StaticPairings::with(&[("matrix", "@horst:srv")]),
        None,
        Some(&wiring(resolver.clone())),
        &events,
        &command_msg("@horst:srv", "what is my flight's GST?"),
    )
    .await;

    assert!(ack.is_none());
    assert_eq!(events.enqueued().len(), 1);
    assert!(resolver.calls.lock().unwrap().is_empty());
}

/// A bus built without ask wiring must behave byte-identically to the
/// pre-slice-2 bus: `/approve x` is just a message.
#[tokio::test]
async fn without_wiring_a_command_is_an_ordinary_message() {
    let events = FakeEvents::default();
    let ack = handle_inbound(
        &StaticPairings::with(&[("matrix", "@horst:srv")]),
        None,
        None,
        &events,
        &command_msg("@horst:srv", "/approve tok9"),
    )
    .await;

    assert!(ack.is_none());
    assert_eq!(events.enqueued().len(), 1);
}

/// The bus registers its own reply queue into the outbox, which is what
/// makes core-initiated delivery reach the same pump replies go through —
/// and deregisters on shutdown, so a bus going away stops being a delivery
/// target rather than accumulating messages nobody drains.
#[tokio::test]
async fn the_bus_registers_its_channel_and_deregisters_on_shutdown() {
    let outbox = Arc::new(ChannelOutbox::new());
    let resolver: Arc<dyn AskResolver> = Arc::new(RecordingResolver::default());
    let (ch, mut sent) = FakeChannel::new("matrix");

    let bus = ChannelBus::spawn(
        vec![Box::new(ch)],
        Arc::new(StaticPairings::with(&[("matrix", "@horst:srv")])),
        None,
        Arc::new(FakeEvents::default()),
        Box::new(NoCompletions),
        Some(AskWiring { outbox: outbox.clone(), resolver }),
    );

    outbox
        .try_deliver(OutgoingMessage {
            channel: ChannelId("matrix".into()),
            peer: PeerId("@horst:srv".into()),
            conversation: ConversationId("!room:srv".into()),
            body: "core-initiated".into(),
        })
        .expect("a running bus is a delivery target");
    assert_eq!(sent.recv().await.expect("delivered").body, "core-initiated");

    bus.shutdown().await;
    assert_eq!(
        outbox.try_deliver(OutgoingMessage {
            channel: ChannelId("matrix".into()),
            peer: PeerId("@horst:srv".into()),
            conversation: ConversationId("!room:srv".into()),
            body: "after shutdown".into(),
        }),
        Err(OutboxError::NoSuchChannel),
    );
}
```

**Note for the implementer:** `FakeEvents`, `StaticPairings`, `FakeChannel` and `NoCompletions` already exist in this test file (or in `channel::auth`) under those or similar names — read the file and reuse what is there rather than adding parallel fakes. If `FakeEvents` has no `actions()`/`enqueued()` accessor, add the minimal one it needs.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib channel::bus 2>&1 | tail -20
```

Expected: FAIL to compile — `AskWiring`, `AskResolver`, `ASK_ANSWER_REJECTED` undefined and `handle_inbound` takes 4 arguments.

- [ ] **Step 3: Write the implementation**

Add to `core/src/channel/mod.rs`'s `actions` module:

```rust
    /// A paired peer sent a well-formed `/approve`/`/deny` whose token
    /// resolved nothing (#564 slice 2). Carries the channel + peer only —
    /// never the token, never the body.
    ///
    /// **Deliberately does not say why.** Wrong token, already answered,
    /// past its deadline and "not this peer's ask" are one outcome by
    /// construction (`db::asks::resolve_with_nonce`), because splitting
    /// them hands a token-guessing peer an existence oracle. What the row
    /// is for is counting: repeated rejections from a paired peer are a
    /// signal even when no single one is.
    pub const ASK_ANSWER_REJECTED: &str = "channel.ask_answer_rejected";
```

In `core/src/channel/bus.rs`, add the seam and wiring type:

```rust
/// Resolution seam for an answer arriving over a channel.
///
/// A trait because the real implementation needs a `PgPool` and this
/// module's tests are deliberately PG-free (spec D12). Its counterpart
/// [`ChannelOutbox`] gets no trait: the real registry with a drained
/// receiver *is* the perfect fake, so wrapping it would only stop the tests
/// covering the real thing.
#[async_trait::async_trait]
pub trait AskResolver: Send + Sync {
    /// Resolve the ask the nonce correlates to, if `claimant` owns its task.
    /// `Ok(None)` covers every refusal, indistinguishably.
    async fn resolve(
        &self,
        nonce: &kastellan_db::asks::Nonce,
        choice: &str,
        claimant: &kastellan_db::asks::Claimant,
    ) -> anyhow::Result<Option<kastellan_db::asks::ResolvedAsk>>;
}

/// Real DB-backed `AskResolver`.
pub struct PgAskResolver {
    pool: sqlx::PgPool,
}

impl PgAskResolver {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl AskResolver for PgAskResolver {
    async fn resolve(
        &self,
        nonce: &kastellan_db::asks::Nonce,
        choice: &str,
        claimant: &kastellan_db::asks::Claimant,
    ) -> anyhow::Result<Option<kastellan_db::asks::ResolvedAsk>> {
        Ok(kastellan_db::asks::resolve_with_nonce(
            &self.pool,
            nonce,
            claimant,
            &serde_json::json!({"choice": choice}),
        )
        .await?)
    }
}

/// Everything a bus needs to take part in the operator-ask loop: the
/// registry it publishes its outbound queue into, and the resolver it hands
/// answers to. `None` at `spawn` means this bus does neither, and behaves
/// exactly as it did before #564 slice 2.
pub struct AskWiring {
    pub outbox: Arc<super::outbox::ChannelOutbox>,
    pub resolver: Arc<dyn AskResolver>,
}
```

Add the arm to `handle_inbound`, whose signature gains `asks: Option<&AskWiring>` as the **third** parameter, and extend its doc comment with a step between the existing 1 and 2:

```rust
///   1b. **recognise an answer** — if the body parses as `/approve <token>`
///       or `/deny <token>`, it is an answer to a raised ask, not an
///       instruction. Placement is the security content (spec D5): AFTER
///       authorization, so only a paired peer can resolve anything and the
///       claimant is the sender the transport vouched for; and BEFORE
///       screening + enqueue, so an answer can never become a task.
///
///       The injection guard deliberately does **not** run on it (spec D6):
///       the body is a closed set — one of two fixed verbs plus an opaque
///       token — that is parsed into a command and never interpolated into
///       a plan, a prompt or a tool argument, so there is nothing for a
///       screen to protect, and a false positive would block the one action
///       this whole path exists to enable.
```

The code, immediately after the `authorize` match and before `screen_and_classify`:

```rust
    if let Some(wiring) = asks {
        if let Some(cmd) = super::ask_message::parse_ask_command(&msg.body) {
            let claimant =
                kastellan_db::asks::Claimant::new(msg.channel.0.clone(), msg.peer.0.clone());
            let nonce = kastellan_db::asks::Nonce::from_wire(cmd.token);
            let body = match wiring.resolver.resolve(&nonce, cmd.choice.as_str(), &claimant).await
            {
                Ok(Some(resolved)) => {
                    events
                        .audit(
                            crate::scheduler::audit::ACTION_ASK_RESOLVED,
                            serde_json::json!({
                                "ask_id": resolved.ask_id,
                                "task_id": resolved.task_id,
                                "choice": cmd.choice.as_str(),
                                "resolved_by": claimant.attribution(),
                                "via": "channel",
                            }),
                        )
                        .await;
                    super::ask_message::ack_resolved(cmd.choice, resolved.task_id)
                }
                Ok(None) => {
                    events
                        .audit(
                            actions::ASK_ANSWER_REJECTED,
                            serde_json::json!({"channel": msg.channel.0, "peer": msg.peer.0}),
                        )
                        .await;
                    super::ask_message::ACK_NOT_ANSWERABLE.to_string()
                }
                Err(e) => {
                    // Fail closed and say nothing specific: a DB error and a
                    // refused answer must look the same to the peer, or the
                    // error path becomes the existence oracle the refusal
                    // path refuses to be.
                    warn!(error = %e, "ask resolution failed");
                    events
                        .audit(
                            actions::ASK_ANSWER_REJECTED,
                            serde_json::json!({"channel": msg.channel.0, "peer": msg.peer.0}),
                        )
                        .await;
                    super::ask_message::ACK_NOT_ANSWERABLE.to_string()
                }
            };
            return Some(OutgoingMessage {
                channel: msg.channel.clone(),
                peer: msg.peer.clone(),
                conversation: msg.conversation.clone(),
                body,
            });
        }
    }
```

`ChannelBus::spawn` gains `asks: Option<AskWiring>` as its **sixth** parameter. Inside the per-channel loop, right after `senders.insert(id.clone(), tx);`:

```rust
            // Publish this channel's reply queue so core-initiated messages
            // (a raised ask) go through the same pump replies do — one queue
            // per channel, no second delivery path.
            if let Some(w) = &asks {
                w.outbox.register(id.clone(), tx.clone());
            }
```

Note the existing code does `senders.insert(id.clone(), tx)` — change it to `senders.insert(id.clone(), tx.clone())` so the sender can be registered too.

Store what `shutdown` needs on the struct:

```rust
pub struct ChannelBus {
    handles: Vec<JoinHandle<()>>,
    bell: DeathBell,
    /// Kept so `shutdown` can deregister; also keeps the `AskWiring`'s
    /// `Arc`s alive for the bus's lifetime.
    asks: Option<AskWiring>,
    /// The ids registered into the outbox, so shutdown removes exactly what
    /// spawn added.
    registered: Vec<ChannelId>,
}
```

and in `shutdown`, **before** aborting the handles:

```rust
        // Stop being a delivery target first: an ask queued after this point
        // would go into a channel whose pump is about to be aborted, which
        // is a message that vanishes rather than one that fails.
        if let Some(w) = &self.asks {
            for id in &self.registered {
                w.outbox.deregister(id);
            }
        }
```

Finally, add `None` as the sixth argument at the four integration-test call sites listed under **Files**, and `Some(AskWiring { … })` is deferred to Task 8 for the two production ones — for now pass `None` in `matrix_boot.rs` and `email_boot.rs` so the tree compiles.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib channel:: 2>&1 | tail -5
cargo test -p kastellan-core --test channel_bus_e2e 2>&1 | tail -5
cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: all pass (6 new bus tests), clippy exit 0.

- [ ] **Step 5: Commit**

```bash
git add core/src/channel/bus.rs core/src/channel/bus/tests.rs core/src/channel/mod.rs \
        core/src/main/matrix_boot.rs core/src/main/email_boot.rs \
        core/tests/channel_bus_e2e.rs core/tests/matrix_channel_e2e.rs core/tests/email_channel_e2e.rs
git commit -m "feat(channel): the bus recognises /approve and /deny

Placement is the security content. After authorize, so only a paired peer
can resolve anything and the claimant is the sender the transport
vouched for — never anything in the body, which would hand the
entitlement check back to whoever sent it. Before screen_and_classify, so
an answer can never become a task; a command falling through to the
enqueue path would reach the planner as an instruction.

The injection guard deliberately does not run on it: a closed set of two
verbs plus an opaque token is never interpolated into a plan, so there is
nothing for a screen to protect, and a false positive would block the one
action this path exists to enable.

The load-bearing test asserts an unpaired peer's command produces ZERO
resolver calls — 'never consulted' is a different posture from 'consulted
and refused', and only the first is what the ordering claims. A DB error
returns the same sentence as a refusal, so the error path does not become
the existence oracle the refusal path refuses to be.

The bus publishes its own reply queue into the outbox, so a core-initiated
ask goes through the same pump replies do, and deregisters before
aborting its pumps.

Refs #564

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: The scheduler delivers a raised ask

Implements spec **D2**, **D13**, and completes the raise path.

**Files:**
- Modify: `core/src/scheduler/asks/lifecycle.rs` (`raise_and_suspend`)
- Modify: `core/src/scheduler/inner_loop.rs` (`TaskContext.origin`, `run_to_terminal`, the `Escalate` arm)
- Modify: `core/src/scheduler/runner.rs` (`spawn_scheduler`, `lane_loop`)
- Modify: `core/src/scheduler/runner/task_exec.rs` (`run_one`)
- Modify: `core/src/scheduler/agent.rs`, `core/src/scheduler/inner_loop/tests.rs`, `core/tests/router_agent_mock_e2e.rs`, `core/tests/scheduler_inner_loop_e2e.rs` (add `origin: None` to `TaskContext` literals)
- Modify: `core/tests/scheduler_ask_path_e2e.rs` (the new e2e)

**Interfaces:**
- Consumes: `delivery::{deliver_ask, delivery_audit_row}` (Task 5), `AskDestination`/`destination_from_task_payload` (Task 2), `ChannelOutbox` (Task 3).
- Produces:
  ```rust
  // TaskContext gains:
  pub origin: Option<crate::channel::ask_message::AskDestination>,
  // signatures gain one parameter each:
  pub fn spawn_scheduler(..., outbox: Option<Arc<ChannelOutbox>>) -> SchedulerHandle;
  pub async fn run_to_terminal(..., outbox: Option<&ChannelOutbox>) -> Result<InnerLoopResult, InnerLoopError>;
  pub async fn raise_and_suspend(..., outbox: Option<&ChannelOutbox>, dest: Option<&AskDestination>) -> Result<i64, DbError>;
  ```

- [ ] **Step 1: Write the failing test**

Add to `core/tests/scheduler_ask_path_e2e.rs`, following that file's existing harness style:

```rust
/// The whole loop, end to end against a live Postgres: a channel task
/// escalates, the ask is delivered to the outbox carrying a token, that
/// token resolves the ask when presented by the task's own peer, and the
/// task returns to `pending`.
///
/// **This is the only test that proves the delivered token is the token
/// that resolves.** Every pure test on either side passes with the two
/// halves disagreeing — the renderer could print one thing and the
/// resolver expect another, and both suites stay green.
#[test]
fn a_raised_ask_is_delivered_and_its_token_resolves_it() {
    let Some(h) = harness("askdl") else { return };
    h.rt.block_on(async {
        use kastellan_core::channel::{ChannelId, ConversationId, PeerId};
        use kastellan_core::channel::ask_message::{destination_from_task_payload, parse_ask_command};
        use kastellan_core::channel::outbox::ChannelOutbox;
        use kastellan_core::scheduler::asks::raise_and_suspend;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let pool = h.migrated_pool("ask-delivery-e2e").await;
        let pool = &pool;

        let payload = serde_json::json!({
            "kind": "channel", "instruction": "book the flight",
            "channel": "matrix", "peer": "@horst:srv", "conversation": "!room:srv",
        });
        let task_id = tasks::insert_pending(pool, Lane::Fast, payload.clone()).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();

        let outbox = ChannelOutbox::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        outbox.register(ChannelId("matrix".into()), tx);

        let dest = destination_from_task_payload(&payload).expect("destination");
        let ask_id = raise_and_suspend(
            pool, task_id, &escalating_plan(), "sends money to a stranger",
            kastellan_core::cassandra::types::Severity::High, None,
            Some(&outbox), Some(&dest),
        )
        .await
        .expect("raise + deliver");

        // The delivery carried a usable command, into the right room.
        let sent = rx.recv().await.expect("the ask was delivered");
        assert_eq!(sent.conversation.0, "!room:srv");
        let approve = sent
            .body
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("/approve"))
            .expect("an approve command was offered");
        let cmd = parse_ask_command(approve).expect("the offered command parses");

        // ... and that token resolves the ask, for the task's own peer.
        let owner = asks::Claimant::new("matrix", "@horst:srv");
        let resolved = asks::resolve_with_nonce(
            pool, &asks::Nonce::from_wire(cmd.token), &owner,
            &serde_json::json!({"choice": "approve"}),
        )
        .await
        .unwrap()
        .expect("the delivered token resolves the ask it was delivered for");
        assert_eq!(resolved.ask_id, ask_id);
        assert_eq!(resolved.task_id, task_id);
        assert_eq!(tasks::observe_state(pool, task_id).await.unwrap(), "pending");
    });
}

/// A delivery failure must not cost the ask. The registry has no channel,
/// so `try_deliver` fails — and the ask must still be committed, the task
/// still suspended, and `kastellan-cli inbox` still able to answer it.
#[test]
fn a_failed_delivery_still_leaves_a_durable_answerable_ask() {
    let Some(h) = harness("askdf") else { return };
    h.rt.block_on(async {
        use kastellan_core::channel::ask_message::destination_from_task_payload;
        use kastellan_core::channel::outbox::ChannelOutbox;
        use kastellan_core::scheduler::asks::raise_and_suspend;
        use kastellan_db::tasks::Lane;
        use kastellan_db::{asks, tasks};

        let pool = h.migrated_pool("ask-delivery-failure-e2e").await;
        let pool = &pool;

        let payload = serde_json::json!({
            "kind": "channel", "instruction": "book the flight",
            "channel": "matrix", "peer": "@horst:srv", "conversation": "!room:srv",
        });
        let task_id = tasks::insert_pending(pool, Lane::Fast, payload.clone()).await.unwrap();
        tasks::claim_one(pool, Lane::Fast, 60).await.unwrap().unwrap();

        let empty_outbox = ChannelOutbox::new(); // nothing registered
        let dest = destination_from_task_payload(&payload).expect("destination");
        let ask_id = raise_and_suspend(
            pool, task_id, &escalating_plan(), "sends money to a stranger",
            kastellan_core::cassandra::types::Severity::High, None,
            Some(&empty_outbox), Some(&dest),
        )
        .await
        .expect("a delivery failure must not fail the raise");

        assert_eq!(asks::get(pool, ask_id).await.unwrap().unwrap().state, "pending");
        assert_eq!(tasks::observe_state(pool, task_id).await.unwrap(), "awaiting_operator");
        assert!(asks::resolve(
            pool, ask_id, "hherb", &serde_json::json!({"choice": "approve"}),
        ).await.unwrap(), "the CLI must still be able to answer it");
    });
}
```

Reuse the file's existing plan fixture if it has one; otherwise add `fn escalating_plan() -> Plan` modelled on `plan_with_tool` in `scheduler/asks/pure.rs`'s tests.

- [ ] **Step 2: Run the test to verify it fails**

```bash
source "$HOME/.cargo/env"
KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin" \
  cargo test -p kastellan-core --test scheduler_ask_path_e2e 2>&1 | tail -20
```

Expected: FAIL to compile — `raise_and_suspend` takes 6 arguments, not 8.

- [ ] **Step 3: Write the implementation**

**3a.** `raise_and_suspend` in `lifecycle.rs` — two new parameters and the delivery step:

```rust
/// … (keep the existing doc) …
///
/// **Delivery is best-effort and comes last** (spec D2). `raise` has
/// already committed by then: the ask is durable and the task is
/// suspended. Every delivery failure is audited and returns `Ok`, because
/// a Matrix outage must not become a task failure on the one path where
/// the reviewer said a human must decide — and `kastellan-cli inbox` can
/// still answer it.
///
/// Eight parameters: six are the ask being raised, two are where to send
/// it. Bundling them would only move the list to the call site, which has
/// exactly one caller.
#[allow(clippy::too_many_arguments)]
pub async fn raise_and_suspend(
    pool: &PgPool,
    task_id: i64,
    plan: &Plan,
    concern: &str,
    severity: Severity,
    resume_state: Option<&serde_json::Value>,
    outbox: Option<&ChannelOutbox>,
    dest: Option<&AskDestination>,
) -> Result<i64, DbError> {
    let digest = plan_digest(plan);
    let deadline_at = OffsetDateTime::now_utc() + Duration::seconds(deadline_from_env());

    let raised = db_asks::raise(
        pool, task_id, ASK_KIND_PLAN_APPROVAL, concern,
        &serde_json::json!(["approve", "deny"]), Some(&digest), deadline_at, resume_state,
    )
    .await?;

    let db_asks::RaisedAsk { ask_id, nonce } = raised;
    emit_ask_raised(pool, ask_id, task_id, &digest, severity, deadline_at).await;

    // The one place the plaintext nonce is used. It goes into a message
    // body and nowhere else — not the audit row, not a log line.
    let outcome =
        super::delivery::deliver_ask(outbox, dest, task_id, concern, nonce.expose(), deadline_at);
    drop(nonce);

    let (action, payload) = super::delivery::delivery_audit_row(ask_id, task_id, &outcome);
    if let Err(e) = kastellan_db::audit::insert(pool, SCHEDULER_AUDIT_ACTOR, action, payload).await {
        tracing::warn!(ask_id, task_id, error = %e, "audit insert for ask delivery failed (best-effort)");
    }

    Ok(ask_id)
}
```

Add the imports it needs:

```rust
use crate::channel::ask_message::AskDestination;
use crate::channel::outbox::ChannelOutbox;
```

**3b.** `TaskContext` in `inner_loop.rs` gains:

```rust
    /// Where this task came from, if it came from a channel — the routing
    /// an escalation's question is delivered to (#564 slice 2, spec D13).
    ///
    /// Computed once in `runner::task_exec::run_one` from the payload it
    /// already holds, rather than re-read from the DB on the escalation
    /// path. `None` for a `kastellan-cli ask` or scheduled task, whose ask
    /// is answered through `kastellan-cli inbox`.
    pub origin: Option<crate::channel::ask_message::AskDestination>,
```

Set it in `run_one` where `TaskContext` is built:

```rust
        origin: crate::channel::ask_message::destination_from_task_payload(&task.payload),
```

Add `origin: None` to the other 13 `TaskContext` literals (the compiler names every one).

**3c.** Thread `outbox` through. `run_to_terminal` gains `outbox: Option<&ChannelOutbox>` as its last parameter and passes `(outbox, ctx.origin.as_ref())` into `raise_and_suspend`. `run_one` gains `outbox: Option<&ChannelOutbox>`; `lane_loop` gains `outbox: Option<Arc<ChannelOutbox>>`; `spawn_scheduler` gains `outbox: Option<Arc<ChannelOutbox>>` and clones it into both lanes. Keep the existing `#[allow(clippy::too_many_arguments)]` on `lane_loop` and extend its comment to mention the new dependency.

At the `Escalate` arm's call site:

```rust
                        match asks::raise_and_suspend(
                            pool, ctx.task_id, &plan, reason, *sev, Some(&resume_state),
                            outbox, ctx.origin.as_ref(),
                        )
                        .await
```

**3d.** In `core/src/main.rs`, pass `None` to `spawn_scheduler` for now — Task 8 wires the real value.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib 2>&1 | tail -5
KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin" \
  cargo test -p kastellan-core --test scheduler_ask_path_e2e 2>&1 | tail -8
cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: lib tests pass; the e2e reports its existing tests plus the 2 new ones, no `[SKIP]`; clippy exit 0.

- [ ] **Step 5: Commit**

```bash
git add core/src/scheduler/ core/src/main.rs core/tests/scheduler_ask_path_e2e.rs \
        core/tests/router_agent_mock_e2e.rs core/tests/scheduler_inner_loop_e2e.rs
git commit -m "feat(scheduler): a raised ask is delivered to the conversation it came from

raise_and_suspend now renders the ask and queues it to the task's own
channel, after the raise has committed. Every delivery failure is audited
and returns Ok — the ask is durable and the task suspended by then, so a
Matrix outage must not become a task failure on the one path where the
reviewer said a human must decide. An e2e asserts exactly that: with
nothing registered, the ask is still pending, the task still suspended,
and the CLI still resolves it.

TaskContext carries the origin, computed once in run_one from the payload
it already holds rather than re-read on the escalation path. The 14
literals each gaining a field is the point — a new field is a compile
error, not a silent None.

The e2e that matters parses the /approve line out of the delivered
message and resolves the ask with it. It is the only test that proves the
token that was delivered is the token that resolves: every pure test on
either side stays green with the renderer and the resolver disagreeing.

Refs #564

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Daemon wiring

Implements spec **D1**'s "created in `main` before either".

**Files:**
- Modify: `core/src/main.rs`
- Modify: `core/src/main/matrix_boot.rs`
- Modify: `core/src/main/email_boot.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: no new public API. `supervise_matrix_channel` and `supervise_email_channel` each gain a trailing `outbox: Arc<ChannelOutbox>` parameter.

**Honest note on verification:** this task is wiring, and its correctness is not provable by a unit test — the structural guarantees (the bus registers; the scheduler delivers) are already pinned by Tasks 6 and 7. Its verification is the full workspace build plus the live DGX run in Task 9. Do not invent a test that asserts `main` calls a function.

- [ ] **Step 1: Create the outbox and pass it to the scheduler**

In `core/src/main.rs`, immediately before the `spawn_scheduler` call:

```rust
    // The core-initiated-outbound registry, created HERE because both sides
    // need it and neither can own it: the scheduler is spawned on the next
    // line, the channel supervisors below it, and each supervisor restarts
    // its bus underneath. See `channel::outbox`.
    let outbox = std::sync::Arc::new(kastellan_core::channel::outbox::ChannelOutbox::new());

    let scheduler = kastellan_core::scheduler::spawn_scheduler(
        pool.clone(),
        formulator,
        review,
        dispatcher,
        entity_extractor.clone(),
        embedder,
        Some(outbox.clone()),
    );
```

- [ ] **Step 2: Pass it to both channel supervisors**

```rust
    let matrix = matrix_boot::supervise_matrix_channel(&pool, &sandboxes, &force_routing, outbox.clone());
    …
    let email = email_boot::supervise_email_channel(&pool, &sandboxes, &force_routing, outbox.clone());
```

Thread the parameter through each module's `supervise_*` → retry loop → `attempt` function (follow the existing `pool`/`sandboxes` threading exactly), and build the wiring at the `ChannelBus::spawn` call:

```rust
    let asks = kastellan_core::channel::bus::AskWiring {
        outbox,
        resolver: Arc::new(kastellan_core::channel::bus::PgAskResolver::new(pool.clone())),
    };
    BootOutcome::Started(StartedChannel::from_bus(ChannelBus::spawn(
        vec![Box::new(worker.channel)],
        authorizer,
        Some(pairing),
        events,
        Box::new(completed),
        Some(asks),
    )))
```

Do the same in `email_boot.rs`. The email leg is deliberately wired even though `EmailChannel::send` still refuses: that produces an honest `ask.delivery_failed` row rather than a silent drop, and it is the correct behaviour until outbound SMTP lands.

- [ ] **Step 3: Build the whole workspace**

```bash
source "$HOME/.cargo/env"
cargo build --workspace 2>&1 | tail -20
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: clean, clippy exit 0.

- [ ] **Step 4: Run the full workspace suite**

```bash
source "$HOME/.cargo/env"
cargo test --workspace --no-fail-fast 2>&1 | tail -20
```

Expected: zero failures. Compare the total against the pre-slice baseline plus the tests this plan added (15 Task 0 unchanged + 2 Task 1 unit + 5 Task 1 e2e + 14 Task 2 + 8 Task 3 + 3 Task 4 + 7 Task 5 + 6 Task 6 + 2 Task 7 e2e = **+47**). A different delta means a test was lost or double-counted — reconcile it before committing.

- [ ] **Step 5: Commit**

```bash
git add core/src/main.rs core/src/main/matrix_boot.rs core/src/main/email_boot.rs
git commit -m "feat(core): wire the outbox into the daemon

Created in main before the scheduler and before both channel supervisors,
because neither side can own it: the scheduler is spawned first and each
supervisor restarts its bus underneath.

The email leg is wired even though EmailChannel::send still refuses
unconditionally. That is deliberate — it produces an honest
ask.delivery_failed row rather than a silent drop, and it is the correct
behaviour until outbound SMTP lands in email slice 2.

Refs #564

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Gate, deploy, live-verify, and close out the docs

- [ ] **Step 1: Gate on the DGX (authoritative)**

```bash
ssh dgx 'cd ~/src/kastellan && git fetch origin && git checkout feat/564-slice-2-ask-channel && git pull'
ssh dgx 'cd ~/src/kastellan && source $HOME/.cargo/env && export CARGO_TARGET_DIR=$HOME/.cargo-target-slice2 && cargo test --workspace --no-fail-fast -- --nocapture > $HOME/slice2-test.log 2>&1; echo TEST_EXIT=$?'
ssh dgx 'grep -c "^test result" $HOME/slice2-test.log; grep "\[SKIP\]" $HOME/slice2-test.log | sort | uniq -c'
ssh dgx 'cd ~/src/kastellan && source $HOME/.cargo/env && export CARGO_TARGET_DIR=$HOME/.cargo-target-slice2 && cargo clippy --workspace --all-targets -- -D warnings > $HOME/slice2-clippy.log 2>&1; echo CLIPPY_EXIT=$?; grep -c Checking $HOME/slice2-clippy.log'
```

Required: `TEST_EXIT=0`, `CLIPPY_EXIT=0`, and the `[SKIP]` lines are **only** `KASTELLAN_GLINER_RELEX_ENABLE`. Logs under `$HOME`, never `/tmp` — `/tmp` is scrubbed mid-run on both hosts. Count the `Checking` lines: a warm target dir reports a full-workspace pass it never ran.

- [ ] **Step 2: Gate on the Mac**

```bash
source "$HOME/.cargo/env"
CARGO_TARGET_DIR="$HOME/.cargo-target-slice2" cargo test --workspace --no-fail-fast -- --nocapture 2>&1 | tail -20
CARGO_TARGET_DIR="$HOME/.cargo-target-slice2" cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3
```

The Mac is load-bearing in the opposite direction: it is the only host that compiles the launchd backend and every macOS arm.

- [ ] **Step 3: Deploy to the DGX**

```bash
ssh dgx 'cd ~/src/kastellan && bash scripts/build-release.sh'
```

`build-release.sh`, **not** `cargo build --release --workspace` — the latter omits `--features live-matrix`, so the installed matrix worker is a stub that exits 1 and the channel crash-loops. Then install, restart, and confirm the four operator keys survived (`kastellan.env.local` should win, but verify rather than assume):

```bash
ssh dgx 'cd ~/src/kastellan && ./target/release/kastellan-cli install && systemctl --user restart kastellan.target'
ssh dgx 'grep -E "MATRIX|FORCE_ROUTING" ~/.config/kastellan/kastellan.env ~/.config/kastellan/kastellan.env.local'
ssh dgx 'systemctl --user list-units "kastellan*" --no-pager; tail -30 ~/.local/state/kastellan/*.out'
```

Expect `channel bus running {channel:matrix, attempts:1}`.

- [ ] **Step 4: Live-verify the round trip**

From the Matrix room, send a task that reliably escalates. Then confirm, in order:

1. the bot posts the `⚠️ Approval needed — task N` message with two `/approve`/`/deny` lines;
2. `ssh dgx './target/release/kastellan-cli inbox list'` shows the ask pending;
3. replying `/approve <token>` gets the `✓ Approved — task N is resuming.` ack;
4. the task completes and its answer arrives in the same room;
5. the audit trail carries `ask.raised` → `ask.delivered` → `ask.resolved{via:"channel"}` → `task.finalize`.

Then verify the negative that matters: send `/approve <token>` again and confirm it returns `ACK_NOT_ANSWERABLE` (already resolved) and writes `channel.ask_answer_rejected`.

**If nothing escalates naturally**, temporarily lower the escalation bar rather than faking the ask — a hand-inserted row would not exercise the delivery path, which is the whole slice.

- [ ] **Step 5: Update HANDOVER.md and ROADMAP.md**

HANDOVER: new `main` HEAD + the measured baselines from Steps 1–2, slice 2 moved into *Recently merged*, the #564 entry in *Next TODO* reduced to what remains (the `ask_user` planner tool, `propose_plan`, the autonomy ceiling, the dead-letter store), and any live-verification finding worth carrying. ROADMAP: tick slice 2 under the Operator-ask-channel item with the branch, the PR and the decisions worth not re-deriving. Prune both to stay under 500 lines.

- [ ] **Step 6: Open the PR**

```bash
git push -u origin feat/564-slice-2-ask-channel
gh pr create --base main --title "feat(channel,scheduler,db): the ask channel — an escalation reaches the operator on Matrix (#564 slice 2)" --body "…"
```

The body must state: what it delivers, the D16 finding (the nonce is a bearer token, so entitlement had to move into the guard) and that it came from questioning whether messages should be signed, the gate numbers from both hosts, and the live-verification transcript from Step 4. Link `Refs #564`.

---

## Self-review

**Spec coverage.** D1 → Task 3 + Task 8. D2 → Task 5 + Task 7. D3 → Task 5. D4 → Task 3. D5 → Task 6. D6 → Task 6. D7 → Task 2. D8 → Task 1. D9 → Task 2 (`ACK_NOT_ANSWERABLE`) + Task 6. D10 → Task 2 + Task 4. D11 → Task 2. D12 → Task 3 (no trait) + Task 6 (trait). D13 → Task 7. D14 → Task 4. D15 → Task 0. D16 → Task 1. D17 → Task 1. Audit-row table → Tasks 5 and 6. Testing section → distributed across Tasks 1–7, with the mutation set to run during the review wave. Every spec section has a task.

**Type consistency.** `AskDestination`, `AskChoice::as_str`, `AskCommand{choice, token}`, `Claimant::{new, attribution}`, `ResolvedAsk{ask_id, task_id}`, `OutboxError::as_str`, `DeliveryOutcome`, `AskWiring{outbox, resolver}` are each defined once and used with the same names and field types downstream. `parse_ask_command` returns `Option<AskCommand>` everywhere. `resolve_with_nonce` returns `Result<Option<ResolvedAsk>, DbError>` in Task 1 and is consumed as such in Task 6 and Task 7's e2e.

**Known ordering constraint.** Task 6 leaves `matrix_boot.rs`/`email_boot.rs` passing `None`, and Task 7 leaves `main.rs` passing `None`; Task 8 replaces both. That is deliberate — it keeps the tree compiling and every intermediate commit green — but it means **slice 2 does nothing on a real daemon until Task 8 lands**. Do not live-test before then.
