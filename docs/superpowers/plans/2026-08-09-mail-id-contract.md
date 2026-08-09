# Mail id contract (#527) + `full_headers` query spelling (#500) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `mail.get_message` accept the ids localmail actually emits, tell the planner what to send when it does not, fix the `full_headers` query parameter that has never worked, and correct every fixture that was agreeing with the worker instead of the service.

**Architecture:** One pure newtype (`LocalmailId`) widens the *accepted JSON types* for every mail id parameter while keeping the *validated output* strictly numeric, so the set of values that can reach a URL path is unchanged. A pure `explain()` turns a rejected value into text aimed at the planner, which reads it on the next iteration. `detail_path()` translates the tool's boolean `full_headers` into the query spelling the service actually reads. The shared and inline mocks are corrected to the measured live shapes so the failure is reproducible in-repo.

**Tech Stack:** Rust 2021, `serde` / `serde_json`, `kastellan-protocol` JSON-RPC, `kastellan-tests-common` mock HTTP origin.

**Spec:** [`docs/superpowers/specs/2026-08-09-mail-id-contract-design.md`](../specs/2026-08-09-mail-id-contract-design.md) · **Branch:** `fix/527-500-mail-id-contract` · **Closes:** [#527](https://github.com/hherb/kastellan/issues/527), [#500](https://github.com/hherb/kastellan/issues/500)

## Global Constraints

- **Run every `cargo` command in the FOREGROUND.** Never background a `cargo test` / `cargo clippy` and wait on it — that has wedged prior sessions.
- **`source "$HOME/.cargo/env"` first** — cargo is not on the `PATH` for non-interactive shells.
- **`git add <specific files>` only. NEVER `git add -A`** — untracked files in this tree must not be swept in.
- **Clippy is enforced:** `cargo clippy --workspace --all-targets -- -D warnings` must stay at exit 0.
- **All tests pass before every commit.**
- On the Mac, use a private `CARGO_TARGET_DIR` under `$HOME` (e.g. `$HOME/.cache/kastellan-mail-target`) — the IDE's rust-analyzer holds `target/debug/.cargo-lock`. **Never** put it under `/tmp`; macOS scrubs that mid-run.
- **No `cfg(target_os)` code anywhere in this plan**, so both hosts must see the same test count.
- Every id value below is a **measured** live value, not an illustration. Do not "tidy" them.

### The measurement this plan is built on (2026-08-09, live DGX)

```
GET  /v1/messages  -> {"messages":[{"message_id":"37477", …,"account":{"id":"1"}}], "next_cursor":"ZHwy…"}
POST /v1/search    -> {"results" :[{"message_id":"20973", …}],                      "next_cursor":"6f6dd7a731…"}
GET  /v1/accounts  -> [{"id":"1","name":"horst-gmail", …}]
GET  /v1/messages/{id}              -> no `headers` key
GET  /v1/messages/{id}?headers=full -> `headers` key, 19 entries
GET  /v1/messages/{id}?full_headers=true -> no `headers` key   <- what we send today
```

`mail.get_message` is **14 failed / 26 dispatched** all-time. Every other mail tool is 0-failure.

---

## File Structure

| File | Responsibility |
| --- | --- |
| `workers/mail/src/ids.rs` **(new)** | `LocalmailId` + pure `parse_id` + pure `explain`. All id grammar and all planner-facing error text. |
| `workers/mail/src/main.rs` | add `mod ids;` |
| `workers/mail/src/handler.rs` | use `LocalmailId` in three params; add pure `detail_path`; `join_ids` takes `LocalmailId` |
| `workers/mail/tests/mail_e2e.rs` | inline mock → measured shapes; the chained `search → get_message` regression test |
| `tests-common/src/mock_localmail.rs` | three routes → measured shapes; shape pins (these run on **every PR**) |
| `core/src/workers/mail.rs` | `message_id` description rewrite + pin |
| `core/tests/mail_daemon_e2e.rs` | real-tier gate: `results` → `messages`, string-id handling |

`handler.rs` is 450 lines; the new type deliberately does not go there.

---

## Task 1: Correct the shared mock to the measured shapes

**Files:**
- Modify: `tests-common/src/mock_localmail.rs` (routes at ~267-274, ~326-331; consts at ~41)
- Test: same file, `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `CANNED_MESSAGE_ID: i64 = 7` (unchanged) and a new `pub const CANNED_NEXT_CURSOR: &str`. `/v1/search` and `/v1/messages` now serve `message_id` as a **JSON string**; `/v1/messages` keys rows under **`messages`**; `/v1/accounts` serves `id` as a **string**. Task 2's chained test depends on the search route's string id.

Why this task is first and alone: it changes no production code, so it must be green on its own, and Task 2's RED depends on it. The two routes that are already correct (`/v1/changes`, `/v1/messages/{id}`) are **not** touched.

- [ ] **Step 1: Add the cursor const**

In `tests-common/src/mock_localmail.rs`, next to `CANNED_MESSAGE_ID` (~line 41):

```rust
/// A realistic opaque paging token. Base64 of `d|2026-08-08T22:01:58+00:00|37474`,
/// copied from a live `/v1/messages` response. It is here because the live audit
/// log shows the planner pasting this value into `message_id` (3 of 14 failures) —
/// a `null` cursor cannot reproduce that, so the mock would hide it.
pub const CANNED_NEXT_CURSOR: &str = "ZHwyMDI2LTA4LTA4VDIyOjAxOjU4KzAwOjAwfDM3NDc0";
```

- [ ] **Step 2: Correct the `/v1/search` route**

Replace the `/v1/search` arm:

```rust
    } else if path.starts_with("/v1/search") {
        // Shapes measured against the live localmail 2026-08-09: `message_id` is
        // a STRING on this route, exactly as on /v1/changes below. The mock
        // previously served a NUMBER here, which is why a hermetic
        // search -> get_message chain passed while production failed 54% of the
        // time (#527): the worker's `i64` agreed with the mock and not with the
        // service. `results` (not `hits`) is correct and stays.
        json(serde_json::json!({
            "results": [{
                "message_id": CANNED_MESSAGE_ID.to_string(),
                "subject": "invoice",
                "snippet": "…"
            }],
            "next_cursor": CANNED_NEXT_CURSOR
        }).to_string())
```

- [ ] **Step 3: Correct the `/v1/messages` list route**

Replace the `path.starts_with("/v1/messages")` arm (the LAST one, after `is_message_by_id`):

```rust
    } else if path.starts_with("/v1/messages") {
        // Measured live 2026-08-09: the list route keys rows under `messages`
        // (NOT `results` — that is the search route) and serves `message_id` as
        // a STRING. Both differed from this mock.
        json(serde_json::json!({
            "messages": [{
                "message_id": CANNED_MESSAGE_ID.to_string(),
                "subject": "invoice",
                "account": {"id": "1", "name": "horst-gmail"}
            }],
            "next_cursor": CANNED_NEXT_CURSOR
        }).to_string())
```

- [ ] **Step 4: Correct the `/v1/accounts` route**

```rust
    } else if path.starts_with("/v1/accounts") {
        // Measured live: `id` is a STRING here too.
        json(serde_json::json!([{"id": "1", "name": "horst-gmail"}]).to_string())
```

- [ ] **Step 5: Write the shape pins**

Append to the `#[cfg(test)] mod tests` block in the same file (it already has `use super::*`, so the module-private `route` is in scope).

These pins call `route()` **directly** rather than spawning a mock over TCP the way `changes_returns_message_id_and_next_cursor_as_strings` does. `route(head: &str) -> (&'static str, &'static str, Vec<u8>)` is pure (`mock_localmail.rs:218`) and its own doc says so, so a socket buys nothing here and would drag `tokio` into three more tests. `route` 401s without a non-empty bearer, so the helper supplies one:

```rust
    /// Drive the pure router with a minimal well-formed request head.
    /// `route` refuses a request with no non-empty bearer, so one is supplied.
    fn routed(request_line: &str) -> serde_json::Value {
        let head = format!("{request_line}\r\nHost: x\r\nAuthorization: Bearer t\r\n");
        let (status, ctype, body) = route(&head);
        assert!(status.starts_with("200"), "unexpected status {status} for {request_line}");
        assert_eq!(ctype, "application/json", "for {request_line}");
        serde_json::from_slice(&body).expect("json body")
    }

    /// `/v1/search` must serve `message_id` as a JSON string. The mock served a
    /// NUMBER until 2026-08-09, which is precisely why no hermetic test caught
    /// #527: `mail.get_message` takes an `i64`, so the mock agreed with the
    /// worker while the real service disagreed with both.
    #[test]
    fn search_returns_message_id_as_a_string() {
        let v = routed("POST /v1/search HTTP/1.1");
        assert!(
            v["results"][0]["message_id"].is_string(),
            "search message_id must be a JSON string (live localmail serves \"20973\"); got {}",
            v["results"][0]["message_id"]
        );
    }

    /// The list route keys rows under `messages` and serves string ids. It used
    /// `results` + a number, disagreeing with the live service on both counts.
    #[test]
    fn list_messages_keys_rows_under_messages_with_string_ids() {
        let v = routed("GET /v1/messages?limit=50 HTTP/1.1");
        assert!(
            v["messages"].is_array(),
            "list route must key rows under `messages` (that is the live shape; \
             `results` is the SEARCH route); got keys {:?}",
            v.as_object().map(|o| o.keys().collect::<Vec<_>>())
        );
        assert!(
            v["messages"][0]["message_id"].is_string(),
            "list message_id must be a JSON string; got {}",
            v["messages"][0]["message_id"]
        );
    }

    /// `/v1/accounts` serves `id` as a string, like every other id localmail emits.
    #[test]
    fn accounts_return_id_as_a_string() {
        let v = routed("GET /v1/accounts HTTP/1.1");
        assert!(
            v[0]["id"].is_string(),
            "account id must be a JSON string; got {}",
            v[0]["id"]
        );
    }
```

**Watch out:** `route` matches the detail route on `/v1/messages/{CANNED_MESSAGE_ID}` *before* the general `/v1/messages` prefix, and the general arm is the LAST one in the chain. Editing the wrong arm is easy — the list arm is the one after `is_message_by_id`, not the one before it.

- [ ] **Step 6: Run the tests**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-tests-common -- --nocapture
```

Expected: PASS, +3 tests over the previous count for this crate.

- [ ] **Step 7: Check nothing else depended on the old shapes**

```sh
cargo test -p kastellan-worker-mail
```

Expected: PASS (this crate uses its own inline mock, not this one — Task 2 corrects that).

- [ ] **Step 8: Commit**

```bash
git add tests-common/src/mock_localmail.rs
git commit -m "test(mock): serve localmail's real id shapes on search/list/accounts

Measured against the live service 2026-08-09: every id is a JSON string
and the list route keys rows under \`messages\`, not \`results\`. This mock
served numbers and \`results\`, so it agreed with the worker's i64 rather
than with the service — which is why a hermetic search -> get_message
chain passes while production fails 54% of the time (#527).

The two routes email-in consumes were already correct and carry a comment
explaining this exact trap; the three the mail tool consumes were not."
```

---

## Task 2: `LocalmailId` — accept the ids localmail emits, explain the ones it does not

**Files:**
- Create: `workers/mail/src/ids.rs`
- Modify: `workers/mail/src/main.rs` (add `mod ids;`)
- Modify: `workers/mail/src/handler.rs` (`get_message`, `list_messages`, `search` filters passthrough is untouched; `join_ids`)
- Modify/Test: `workers/mail/tests/mail_e2e.rs` (inline mock fidelity + the chained regression test)

**Interfaces:**
- Consumes: Task 1's corrected shared mock (not directly — this task uses `workers/mail`'s own inline mock, corrected here).
- Produces:
  - `pub struct LocalmailId(i64)` with `pub fn get(self) -> i64` and `impl Display`
  - `pub fn parse_id(v: &serde_json::Value) -> Result<i64, String>`
  - `pub fn explain(v: &serde_json::Value) -> String`
  - `impl<'de> serde::Deserialize<'de> for LocalmailId`
  - Task 3 relies on `LocalmailId::get()` when building the detail path.

- [ ] **Step 1: Write the failing chained regression test**

In `workers/mail/tests/mail_e2e.rs`, first correct the inline mock so it tells the truth. Change the two fixture lines:

```rust
            let (status, ctype, body): (&str, &str, Vec<u8>) = if first.contains("GET /v1/accounts")
            {
                // Live localmail serves ids as STRINGS on every route.
                ("200 OK", "application/json", br#"[{"id":"1","name":"work"}]"#.to_vec())
            } else if first.contains("POST /v1/search") {
                // Real localmail keys results under "results" (not "hits") and
                // serves `message_id` as a STRING. Serving a number here is what
                // let #527 hide: the worker's i64 agreed with this fixture and
                // not with the service.
                ("200 OK", "application/json", br#"{"results":[{"message_id":"7"}],"next_cursor":null}"#.to_vec())
            } else if first.contains("GET /v1/messages/7") {
                ("200 OK", "application/json", br#"{"id":"7","subject":"invoice","attachments":[]}"#.to_vec())
            } else if first.contains("/v1/attachments/") && first.contains("/text") {
```

(The `GET /v1/messages/7` arm is **new** — the chained test needs the detail route to answer.)

Then update the two existing assertions that encoded the old fiction, and add the chained test at the end of `mail_worker_stdio_roundtrip_against_mock`:

```rust
    // 1. list_accounts → the mock's one account. Live localmail serves the id
    //    as a string; the worker passes the body through untouched.
    let r = rpc(&mut stdin, &mut stdout, 1, "mail.list_accounts", serde_json::json!({}));
    assert_eq!(r["result"][0]["id"], "1", "resp: {r}");

    // 2. search → a hit under localmail's real "results" key, with the
    //    STRING message_id the real service emits.
    let r = rpc(&mut stdin, &mut stdout, 2, "mail.search", serde_json::json!({"query": "qantas"}));
    assert_eq!(r["result"]["results"][0]["message_id"], "7", "resp: {r}");
```

And a new test in the same file:

```rust
/// The #527 regression, reproduced end to end: take the `message_id` **exactly
/// as `mail.search` returned it** and hand it straight to `mail.get_message`.
///
/// That is what the planner does, and until this fix it failed with
/// `invalid type: string "7", expected i64` — 7 of the 14 live failures. Feeding
/// the value through rather than retyping it as a literal is the whole point of
/// the test: a hand-written `7` passes with or without the fix.
#[test]
fn a_message_id_taken_verbatim_from_a_search_hit_is_accepted() {
    let (base, _mock) = spawn_mock();

    let tmp = std::env::temp_dir().join(format!("mail-chain-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let token_file = tmp.join("token");
    std::fs::write(&token_file, "e2e-token\n").unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_kastellan-worker-mail"))
        .env("KASTELLAN_MAIL_ENDPOINT", &base)
        .env("KASTELLAN_MAIL_TOKEN_FILE", &token_file)
        .env("KASTELLAN_LANDLOCK_PROFILE", "none")
        .env("KASTELLAN_SECCOMP_PROFILE", "none")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mail worker");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    let hit = rpc(&mut stdin, &mut stdout, 1, "mail.search", serde_json::json!({"query": "invoice"}));
    let id = hit["result"]["results"][0]["message_id"].clone();
    assert!(id.is_string(), "fixture must serve the live string shape, got {id}");

    // Verbatim — no parsing, no re-typing.
    let got = rpc(&mut stdin, &mut stdout, 2, "mail.get_message", serde_json::json!({"message_id": id}));
    assert!(
        got.get("error").is_none(),
        "get_message must accept the id search just returned; got {got}"
    );
    assert_eq!(got["result"]["id"], "7", "resp: {got}");

    let _ = child.kill();
}
```

- [ ] **Step 2: Run it to verify it fails**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-mail --test mail_e2e a_message_id_taken_verbatim -- --nocapture
```

Expected: **FAIL**, with the error body containing `invalid type: string "7", expected i64`. This is the in-repo reproduction of the live 54%. If it passes, the inline mock was not corrected — stop and fix Step 1.

- [ ] **Step 3: Create `workers/mail/src/ids.rs`**

```rust
//! Ids in the two shapes localmail actually puts on the wire.
//!
//! localmail serialises **every** id as a JSON string — `"message_id":"37477"`
//! on `/v1/search` and `/v1/messages`, `"id":"1"` on `/v1/accounts` — while this
//! worker interpolates ids into URL paths and so must keep them strictly
//! numeric. Before this type the params took a bare `i64`, so the planner
//! copying an id straight out of a search hit (which is what it does, and the
//! only sane thing for it to do) produced `invalid type: string "17817",
//! expected i64`: 7 of the 14 live `mail.get_message` failures.
//!
//! `LocalmailId` widens the *accepted JSON types* while keeping the *validated
//! output* an `i64`, so the set of values that can reach a URL path is exactly
//! what it was before — this is not a loosening of the traversal guard.
//!
//! The widening stops at the two forms localmail emits. No trimming, no sign,
//! no floats: the type stays a validator, not a repair layer. A value that is
//! not an id gets [`explain`], which is written for the planner rather than for
//! a log reader — `inner_loop` feeds a failed step's error back on the next
//! iteration, so this text is the only chance to correct the mistake.

use std::fmt;

/// A localmail row id, accepted as `37477` or `"37477"`, always yielded as `i64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalmailId(i64);

impl LocalmailId {
    /// The validated numeric id.
    pub fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for LocalmailId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<'de> serde::Deserialize<'de> for LocalmailId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        parse_id(&v).map(LocalmailId).map_err(serde::de::Error::custom)
    }
}

/// Pure: a JSON value → a validated non-negative id, or the planner-facing
/// reason it is not one.
///
/// Accepts a non-negative integer JSON number, or a string of ASCII digits.
/// Everything else — signs, surrounding whitespace, the empty string, floats,
/// negatives, `i64` overflow, and every non-scalar — is refused.
pub fn parse_id(v: &serde_json::Value) -> Result<i64, String> {
    match v {
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) if i >= 0 => Ok(i),
            _ => Err(explain(v)),
        },
        serde_json::Value::String(s) => {
            if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                return Err(explain(v));
            }
            // Digits-only and non-empty, so the only remaining failure is
            // overflowing i64.
            s.parse::<i64>().map_err(|_| explain(v))
        }
        _ => Err(explain(v)),
    }
}

/// What to tell the **planner** when a value is not a usable id.
///
/// Each arm names a mistake the live `audit_log` actually recorded, because
/// this string is fed back into the next planning iteration and a generic
/// "expected i64" has demonstrably not been enough: the same three mistakes
/// recurred across two months.
pub fn explain(v: &serde_json::Value) -> String {
    const WANT: &str = "expected the numeric message_id of a mail.search / \
                        mail.list_messages hit, e.g. 37477 (a number, or a string of digits)";
    match v {
        serde_json::Value::String(s) if s.starts_with("{{") && s.ends_with("}}") => format!(
            "{WANT}. Got the placeholder {:?}: there is NO template substitution in this \
             system — write the literal id from the previous step's output.",
            head(s)
        ),
        serde_json::Value::String(s) => format!(
            "{WANT}. Got {:?}, which is not a number. If this came from next_cursor, that is \
             an opaque paging token and not an id — re-read the message_id field of the hit.",
            head(s)
        ),
        _ => format!("{WANT}. Got {v}."),
    }
}

/// Keep a rejected value short. The cursor cases are long base64/hex blobs and
/// this text goes into the planner's next prompt, where tokens are the budget.
fn head(s: &str) -> String {
    const MAX: usize = 48;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(MAX).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- the accepted grammar, one test per row of the spec's table ---

    #[test]
    fn a_json_number_is_accepted() {
        assert_eq!(parse_id(&json!(37477)), Ok(37477));
    }

    #[test]
    fn a_digit_string_is_accepted() {
        // The shape localmail actually emits, and the 7-of-14 failure case.
        assert_eq!(parse_id(&json!("37477")), Ok(37477));
    }

    #[test]
    fn leading_zeros_are_accepted() {
        // localmail never emits this, but it is unambiguous.
        assert_eq!(parse_id(&json!("0037")), Ok(37));
    }

    #[test]
    fn a_signed_string_is_rejected() {
        // Sign characters are not digits, and no row id is negative.
        assert!(parse_id(&json!("-1")).is_err());
        assert!(parse_id(&json!("+1")).is_err());
    }

    #[test]
    fn surrounding_whitespace_is_rejected_not_trimmed() {
        // Not trimming is deliberate: whitespace means the value came from
        // somewhere it should not have, and repairing it would hide that.
        assert!(parse_id(&json!(" 37477")).is_err());
        assert!(parse_id(&json!("37477 ")).is_err());
    }

    #[test]
    fn the_empty_string_is_rejected() {
        assert!(parse_id(&json!("")).is_err());
    }

    #[test]
    fn a_float_is_rejected() {
        assert!(parse_id(&json!(37.0)).is_err());
        assert!(parse_id(&json!(3.5)).is_err());
    }

    #[test]
    fn a_negative_number_is_rejected() {
        assert!(parse_id(&json!(-1)).is_err());
    }

    #[test]
    fn an_overflowing_digit_string_is_rejected() {
        assert!(parse_id(&json!("99999999999999999999999")).is_err());
    }

    #[test]
    fn non_scalars_and_null_are_rejected() {
        assert!(parse_id(&json!(null)).is_err());
        assert!(parse_id(&json!(true)).is_err());
        assert!(parse_id(&json!([1])).is_err());
        assert!(parse_id(&json!({"id": 1})).is_err());
    }

    // --- the planner-facing text: one test per live failure class ---

    #[test]
    fn a_template_placeholder_is_told_there_is_no_substitution() {
        // 4 of the 14 live failures were exactly this value.
        let m = explain(&json!("{{message_id}}"));
        assert!(m.contains("NO template substitution"), "got: {m}");
        assert!(m.contains("literal id"), "got: {m}");
    }

    #[test]
    fn a_paging_cursor_is_named_as_a_cursor() {
        // 3 of the 14 live failures pasted next_cursor in here.
        let m = explain(&json!("ZHwyMDI2LTA4LTA4VDIyOjAxOjU4KzAwOjAwfDM3NDc0"));
        assert!(m.contains("next_cursor"), "got: {m}");
        assert!(m.contains("paging token"), "got: {m}");
    }

    #[test]
    fn every_explanation_names_the_field_and_gives_an_example() {
        for v in [json!("{{x}}"), json!("ZHwy"), json!(null), json!(-1)] {
            let m = explain(&v);
            assert!(m.contains("message_id"), "must name the field; got: {m}");
            assert!(m.contains("37477"), "must give a concrete example; got: {m}");
        }
    }

    #[test]
    fn a_long_rejected_value_is_truncated() {
        // This text rides into the planner's next prompt, so it must not grow
        // with the offending value. The bound is generous on purpose: the fixed
        // prose is already ~280 chars, and pinning it tighter would make an
        // ordinary wording edit fail this test for no reason. What is being
        // asserted is that 500 chars of input do NOT reach the output.
        let m = explain(&json!("x".repeat(500)));
        assert!(m.len() < 400, "explanation must not grow with the value; got {} chars", m.len());
        assert!(m.contains('…'), "the value should be visibly truncated; got: {m}");
    }
}
```

- [ ] **Step 4: Declare the module**

In `workers/mail/src/main.rs`:

```rust
mod client;
mod handler;
mod ids;
```

- [ ] **Step 5: Use it in the three id parameters**

In `workers/mail/src/handler.rs`, add the import near the top:

```rust
use crate::ids::LocalmailId;
```

Change `get_message`'s params struct (leave the URL alone — that is Task 3):

```rust
        #[derive(serde::Deserialize)]
        struct P {
            message_id: LocalmailId,
            #[serde(default)]
            full_headers: bool,
        }
        let p: P = parse_params(params)?;
        let path = format!("/v1/messages/{}?full_headers={}", p.message_id.get(), p.full_headers);
```

Change `list_messages`'s params struct:

```rust
        #[derive(serde::Deserialize)]
        struct P {
            #[serde(default)]
            account_ids: Option<Vec<LocalmailId>>,
            #[serde(default)]
            folder_ids: Option<Vec<LocalmailId>>,
            #[serde(default)]
            limit: Option<u32>,
            #[serde(default)]
            cursor: Option<String>,
        }
```

And widen `join_ids` to match (it already formats via `to_string`, so only the type changes):

```rust
/// `/v1/accounts` serves ids as strings too, so these arrive in either shape;
/// `LocalmailId` has already validated them to digits by the time they get here.
fn join_ids(v: &[LocalmailId]) -> String {
    v.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")
}
```

- [ ] **Step 6: Run the tests**

```sh
cargo test -p kastellan-worker-mail -- --nocapture
```

Expected: PASS, including `a_message_id_taken_verbatim_from_a_search_hit_is_accepted`.

- [ ] **Step 7: Verify the planner-facing error actually reaches the wire**

```sh
cargo test -p kastellan-worker-mail --test mail_e2e -- --nocapture
```

Then confirm by hand that a bad id produces the new text rather than the serde
default — add this assertion to the chained test before the `child.kill()`:

```rust
    // The other 7 live failures: a cursor and a placeholder must now come back
    // with text the planner can act on, since inner_loop feeds it the error.
    let bad = rpc(&mut stdin, &mut stdout, 3, "mail.get_message",
        serde_json::json!({"message_id": "ZHwyMDI2LTA4LTA4VDIyOjAxOjU4KzAwOjAwfDM3NDc0"}));
    let msg = bad["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("next_cursor"), "cursor must be named; got {bad}");

    let bad = rpc(&mut stdin, &mut stdout, 4, "mail.get_message",
        serde_json::json!({"message_id": "{{message_id}}"}));
    let msg = bad["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("NO template substitution"), "placeholder must be named; got {bad}");
```

Re-run the test. Expected: PASS. If the message is the bare serde text, `parse_params`
is discarding the custom error — investigate before continuing.

- [ ] **Step 8: Clippy**

```sh
cargo clippy -p kastellan-worker-mail --all-targets -- -D warnings
```

Expected: exit 0.

- [ ] **Step 9: Commit**

```bash
git add workers/mail/src/ids.rs workers/mail/src/main.rs workers/mail/src/handler.rs workers/mail/tests/mail_e2e.rs
git commit -m "fix(mail): accept the ids localmail emits, and say why when one is not (#527)

localmail serialises every id as a JSON string, so a planner copying
message_id straight out of a search hit — the only sane thing for it to
do — hit \`invalid type: string \"17817\", expected i64\`. That is 7 of the
14 live get_message failures; the tool is 14/26 all-time.

LocalmailId widens the accepted JSON types while still yielding a
validated i64, so the set of values that can reach a URL path is
unchanged — the traversal guard is not loosened. The remaining 7 failures
were a pasted next_cursor and an invented {{message_id}} template, so a
rejected value now gets text aimed at the planner, which reads it on the
next iteration."
```

---

## Task 3: `full_headers` → the query spelling the service reads (#500)

**Files:**
- Modify: `workers/mail/src/handler.rs` (`get_message`, plus a new pure `detail_path`)
- Test: `workers/mail/src/handler.rs` `#[cfg(test)] mod tests` (~line 326)

**Interfaces:**
- Consumes: `LocalmailId::get()` from Task 2.
- Produces: `fn detail_path(message_id: i64, full_headers: bool) -> String`.

- [ ] **Step 1: Write the failing tests**

Replace the existing `get_message_builds_path` test and add a second:

```rust
    #[test]
    fn get_message_builds_path() {
        // Compact is localmail's default, so the flag is simply omitted.
        let mut h = MailHandler::with_client(client_with(Box::new(PathFake("/v1/messages/5"))));
        h.call("mail.get_message", serde_json::json!({"message_id": 5})).unwrap();
    }

    /// #500: the service reads a differently NAMED query parameter and derives
    /// the flag from its VALUE (`full_headers=(headers == "full")`), so the
    /// `?full_headers=true` this worker used to send was dropped by FastAPI and
    /// the response never carried `headers` — measured against the live service
    /// on 2026-08-09, where `?headers=full` returns 19 headers and
    /// `?full_headers=true` returns none.
    #[test]
    fn get_message_asks_for_full_headers_the_way_localmail_reads_it() {
        let mut h = MailHandler::with_client(client_with(Box::new(PathFake("/v1/messages/5?headers=full"))));
        h.call("mail.get_message", serde_json::json!({"message_id": 5, "full_headers": true})).unwrap();
    }
```

- [ ] **Step 2: Run to verify they fail**

```sh
cargo test -p kastellan-worker-mail --lib get_message -- --nocapture
```

Expected: **FAIL** — `unexpected request path`, because the worker still builds `?full_headers=…`.

- [ ] **Step 3: Add the pure path builder**

In `workers/mail/src/handler.rs`, next to the other pure helpers (near `join_ids`):

```rust
/// localmail's message-detail URL.
///
/// The tool's public parameter is the boolean `full_headers` and stays that way
/// — it is the advertised schema. The service, however, reads a differently
/// *named* query parameter and derives the flag from its *value*
/// (`serve/routes/messages.py::detail`: `full_headers=(headers == "full")`), so
/// FastAPI silently dropped the `?full_headers=<bool>` this worker used to send
/// and every response came back without `headers`. Translating here keeps the
/// mismatch at the one boundary where it belongs.
///
/// Compact is the service's default, so the parameter is omitted rather than
/// sent as `headers=compact`.
fn detail_path(message_id: i64, full_headers: bool) -> String {
    if full_headers {
        format!("/v1/messages/{message_id}?headers=full")
    } else {
        format!("/v1/messages/{message_id}")
    }
}
```

And use it in `get_message`:

```rust
        let p: P = parse_params(params)?;
        self.client
            .get_json(&detail_path(p.message_id.get(), p.full_headers))
            .map_err(mail_err_to_rpc)
```

- [ ] **Step 4: Run to verify they pass**

```sh
cargo test -p kastellan-worker-mail -- --nocapture
cargo clippy -p kastellan-worker-mail --all-targets -- -D warnings
```

Expected: PASS, clippy exit 0.

- [ ] **Step 5: Commit**

```bash
git add workers/mail/src/handler.rs
git commit -m "fix(mail): ask for full headers the way localmail reads it (#500)

The worker sent ?full_headers=<bool>; the service declares a query param
named \`headers\` and derives the flag from its value, so FastAPI dropped
ours and mail.get_message(full_headers: true) has never returned headers
— confirmed against the live service, where ?headers=full yields 19 and
?full_headers=true yields none.

The tool's public parameter stays \`full_headers\`; only the HTTP boundary
changes. Compact is the service default, so the param is now omitted
rather than sent with a false value."
```

---

## Task 4: Stop the description from inviting a placeholder

**Files:**
- Modify: `core/src/workers/mail.rs:136` (the `message_id` `ToolParam`)
- Test: `core/src/workers/mail.rs` `#[cfg(test)] mod tests` (~line 293, next to `advertises_all_six_tools`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: nothing later tasks depend on.

Context the implementer needs: the live audit log shows `"{{message_id}}"` sent 4 times, and the `{{…}}` habit appears on **this tool and no other in the entire log**. There is no template-substitution mechanism anywhere in the tree. The current text — *"message id from a search/list hit"* — tells the model the value comes from another step without naming the field or saying to inline it, which is what induces the placeholder.

- [ ] **Step 1: Write the failing test**

```rust
    /// The `message_id` description is load-bearing, not prose: the live audit
    /// log recorded 4 dispatches sending the literal `{{message_id}}` and 3
    /// sending `next_cursor`, and this tool is the ONLY one in the log that has
    /// ever been given a `{{…}}` placeholder. So the description must name the
    /// field, rule out the adjacent one, and forbid the placeholder.
    ///
    /// Asserted by substring rather than by pinning the whole string: the exact
    /// wording should stay editable, the three commitments should not.
    #[test]
    fn message_id_description_names_the_field_and_rules_out_the_two_live_mistakes() {
        let docs = MailManifest.tool_docs();
        let get = docs
            .iter()
            .find(|d| d.method == "mail.get_message")
            .expect("mail.get_message must be advertised");
        let p = get
            .params
            .iter()
            .find(|p| p.name == "message_id")
            .expect("message_id must be advertised");
        let d = p.description;
        assert!(d.contains("message_id"), "must name the field to read: {d:?}");
        assert!(d.contains("next_cursor"), "must rule out the adjacent cursor: {d:?}");
        assert!(
            d.contains("not a placeholder") || d.contains("literal"),
            "must forbid a template placeholder: {d:?}"
        );
    }
```

- [ ] **Step 2: Run to verify it fails**

```sh
cargo test -p kastellan-core --lib workers::mail -- --nocapture
```

Expected: **FAIL** on `must name the field to read` — the current text is `"message id from a search/list hit"`.

- [ ] **Step 3: Rewrite the description**

In `core/src/workers/mail.rs`, replace the `message_id` param:

```rust
                    ToolParam {
                        name: "message_id",
                        description: "numeric message_id of a mail.search / mail.list_messages hit, \
                                      e.g. 37477 (a number or a digit-string). NOT next_cursor — that \
                                      is a paging token. Use the literal value from the previous step's \
                                      output, not a placeholder.",
                        required: true,
                    },
```

- [ ] **Step 4: Run to verify it passes**

```sh
cargo test -p kastellan-core --lib workers::mail -- --nocapture
cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: PASS, clippy exit 0.

- [ ] **Step 5: Commit**

```bash
git add core/src/workers/mail.rs
git commit -m "fix(mail): stop message_id's description inviting a placeholder (#527)

\"message id from a search/list hit\" tells the model the value comes from
another step without naming the field or saying to inline it — and the
live audit log shows this tool is the ONLY one ever given a {{…}}
placeholder (4 times), plus 3 dispatches that pasted next_cursor.

The replacement names the field, gives a concrete id, rules out the
adjacent cursor and forbids the placeholder. Pinned by substring so the
wording stays editable but the three commitments do not."
```

---

## Task 5: Correct the real-localmail drift gate

**Files:**
- Modify: `core/tests/mail_daemon_e2e.rs:362-378` (inside `mock_localmail_shapes_match_real_localmail`)

**Interfaces:**
- Consumes: nothing.
- Produces: nothing.

Context: this `#[ignore]`d, Mac-only test exists specifically to catch `mock_localmail` drifting from the real service — its own comment says so. It asserts `/v1/messages` keys rows under `results`; the live service returns `messages`. It also reads ids with `as_i64()`, which returns `None` for the string ids the service actually sends, so every row is skipped. Whether the service always did this or changed with localmail's server-side-cursor merge is **not established** — either way the gate must match the measurement.

The `detail_shape_checked` assert below the loop already fails loudly when nothing was exercised, so the silent-skip half is caught — but only after the `results` assert has already failed first.

- [ ] **Step 1: Fix the list-shape assertion**

Replace lines ~362-369:

```rust
    let (_h, list) = curl("GET", "/v1/messages?limit=50", None);
    // The LIST route keys rows under `messages` and the SEARCH route under
    // `results` — they differ, and this gate asserted `results` for both until
    // 2026-08-09. Measured live: `/v1/messages` returns exactly
    // ["messages", "next_cursor"]. get_message's shape is pinned below.
    assert!(
        list.as_ref().and_then(|v| v.get("messages")).map(|r| r.is_array()).unwrap_or(false),
        "real localmail /v1/messages must key rows under `messages`: {list:?}"
    );
```

- [ ] **Step 2: Fix the id read**

Replace lines ~373-378:

```rust
    if let Some(rows) = list.as_ref().and_then(|v| v.get("messages")).and_then(|r| r.as_array()) {
        for row in rows {
            // localmail serves ids as STRINGS. `as_i64()` alone returns None for
            // every row, so this loop used to skip the whole archive and exercise
            // nothing — the silent pass the assert below the loop exists to catch.
            let Some(id) = row
                .get("message_id")
                .or_else(|| row.get("id"))
                .and_then(|v| v.as_i64().map(|i| i.to_string()).or_else(|| v.as_str().map(str::to_owned)))
            else {
                continue;
            };
```

The rest of the loop uses `id` only in `format!("/v1/messages/{id}")`, so a `String` is a drop-in.

- [ ] **Step 3: Compile-check both hosts' view**

```sh
cargo test -p kastellan-core --test mail_daemon_e2e --no-run
cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: compiles, clippy exit 0.

- [ ] **Step 4: Run it against the real service (DGX)**

```sh
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && \
  KASTELLAN_MAIL_ENDPOINT=https://10.0.0.3:8443 \
  KASTELLAN_MAIL_TOKEN=$(cat ~/.config/kastellan/mail-token) \
  cargo test -p kastellan-core --test mail_daemon_e2e mock_localmail_shapes_match_real_localmail -- --ignored --nocapture'
```

Expected: PASS. It has almost certainly never been run green; if it fails on a *different* shape, that is a new finding — record it rather than patching past it.

- [ ] **Step 5: Commit**

```bash
git add core/tests/mail_daemon_e2e.rs
git commit -m "test(mail): the drift gate had drifted from the real service

This #[ignore]d gate exists to catch mock_localmail diverging from real
localmail. It asserted /v1/messages keys rows under \`results\`; the live
service returns \`messages\` (measured 2026-08-09: exactly [\"messages\",
\"next_cursor\"]). And it read ids with as_i64(), which is None for the
strings localmail actually sends — so every row was skipped and the
attachment leg never ran.

Being #[ignore]d and Mac-only is why neither was noticed."
```

---

## Verification (controller, after all tasks)

- [ ] **Full two-host gate.**

DGX (authoritative, whole workspace):

```sh
ssh dgx 'cd ~/src/kastellan && source "$HOME/.cargo/env" && \
  cargo test --workspace -- --nocapture > ~/mail-id-gate.log 2>&1; echo "TEST_EXIT=$?" >> ~/mail-id-gate.log; \
  cargo clippy --workspace --all-targets -- -D warnings >> ~/mail-id-gate.log 2>&1; echo "CLIPPY_EXIT=$?" >> ~/mail-id-gate.log; \
  echo DONE >> ~/mail-id-gate.log'
```

Log goes to `$HOME`, **never `/tmp`** — `/tmp` is scrubbed mid-run on both hosts and has eaten a finished 45-minute gate.

Mac (targeted, private `CARGO_TARGET_DIR` under `$HOME`):

```sh
cargo test -p kastellan-worker-mail -p kastellan-tests-common -- --nocapture
cargo test -p kastellan-core --lib workers::mail -- --nocapture
cargo clippy -p kastellan-worker-mail -p kastellan-tests-common -p kastellan-core --all-targets -- -D warnings
```

- [ ] **Test-count prediction, stated before running.** Baseline DGX **3085**.

| Task | New tests |
| --- | --- |
| 1 — mock shape pins | +3 |
| 2 — `ids.rs` grammar (10) + `explain` (4) + chained e2e (1) | +15 |
| 3 — `full_headers` path | +1 |
| 4 — description pin | +1 |
| 5 — gate correction | 0 |
| **Predicted DGX total** | **3105** |

No `cfg(target_os)` code anywhere in the diff, so **both hosts must see the same delta** — that agreement is the cross-check. If the landed count differs from 3105, **investigate before accepting it**: the #458 gate came in +2 over prediction and the +2 turned out to be the point of the change.

- [ ] **Live acceptance on the DGX**, on a quiet box (a concurrent `cargo test --workspace` makes a live task look like the runaway-thinking bug).

Baseline to beat, measured 2026-08-09: **14 failed / 26 dispatched.**

```sh
ssh dgx 'psql -h /home/hherb/.local/share/kastellan/pg/data/sockets -d kastellan -At -c \
  "SELECT (payload ? '"'"'err'"'"') AS failed, count(*) FROM audit_log WHERE action='"'"'mail.get_message'"'"' AND ts > now() - interval '"'"'1 hour'"'"' GROUP BY 1;"'
```

Confirm all three classes individually:
1. a numeric-string id now **succeeds** (the 7-case),
2. a pasted cursor now returns the `next_cursor` explanation rather than `expected i64`,
3. `full_headers: true` now returns a `headers` key with ~19 entries.

- [ ] **Update `HANDOVER.md` + `ROADMAP.md`** and commit them together with a `docs(handover):` message.
- [ ] **Open the PR** linking #527 and #500, noting #533 and #534 as the filed-not-fixed siblings.

---

## Self-review notes

- **Spec coverage:** id newtype → Task 2; error classifier → Task 2; description → Task 4; #500 → Task 3; mock fidelity → Task 1; chained regression test → Task 2; `tests-common` CI-enforced pins → Task 1; two-host gate + live acceptance → Verification. The real-tier gate correction (Task 5) is **beyond** the spec — it was found while writing the plan and is recorded above with its own evidence.
- **Declined items** from the spec (planner-prompt change; normalising ids on the way out) are correctly absent from every task.
- **Type consistency:** `LocalmailId::get() -> i64` is produced in Task 2 and consumed in Task 3's `detail_path(message_id: i64, …)`; `join_ids(&[LocalmailId])` uses `Display`, defined in Task 2.
- **Known coverage limit, stated rather than hidden:** the chained regression test lives in `workers/mail/tests/`, which CI does **not** run (`linux-check.yml` runs only `cargo test -p kastellan-tests-common`). Only Task 1's shape pins are CI-enforced. Making the behavioural test CI-visible would need a runner change and is out of scope here.
