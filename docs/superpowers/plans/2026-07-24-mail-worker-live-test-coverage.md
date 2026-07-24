# Mail-worker live-test coverage — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the mail worker's three untested legs — OS-sandbox, egress-proxy, daemon/planner — with automated tests, sharing new `tests-common` infrastructure rather than copying it.

**Architecture:** Two shared `kastellan-tests-common` modules (`mock_localmail`, `scripted_llm`) feed two new `core/tests/*` files. `core/tests/mail_e2e.rs` drives the real mail binary under the real jail (direct + force-routed couplings). `core/tests/mail_daemon_e2e.rs` drives the real daemon so an LLM selects `mail.*`. A Mac-only `#[ignore]` contract test pins the mock against real localmail.

**Tech Stack:** Rust (edition per workspace), tokio, serde_json, the existing sandbox/egress/daemon test couplings. No new production code, no new dependencies.

**Spec:** `docs/superpowers/specs/2026-07-24-mail-worker-live-test-coverage-design.md`
**Branch:** `test/mail-worker-live-coverage` (already created; spec committed at `041e5a31`).

## Global Constraints

- **No production-code change** to the mail worker (`workers/mail/`) or its manifest (`core/src/workers/mail.rs`). Test + test-infra only. (If a leg surfaces a real worker bug, fix it under a clearly-labelled step — none is anticipated.)
- **No new dependencies.** `tokio` + `serde_json` are already `tests-common` deps.
- **AGPL-compatible deps only** (project-wide).
- **Cross-platform:** every new test file is `#![cfg(any(target_os = "linux", target_os = "macos"))]` and uses the skip-as-pass posture (`skip_if_no_supervisor` / `skip_if_sandbox_unavailable` / `pg_bin_dir_or_skip` / binary-exists / `egress_proxy_bin_or_skip`).
- **Files under ~500 lines** where feasible; the two shared modules and the two test files each stay well under.
- **Cargo needs its env:** every `cargo` command is preceded by `source "$HOME/.cargo/env"`. On the Mac, run under a scratch `CARGO_TARGET_DIR` if the rust-analyzer build-lock bites ([[mac-cargo-buildlock-prefer-dgx]]); the DGX (`ssh dgx '<cmd>'`) is the Linux acceptance gate.
- **TDD + frequent commits;** all non-skipped tests pass before each commit.
- **The egress full round-trip is deliberately NOT covered** (webpki wall — see Task 6's code comment and the spec §"Out of scope").

---

## File Structure

- Create `tests-common/src/scripted_llm.rs` — the queued multi-shot mock LLM lifted out of `cli_ask_e2e.rs` (URL-routed embed/chat, plan/embedding envelope builders).
- Create `tests-common/src/mock_localmail.rs` — a plain-HTTP canned-response localmail `/v1` origin.
- Modify `tests-common/src/lib.rs` — add `pub mod scripted_llm;` and `pub mod mock_localmail;` (+ doc-comment bullets).
- Modify `core/tests/cli_ask_e2e.rs` — delete the lifted items; import them from `kastellan_tests_common::scripted_llm`.
- Create `core/tests/mail_e2e.rs` — Slice 1 sandbox + egress tiers (1a, 1c, 1b/1d).
- Create `core/tests/mail_daemon_e2e.rs` — Slice 2 planner tiers (2a, 2b) + the Mac-only contract test.

---

## SLICE 1 — shared infra + sandbox/egress legs

### Task 1: Lift the scripted-LLM mock into `tests-common::scripted_llm`

**Files:**
- Create: `tests-common/src/scripted_llm.rs`
- Modify: `tests-common/src/lib.rs` (add `pub mod scripted_llm;` + a doc bullet)
- Modify: `core/tests/cli_ask_e2e.rs` (remove the lifted items, import from the shared module)

**Interfaces:**
- Produces (all `pub`, in `kastellan_tests_common::scripted_llm`):
  - `struct ScriptedLlm { pub base_url: String, pub embed_requests: Arc<Mutex<Vec<String>>>, pub chat_requests: Arc<Mutex<Vec<String>>>, join: Option<tokio::task::JoinHandle<()>> }` (Drop aborts `join`)
  - `async fn spawn_scripted_llm(embed_responses: Vec<String>, chat_responses: Vec<String>) -> ScriptedLlm`
  - `fn envelope_for(plan_json_string: &str) -> String`
  - `fn embedding_envelope() -> String`
  - `fn plan_json(decision: &str, steps: serde_json::Value, result: Option<serde_json::Value>) -> String`
  - module-private (with their unit tests): `EndpointKind`, `classify_endpoint`, `parse_request_path`, `find_double_crlf`, `header_content_length`, `MOCK_MAX_REQUEST_BYTES`

- [ ] **Step 1: Create the module by moving the exact items from `cli_ask_e2e.rs`.**

Create `tests-common/src/scripted_llm.rs`. Move these items **verbatim** from `core/tests/cli_ask_e2e.rs` (current line ranges in parentheses), making the noted edits:

- `MOCK_MAX_REQUEST_BYTES` const (69–72)
- `enum EndpointKind` (81–85)
- `fn classify_endpoint` (95–101)
- `fn parse_request_path` (108–113)
- `struct MockLlm` (172–182) → **rename to `ScriptedLlm`**; make the struct and its `base_url`/`embed_requests`/`chat_requests` fields `pub`
- its `impl Drop` (184–190) → update the type name to `ScriptedLlm`
- `async fn spawn_url_routed_mock` (192–268) → **rename to `pub async fn spawn_scripted_llm`**; its final `MockLlm { … }` becomes `ScriptedLlm { … }`
- `fn find_double_crlf` (270–280)
- `fn header_content_length` (282–305)
- `mod mock_router_unit_tests` (307–350) — the classify/parse pins move with their subjects
- `fn envelope_for` (352–369) → make `pub`
- `fn embedding_envelope` (387–409) → make `pub`
- `fn plan_json` (519–540) → make `pub`

Add the file's imports at the top:

```rust
//! Queued multi-shot mock LLM for daemon e2e tests: an OpenAI-compatible HTTP
//! listener that dispatches canned responses from a per-endpoint FIFO (embed vs.
//! chat), chosen by the request path. Lifted from `core/tests/cli_ask_e2e.rs`
//! (issue: mail-worker live-test coverage) so more than one daemon e2e can drive
//! a scripted planner. The `ScriptedLlm` name distinguishes it from the inert
//! `daemon::MockLlm` (which 503s every request).

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
```

Keep every doc-comment that travels with a moved item (they explain the per-endpoint-queue rationale and the 768-wide embedding filler — load-bearing context).

- [ ] **Step 2: Register the module.**

In `tests-common/src/lib.rs`, add `pub mod scripted_llm;` (alphabetical, before `pub mod serial;`) and a matching bullet in the `# Module layout` doc block:

```rust
//! * [`scripted_llm`] — `ScriptedLlm` + `spawn_scripted_llm` + plan/embedding
//!   envelope builders: the queued multi-shot mock LLM shared by the daemon
//!   e2e tests that drive a scripted planner.
```

- [ ] **Step 3: Re-point `cli_ask_e2e.rs` at the shared module.**

In `core/tests/cli_ask_e2e.rs`: delete every item listed in Step 1. Replace the `use kastellan_tests_common::{…}` list by adding:

```rust
use kastellan_tests_common::scripted_llm::{
    embedding_envelope, envelope_for, plan_json, spawn_scripted_llm, ScriptedLlm,
};
```

Then fix references in the remaining body: every `MockLlm` → `ScriptedLlm`, every `spawn_url_routed_mock(` → `spawn_scripted_llm(`. Leave `cli_ask_e2e.rs`'s local `Daemon`, `bring_up_daemon`, `echo_step`, `cat_passwd_step`, `audit_multiset`, `cluster_for`, `skip_if_any_binary_missing` in place (ask-specific — not lifted).

- [ ] **Step 4: Verify the lift compiles and its pins pass.**

Run:
```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-tests-common scripted_llm -- --nocapture
cargo build --tests -p kastellan-core
```
Expected: the moved `mock_router_unit_tests` pass under `kastellan-tests-common`; `cli_ask_e2e.rs` compiles clean (the lift is behaviour-preserving). Fix any leftover `MockLlm`/`spawn_url_routed_mock` references the compiler flags.

- [ ] **Step 5: Commit.**

```sh
git add tests-common/src/scripted_llm.rs tests-common/src/lib.rs core/tests/cli_ask_e2e.rs
git commit -m "test(infra): lift the scripted-LLM mock into tests-common::scripted_llm

Moves the URL-routed multi-shot mock LLM + plan/embedding envelope builders out
of cli_ask_e2e.rs into a shared module so a second daemon e2e (mail) can drive a
scripted planner. Renamed MockLlm -> ScriptedLlm to distinguish it from the inert
daemon::MockLlm. cli_ask_e2e re-pointed at the shared module (its continued green
is the behaviour-preserving safety net).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `tests-common::mock_localmail` — plain-HTTP canned localmail origin

**Files:**
- Create: `tests-common/src/mock_localmail.rs`
- Modify: `tests-common/src/lib.rs` (add `pub mod mock_localmail;` + a doc bullet)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces (`pub`, in `kastellan_tests_common::mock_localmail`):
  - `struct MockLocalmail { pub base_url: String, join: Option<tokio::task::JoinHandle<()>> }` (Drop aborts `join`)
  - `async fn spawn_mock_localmail() -> MockLocalmail`
  - `const CANNED_SHA256: &str` (64 lowercase hex — the attachment sha the message advertises)
  - `const CANNED_ATTACHMENT_BYTES: &[u8]` (the original-format bytes `get_attachment` delivers)
  - `const CANNED_ATTACHMENT_TEXT: &str` (the extracted text `get_attachment_text` surfaces)
  - `const CANNED_MESSAGE_ID: i64`

- [ ] **Step 1: Write a failing self-test for the mock.**

Create `tests-common/src/mock_localmail.rs` with only the test first (so it fails to compile → RED):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    /// Drive one raw GET /v1/accounts against the mock and confirm it answers
    /// with the localmail `results`-free accounts array shape (a list).
    #[test]
    fn serves_accounts_as_a_json_array() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mock = rt.block_on(spawn_mock_localmail());
        let addr = mock.base_url.strip_prefix("http://").unwrap().to_string();
        let mut s = TcpStream::connect(&addr).unwrap();
        write!(
            s,
            "GET /v1/accounts HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200"), "resp: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap();
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert!(v.is_array(), "accounts must be a JSON array, got {v}");
    }

    /// The attachment-text endpoint must return the localmail envelope shape
    /// `application/json {"text": …}` (the #487 contract), NOT plain text.
    #[test]
    fn attachment_text_is_json_text_envelope() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mock = rt.block_on(spawn_mock_localmail());
        let addr = mock.base_url.strip_prefix("http://").unwrap().to_string();
        let mut s = TcpStream::connect(&addr).unwrap();
        write!(
            s,
            "GET /v1/attachments/{CANNED_SHA256}/text HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        assert!(resp.contains("application/json"), "content-type: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap();
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["text"], CANNED_ATTACHMENT_TEXT);
    }
}
```

- [ ] **Step 2: Run it to confirm RED.**

Run:
```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-tests-common mock_localmail 2>&1 | head -20
```
Expected: FAIL to compile — `spawn_mock_localmail`, `CANNED_SHA256`, `CANNED_ATTACHMENT_TEXT` not found.

- [ ] **Step 3: Implement the mock.**

Prepend to `tests-common/src/mock_localmail.rs` (above the test module):

```rust
//! Plain-HTTP canned-response mock of localmail's `/v1` REST API, serving the
//! six endpoints the mail worker hits in localmail's REAL response shapes (as
//! #487 corrected them: search → `results`, attachment text → `application/json
//! {"text": …}`). Plain HTTP is deliberate: it sidesteps the webpki-only TLS
//! wall entirely (that only bites TLS), so the mail worker's DIRECT transport
//! round-trips hermetically against it. It is NOT reachable via the force-routed
//! transport (HTTPS-only) — see the mail e2e egress tier. Response SHAPES are
//! pinned against real localmail by the Mac-only contract test in
//! `core/tests/mail_daemon_e2e.rs`.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 64 lowercase hex — the attachment sha the canned message advertises and the
/// attachment endpoints key on (the worker validates sha256 shape).
pub const CANNED_SHA256: &str =
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// Extracted text surfaced by `mail.get_attachment_text`.
pub const CANNED_ATTACHMENT_TEXT: &str = "NORTH COAST AREA HEALTH SERVICE invoice total 42.00";
/// Original-format bytes delivered by `mail.get_attachment`.
pub const CANNED_ATTACHMENT_BYTES: &[u8] = b"%PDF-1.4 canned attachment bytes";
/// The message id the canned search/list hits reference.
pub const CANNED_MESSAGE_ID: i64 = 7;

/// A live plain-HTTP localmail mock. Aborts its listener task on drop.
pub struct MockLocalmail {
    pub base_url: String,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for MockLocalmail {
    fn drop(&mut self) {
        if let Some(h) = self.join.take() {
            h.abort();
        }
    }
}

/// Bind an ephemeral loopback port and serve the six `/v1` endpoints. Every
/// request must carry a non-empty `Authorization: Bearer` header (asserted).
pub async fn spawn_mock_localmail() -> MockLocalmail {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");

    let join = tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            // Read until end-of-headers; localmail's search is a POST but its
            // body is not needed to produce a canned page, so we do not wait for
            // a body — the request line + headers are enough to route.
            let mut buf = Vec::with_capacity(1024);
            let mut tmp = [0u8; 512];
            let head = loop {
                let n = match sock.read(&mut tmp).await {
                    Ok(0) | Err(_) => break None,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&tmp[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break std::str::from_utf8(&buf[..i]).ok().map(str::to_owned);
                }
                if buf.len() > 64 * 1024 {
                    break None;
                }
            };
            let (status, ctype, body): (&str, &str, Vec<u8>) = match head.as_deref() {
                Some(h) => route(h),
                None => ("400 Bad Request", "text/plain", b"bad request".to_vec()),
            };
            let resp_head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(resp_head.as_bytes()).await;
            let _ = sock.write_all(&body).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });

    MockLocalmail { base_url, join: Some(join) }
}

/// Pure request-line/headers → (status, content-type, body). Asserts a
/// non-empty bearer so the auth wiring is exercised, then routes by path.
fn route(head: &str) -> (&'static str, &'static str, Vec<u8>) {
    // Bearer presence (auth wiring). A request with no non-empty bearer is a 401.
    let has_bearer = head.lines().any(|l| {
        let mut p = l.splitn(2, ':');
        matches!((p.next(), p.next()), (Some(n), Some(v))
            if n.trim().eq_ignore_ascii_case("authorization")
               && v.trim().strip_prefix("Bearer ").map(|t| !t.trim().is_empty()).unwrap_or(false))
    });
    if !has_bearer {
        return ("401 Unauthorized", "text/plain", b"no bearer".to_vec());
    }
    let path = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/");

    let json = |s: String| ("200 OK", "application/json", s.into_bytes());

    // Order matters: the more specific attachment paths before /v1/messages.
    if path.starts_with("/v1/search") {
        json(serde_json::json!({
            "results": [{"message_id": CANNED_MESSAGE_ID, "subject": "invoice", "snippet": "…"}],
            "next_cursor": serde_json::Value::Null
        }).to_string())
    } else if path.starts_with("/v1/accounts") {
        json(serde_json::json!([{"id": 1, "name": "horst-gmail"}]).to_string())
    } else if path.contains("/text") && path.starts_with("/v1/attachments/") {
        json(serde_json::json!({"text": CANNED_ATTACHMENT_TEXT}).to_string())
    } else if path.starts_with("/v1/attachments/") {
        ("200 OK", "application/pdf", CANNED_ATTACHMENT_BYTES.to_vec())
    } else if path.starts_with(&format!("/v1/messages/{CANNED_MESSAGE_ID}")) {
        json(serde_json::json!({
            "id": CANNED_MESSAGE_ID,
            "subject": "invoice",
            "from": "billing@example.test",
            "body": "please find the invoice attached",
            "attachments": [{
                "filename": "invoice.pdf",
                "sha256": CANNED_SHA256,
                "content_type": "application/pdf",
                "size": CANNED_ATTACHMENT_BYTES.len()
            }]
        }).to_string())
    } else if path.starts_with("/v1/messages") {
        json(serde_json::json!({
            "results": [{"message_id": CANNED_MESSAGE_ID, "subject": "invoice"}],
            "next_cursor": serde_json::Value::Null
        }).to_string())
    } else {
        ("404 Not Found", "text/plain", b"no such endpoint".to_vec())
    }
}
```

- [ ] **Step 4: Register the module.**

In `tests-common/src/lib.rs` add `pub mod mock_localmail;` (before `pub mod pg;`) and a doc bullet:

```rust
//! * [`mock_localmail`] — a plain-HTTP canned-response localmail `/v1` origin
//!   for the mail-worker e2e tiers (real response shapes; pinned by the Mac-only
//!   contract test).
```

- [ ] **Step 5: Run the self-test → GREEN.**

Run:
```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-tests-common mock_localmail -- --nocapture
```
Expected: both tests PASS.

- [ ] **Step 6: Commit.**

```sh
git add tests-common/src/mock_localmail.rs tests-common/src/lib.rs
git commit -m "test(infra): add tests-common::mock_localmail plain-HTTP origin

A canned-response mock of localmail's /v1 API in its real response shapes
(results / application/json {text:…} — the #487 contract), for the mail-worker
sandbox/egress/planner e2e tiers. Plain HTTP sidesteps the webpki TLS wall.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: `core/tests/mail_e2e.rs` — tier 1a (direct round-trip under the real jail)

**Files:**
- Create: `core/tests/mail_e2e.rs`

**Interfaces:**
- Consumes: `kastellan_tests_common::mock_localmail::{spawn_mock_localmail, MockLocalmail}`; `kastellan_core::workers::mail::mail_entry`; `kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec}`; `kastellan_core::secrets::Vault`; `kastellan_tests_common::{backend, bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary, PgCluster}`.
- Produces: the shared `probe_and_pool`, `dispatch_runtime`, `TestEnv`, `ready_or_skip`, `write_token_file` helpers reused by Tasks 4 & 6 in the same file.

- [ ] **Step 1: Scaffold the file with the shared helpers + tier 1a test.**

Create `core/tests/mail_e2e.rs`. Copy the `probe_and_pool` (adjust the `purpose` string to `"mail-e2e"`) and `dispatch_runtime` helpers **verbatim** from `core/tests/web_fetch_e2e.rs:30-50`. Then:

```rust
//! End-to-end: the agent core spawns `kastellan-worker-mail` under the real
//! platform jail (macOS Seatbelt / Linux bwrap) and round-trips `mail.*` calls
//! against a plain-HTTP `mock_localmail` origin.
//!
//! Covers the two legs #487's stdio verification left untested: the OS-sandbox
//! leg (1a direct round-trip, 1c attachment delivery through the jail fs_write
//! boundary) and the egress-proxy leg (1b force-routing coupling). Skips as-pass
//! when PG / the supervisor / the worker binary / a working sandbox is missing.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::workers::mail::mail_entry;
use kastellan_tests_common::mock_localmail::{spawn_mock_localmail, CANNED_SHA256};
use kastellan_tests_common::{
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary, PgCluster,
};

// <-- paste probe_and_pool + dispatch_runtime from web_fetch_e2e.rs here -->

struct TestEnv {
    cluster: PgCluster,
    worker_path: PathBuf,
    token_file: PathBuf,
    _token_dir: tempfile::TempDir,
}

/// Write a 0600 token file into a fresh temp dir; return the dir (kept alive)
/// and the file path (bound into the jail via the mail policy's fs_read).
fn write_token_file() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("mail-token");
    std::fs::write(&path, b"test-bearer-token").expect("write token");
    (dir, path)
}

fn ready_or_skip() -> Option<TestEnv> {
    if skip_if_no_supervisor() || skip_if_sandbox_unavailable() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let worker_path = workspace_target_binary("kastellan-worker-mail");
    if !worker_path.exists() {
        eprintln!("\n[SKIP] mail worker binary not built; run cargo build --workspace\n");
        return None;
    }
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "mail-d",
        "mail-l",
        &format!("kastellan-supervisor-test-pg-mail-{suffix}"),
    );
    let (token_dir, token_file) = write_token_file();
    Some(TestEnv { cluster, worker_path, token_file, _token_dir: token_dir })
}

#[test]
fn direct_search_round_trips_under_the_jail() {
    let env = match ready_or_skip() {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let mock = spawn_mock_localmail().await;
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let policy = mail_entry(
            env.worker_path.clone(),
            &mock.base_url,
            &env.token_file.to_string_lossy(),
        )
        .policy;
        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec { policy: &policy, program: &worker_str, args: &[], wall_clock_ms: None };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn mail under sandbox");

        let result = dispatch(
            &pool,
            &Vault::new(),
            &mut sworker,
            "mail",
            "mail.search",
            serde_json::json!({"query": "invoice"}),
        )
        .await
        .expect("mail.search round trip (worker under jail → mock localmail)");

        assert!(result["results"].is_array(), "expected a results array, got: {result}");

        let _ = sworker.close();
        pool.close().await;
    });
}
```

Add `tempfile` to `core/[dev-dependencies]` if not already present (`grep '^tempfile' core/Cargo.toml`; the workspace already vendors it — use `tempfile = { workspace = true }`).

- [ ] **Step 2: Build + run (verify it compiles; passes or skips).**

Run on the DGX (real bwrap + PG), the Linux acceptance gate:
```sh
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && cargo build --workspace 2>&1 | tail -3 && cargo test -p kastellan-core --test mail_e2e direct_search -- --nocapture 2>&1 | tail -20'
```
Expected: compiles; `direct_search_round_trips_under_the_jail` PASSES (worker binary is built by `--workspace`). On a host missing PG/supervisor it prints `[SKIP]` and passes.

- [ ] **Step 3: Commit.**

```sh
git add core/tests/mail_e2e.rs core/Cargo.toml
git commit -m "test(mail): tier 1a — mail.search round-trips under the real jail

Spawns kastellan-worker-mail under the real Seatbelt/bwrap sandbox and dispatches
mail.search against a plain-HTTP mock_localmail origin, proving the OS-sandbox
leg + the direct transport reach the endpoint. DGX-verified.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: `mail_e2e.rs` — tier 1c (attachment delivery through the jail `fs_write` boundary)

**Files:**
- Modify: `core/tests/mail_e2e.rs`

**Interfaces:**
- Consumes: Task 3's helpers; `kastellan_core::tool_host::apply_workspace_out`.
- Produces: nothing new.

- [ ] **Step 1: Add the tier-1c test.**

Append to `core/tests/mail_e2e.rs`. This exercises the production Phase-A durable-out path (`apply_workspace_out` pushes `fs_write` + `KASTELLAN_WORKER_OUT`), then drives `get_message` → `get_attachment`:

```rust
#[test]
fn attachment_delivered_into_the_task_out_dir() {
    use kastellan_core::tool_host::apply_workspace_out;
    use kastellan_tests_common::mock_localmail::{CANNED_ATTACHMENT_BYTES, CANNED_MESSAGE_ID};

    let env = match ready_or_skip() {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let mock = spawn_mock_localmail().await;
        let pool = probe_and_pool(&env.cluster.conn_spec).await;

        // Durable per-task out dir, bound writable into the jail exactly as the
        // lane runner does in production.
        let out_dir = tempfile::tempdir().expect("out tempdir");
        let mut policy = mail_entry(
            env.worker_path.clone(),
            &mock.base_url,
            &env.token_file.to_string_lossy(),
        )
        .policy;
        apply_workspace_out(&mut policy, out_dir.path());

        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec { policy: &policy, program: &worker_str, args: &[], wall_clock_ms: None };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn mail under sandbox");

        // get_message returns the attachment sha the agent then delivers.
        let msg = dispatch(
            &pool, &Vault::new(), &mut sworker, "mail", "mail.get_message",
            serde_json::json!({"message_id": CANNED_MESSAGE_ID}),
        )
        .await
        .expect("mail.get_message");
        let sha = msg["attachments"][0]["sha256"].as_str().expect("attachment sha");
        assert_eq!(sha, CANNED_SHA256);

        let out = dispatch(
            &pool, &Vault::new(), &mut sworker, "mail", "mail.get_attachment",
            serde_json::json!({"sha256": sha, "filename": "invoice.pdf"}),
        )
        .await
        .expect("mail.get_attachment writes to the jailed out dir");

        let path = out["path"].as_str().expect("delivered path");
        assert!(
            std::path::Path::new(path).starts_with(out_dir.path()),
            "delivered file must be under the task out dir: {path}"
        );
        let bytes = std::fs::read(path).expect("read delivered file");
        assert_eq!(bytes, CANNED_ATTACHMENT_BYTES, "delivered bytes must match the origin");

        let _ = sworker.close();
        pool.close().await;
    });
}
```

- [ ] **Step 2: Run on the DGX.**

```sh
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && cargo test -p kastellan-core --test mail_e2e attachment_delivered -- --nocapture 2>&1 | tail -20'
```
Expected: PASS — the file lands under the jailed `out/` and its bytes match. (If it fails with a write/permission error, the `fs_write` Landlock/Seatbelt binding is the suspect — that IS the leg under test; do not loosen the policy, investigate the binding.)

- [ ] **Step 3: Commit.**

```sh
git add core/tests/mail_e2e.rs
git commit -m "test(mail): tier 1c — attachment delivery through the jail fs_write boundary

Applies the production apply_workspace_out durable-out path, then drives
get_message -> get_attachment under the real jail and asserts the original-format
file lands under the task out dir with the origin's bytes. Exercises the fs_write
Landlock/Seatbelt-write leg unique to mail.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: `mail_e2e.rs` — tier 1b/1d (egress force-routing coupling + allowlist scoping)

**Files:**
- Modify: `core/tests/mail_e2e.rs`

**Interfaces:**
- Consumes: `kastellan_core::egress::net_worker::{spawn_forced_net_worker, NetWorkerSpawn}`; `kastellan_core::egress::audit::EgressAuditRow`; `kastellan_core::tool_host::WorkerSpec`; `kastellan_sandbox::Net`; `kastellan_tests_common::{backend, egress_proxy_bin_or_skip, unique_suffix}`.
- Produces: nothing new.

- [ ] **Step 1: Add the tier-1b/1d test.**

Append to `core/tests/mail_e2e.rs`. This is the coupling/policy-level egress test — it drives the proxy **host-side** (as `egress_force_routing_e2e` does), because a full mail round-trip through the force-routed tunnel is structurally impossible. Reuse `short_scratch_root` / `minted_uds` / `assert_connect_established` by copying them verbatim from `core/tests/egress_force_routing_e2e.rs:54-96` (they are small, self-contained, and not exported):

```rust
// vvv copy short_scratch_root, minted_uds, assert_connect_established,
//     and `const UDS_FILE_NAME` from egress_force_routing_e2e.rs vvv

/// Tier 1b (egress leg) + 1d (allowlist scoping). Brings up a per-worker egress
/// sidecar from MAIL's real derived allowlist via the production
/// `spawn_forced_net_worker` coupling and asserts the sidecar enforces exactly
/// mail's endpoint host:port (allowed), blocks an off-allowlist host AND a wrong
/// loopback port (403 — the 1d scoping assertion), ingests both decisions, and
/// tears down 1:1.
///
/// NOTE: a full mail-JSON round-trip through this tunnel is NOT tested and is
/// not hermetically possible — the force-routed transport (`proxy_connect.rs`)
/// is HTTPS-only and the proxy's MITM upstream (`egress-proxy/pins.rs::
/// build_upstream_client_config`) trusts webpki roots only (pins only
/// strengthen; no origin-CA knob). A plain-HTTP or self-signed loopback origin
/// is therefore unreachable — the #473 wall. The full round-trip is deferred to
/// a real publicly-trusted-cert localmail (see the spec's "Out of scope").
#[test]
fn mail_policy_force_routes_and_enforces_its_endpoint_allowlist() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use kastellan_core::egress::net_worker::{spawn_forced_net_worker, NetWorkerSpawn};
    use kastellan_sandbox::Net;

    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(proxy) = egress_proxy_bin_or_skip() else {
        eprintln!("[SKIP] egress-proxy binary not built");
        return;
    };

    dispatch_runtime().block_on(async {
        let mock = spawn_mock_localmail().await;
        let (_token_dir, token_file) = write_token_file();
        let worker_path = workspace_target_binary("kastellan-worker-mail");

        // Derive the allowlist from mail's REAL manifest policy (proving the
        // manifest wiring produces a force-routable Net::Allowlist).
        let mail_policy = mail_entry(worker_path, &mock.base_url, &token_file.to_string_lossy()).policy;
        let allowlist: Vec<String> = match &mail_policy.net {
            Net::Allowlist(v) => v.clone(),
            other => panic!("mail must be Net::Allowlist, got {other:?}"),
        };
        // mock.base_url is http://127.0.0.1:<port>; the derived entry is that host:port.
        let endpoint_hostport = mock.base_url.strip_prefix("http://").unwrap().to_string();
        assert_eq!(allowlist, vec![endpoint_hostport.clone()], "1d: allowlist is exactly the endpoint");

        let scratch_root = short_scratch_root(&format!("mail-{}", unique_suffix()));
        let actions = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = {
            let actions = Arc::clone(&actions);
            move |row: kastellan_core::egress::audit::EgressAuditRow| {
                actions.lock().unwrap().push(row.action);
            }
        };

        // The worker doesn't drive the proxy here (the host does); a long-lived
        // program keeps the worker + sidecar up. `/bin/sleep` resolves on both
        // macOS and Linux.
        let policy = mail_policy;
        let spec = WorkerSpec { policy: &policy, program: "/bin/sleep", args: &["30"], wall_clock_ms: None };
        let backend = backend();
        let params = NetWorkerSpawn {
            backend: backend.as_ref(),
            sidecar_backend: backend.as_ref(),
            proxy_bin: &proxy,
            spec: &spec,
            allowlist: &allowlist,
            worker_name: "mail",
            secret_fingerprints: &[],
            cert_pins_json: None,
            disable_mitm: false,
        };
        let mut worker = spawn_forced_net_worker(&params, &scratch_root, sink)
            .expect("force-routed mail worker + sidecar spawn");
        let uds = minted_uds(&scratch_root);

        // Allowed: CONNECT to mail's endpoint host:port establishes a tunnel.
        let mut ok = UnixStream::connect(&uds).expect("connect coupling UDS");
        write!(ok, "CONNECT {endpoint_hostport} HTTP/1.1\r\n\r\n").unwrap();
        assert_connect_established(&mut ok);
        drop(ok);

        // 1d: an off-allowlist host is blocked (403).
        let mut bad_host = UnixStream::connect(&uds).unwrap();
        write!(bad_host, "CONNECT evil.test:443 HTTP/1.1\r\n\r\n").unwrap();
        let mut r1 = String::new();
        let _ = bad_host.read_to_string(&mut r1);
        assert!(r1.starts_with("HTTP/1.1 403"), "off-host must 403, got {r1:?}");
        drop(bad_host);

        // 1d: a wrong loopback PORT is blocked (proves port-scoping, not host-only).
        let wrong_port = format!("127.0.0.1:{}", pick_other_port(&endpoint_hostport));
        let mut bad_port = UnixStream::connect(&uds).unwrap();
        write!(bad_port, "CONNECT {wrong_port} HTTP/1.1\r\n\r\n").unwrap();
        let mut r2 = String::new();
        let _ = bad_port.read_to_string(&mut r2);
        assert!(r2.starts_with("HTTP/1.1 403"), "wrong port must 403, got {r2:?}");
        drop(bad_port);

        // Both decisions reached the ingest sink.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            {
                let seen = actions.lock().unwrap();
                let allowed = seen.iter().any(|a| a == "egress.allowed");
                let blocked = seen.iter().any(|a| a == "egress.blocked.allowlist");
                if allowed && blocked { break; }
            }
            assert!(Instant::now() < deadline, "ingest sink missed a decision: {:?}", *actions.lock().unwrap());
            std::thread::sleep(Duration::from_millis(50));
        }

        // 1:1 teardown.
        worker.kill().ok();
        drop(worker);
        let down = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&uds).is_ok() {
            assert!(Instant::now() < down, "sidecar kept serving after worker drop (teardown not 1:1)");
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_dir_all(&scratch_root);
    });
}

/// A loopback port guaranteed different from `hostport`'s port (for the
/// port-scoping 1d assertion). Returns `endpoint_port ^ 1` (still a valid,
/// almost-certainly-unbound port).
fn pick_other_port(hostport: &str) -> u16 {
    let p: u16 = hostport.rsplit(':').next().unwrap().parse().unwrap();
    p ^ 1
}
```

- [ ] **Step 2: Run on the DGX (real netns + proxy).**

```sh
ssh dgx 'source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && cd ~/src/kastellan && cargo test -p kastellan-core --test mail_e2e mail_policy_force_routes -- --nocapture 2>&1 | tail -25'
```
Expected: PASS. Also run on the Mac (Seatbelt) under a scratch target dir — the coupling is cross-platform. If the egress-proxy binary is absent it `[SKIP]`s.

- [ ] **Step 3: Commit.**

```sh
git add core/tests/mail_e2e.rs
git commit -m "test(mail): tier 1b/1d — egress force-routing coupling + allowlist scoping

Drives spawn_forced_net_worker from mail's real derived allowlist and asserts the
sidecar enforces exactly mail's endpoint host:port (off-host AND wrong-port 403 =
the 1d scoping assertion), ingests both decisions, tears down 1:1. Documents why
the full round-trip is not hermetic (HTTPS-only transport + webpki-only MITM
upstream, the #473 wall).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 4: Slice-1 acceptance gate (DGX full workspace).**

```sh
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && cargo test --workspace 2>&1 | tail -5 && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3'
```
Expected: full workspace green (record the passed/failed/ignored counts for the handover), clippy `-D warnings` clean, 0 `[SKIP]` regressions. This is the Slice-1 merge gate.

---

## SLICE 2 — planner leg + fidelity contract

### Task 6: `core/tests/mail_daemon_e2e.rs` — tier 2a (scripted planner selects mail.*)

**Files:**
- Create: `core/tests/mail_daemon_e2e.rs`

**Interfaces:**
- Consumes: `kastellan_tests_common::daemon::bring_up_daemon`; `kastellan_tests_common::scripted_llm::{spawn_scripted_llm, envelope_for, embedding_envelope, plan_json}`; `kastellan_tests_common::mock_localmail::spawn_mock_localmail`; `kastellan_tests_common::{cli_binary, core_binary, bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, skip_if_sandbox_unavailable, unique_suffix, unique_temp_root, current_username, workspace_target_binary}`.
- Produces: nothing (leaf tests).

Study `core/tests/cli_ask_e2e.rs::ask_subprocess_completes_planned_task_end_to_end` (lines 593+) for the exact task-submit + audit-multiset assertion mechanics; the mail version differs only in the plan steps, the worker registration (mail trio, no allowlist seed), and `KASTELLAN_EGRESS_FORCE_ROUTING=0`.

- [ ] **Step 1: Write the tier-2a test.**

Create `core/tests/mail_daemon_e2e.rs`:

```rust
//! End-to-end: the real `kastellan` daemon, given a scripted plan, registers +
//! advertises + dispatches `mail.*` and the result flows back to task
//! completion — proving the daemon/planner leg. The mail worker runs under the
//! real sandbox against a plain-HTTP `mock_localmail`. Force-routing is off
//! (KASTELLAN_EGRESS_FORCE_ROUTING=0) so the daemon worker takes the DIRECT path
//! to the plain-HTTP mock (the force-routed path can't reach a plain-HTTP/
//! self-signed origin — the webpki wall covered structurally by mail_e2e's 1b).
//!
//! Skips as-pass without PG / supervisor / sandbox / the mail+cli+core binaries.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use kastellan_tests_common::daemon::bring_up_daemon;
use kastellan_tests_common::mock_localmail::spawn_mock_localmail;
use kastellan_tests_common::scripted_llm::{
    embedding_envelope, envelope_for, plan_json, spawn_scripted_llm,
};
use kastellan_tests_common::{
    bring_up_pg_cluster, cli_binary, core_binary, current_username, pg_bin_dir_or_skip,
    skip_if_no_supervisor, skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary,
};

/// The single plan step that calls the mail tool.
fn mail_search_step() -> serde_json::Value {
    serde_json::json!([{
        "tool":           "mail",
        "method":         "mail.search",
        "parameters":     {"query": "invoice"},
        "returns":        "results",
        "done_when":      "true",
        "classification": "Public",
    }])
}

#[test]
fn daemon_planner_dispatches_mail_search_end_to_end() {
    for (label, p) in &[
        ("kastellan", core_binary()),
        ("kastellan-cli", cli_binary()),
        ("kastellan-worker-mail", workspace_target_binary("kastellan-worker-mail")),
    ] {
        if !p.exists() {
            eprintln!("\n[SKIP] {label} binary missing at {}; cargo build --workspace\n", p.display());
            return;
        }
    }
    if skip_if_no_supervisor() || skip_if_sandbox_unavailable() {
        return;
    }
    let Some(_bin_dir) = pg_bin_dir_or_skip() else { return };

    let suffix = unique_suffix();
    let user = current_username();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();

    rt.block_on(async {
        let mock_mail = spawn_mock_localmail().await;

        // Scripted planner: iteration 1 embeds (recall), then a terminal plan
        // whose single step calls mail.search and completes.
        let plan = plan_json(
            "Execute",
            mail_search_step(),
            Some(serde_json::json!({"kind": "text", "body": "done"})),
        );
        let scripted = spawn_scripted_llm(
            vec![embedding_envelope()],
            vec![envelope_for(&plan)],
        )
        .await;

        // PG cluster (kept in an outer scope so it lives across the daemon run).
        let cluster = bring_up_pg_cluster(
            &pg_bin_dir_or_skip().unwrap(),
            "maild-d", "maild-l",
            &format!("kastellan-supervisor-test-pg-maild-{suffix}"),
        );

        // Mail worker registration (endpoint = mock; token file; binary path) +
        // force-routing OFF so the direct transport reaches the plain-HTTP mock.
        let token_dir = tempfile::tempdir().unwrap();
        let token_file = token_dir.path().join("mail-token");
        std::fs::write(&token_file, b"test-bearer-token").unwrap();
        // extra_env registers the mail worker in the daemon's own registry (the
        // #179 invariant: the operator CLI subprocess omits these).
        let extra_env = vec![
            ("KASTELLAN_MAIL_ENDPOINT".into(), mock_mail.base_url.clone()),
            ("KASTELLAN_MAIL_TOKEN_FILE".into(), token_file.to_string_lossy().into_owned()),
            ("KASTELLAN_MAIL_BIN".into(), workspace_target_binary("kastellan-worker-mail").to_string_lossy().into_owned()),
            ("KASTELLAN_EGRESS_FORCE_ROUTING".into(), "0".into()),
        ];

        // bring_up_daemon sets KASTELLAN_DATA_DIR from the cluster's data_dir, so
        // the daemon connects to the per-test PG cluster; mock_base_url is the
        // scripted LLM (bring_up_daemon appends /v1). NO DATABASE_URL — the
        // daemon + cli both locate PG via KASTELLAN_DATA_DIR (mirror cli_ask_e2e).
        let (_daemon, _guards) = bring_up_daemon(
            "maild", &suffix, &cluster.data_dir, &scripted.base_url, &user, extra_env,
        );

        // Submit a mail-ish task via the real cli subprocess; it connects to the
        // same cluster via KASTELLAN_DATA_DIR (mirror cli_ask_e2e's env set).
        let out = Command::new(cli_binary())
            .args(["ask", "find my latest invoice email"])
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .env("USER", &user)
            .env("KASTELLAN_DATA_DIR", cluster.data_dir.to_string_lossy().as_ref())
            .output()
            .expect("run kastellan-cli ask");
        assert!(out.status.success(), "cli ask failed: {}", String::from_utf8_lossy(&out.stderr));

        // Assert the scripted planner was told mail exists (the <tools> block),
        // and that mail.search actually dispatched.
        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec).await.unwrap();
        let chat0 = scripted.chat_requests.lock().unwrap().first().cloned().unwrap_or_default();
        assert!(chat0.contains("mail.search"), "planner <tools> must advertise mail.search");

        let dispatched: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_log WHERE action = 'tool.mail.dispatched' OR (actor = 'scheduler' AND action LIKE '%mail%')",
        )
        .fetch_one(&pool)
        .await
        .unwrap_or(0);
        // The exact audit action string is confirmed in Step 2 from the real
        // schema; adjust the predicate to the actual `mail.*` dispatch row.
        assert!(dispatched >= 1, "expected a mail dispatch audit row");

        pool.close().await;
    });
}
```

> **Step-1 note for the implementer:** the audit-action predicate above is a
> placeholder shape. In Step 2 you will read the real dispatch-row action
> emitted for a tool call (grep `core/src/scheduler/tool_dispatch` and the
> `audit_multiset` rows in `cli_ask_e2e.rs`) and replace the `WHERE` clause with
> the exact `(actor, action)` a `mail.search` dispatch writes. Do NOT ship the
> `LIKE '%mail%'` fallback — pin the exact action.

- [ ] **Step 2: Pin the exact audit action, then run on the DGX.**

First determine the real dispatch audit row:
```sh
grep -rn "dispatched\|tool_dispatch\|action" core/src/scheduler/tool_dispatch*.rs core/src/scheduler/tool_dispatch/ 2>/dev/null | grep -i "action\|dispatch" | head
sed -n '564,579p' core/tests/cli_ask_e2e.rs   # audit_multiset shape reference
```
Replace the `WHERE` clause with the exact `(actor, action)`. Then:
```sh
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && cargo test -p kastellan-core --test mail_daemon_e2e daemon_planner_dispatches -- --nocapture 2>&1 | tail -30'
```
Expected: PASS — the daemon plans (scripted), the mail worker dispatches `mail.search` under the sandbox against the mock, the task completes. If the daemon log shows the mock 503'd, the embed/chat queue depths are off (add exactly one `embedding_envelope()` per plan iteration).

- [ ] **Step 3: Commit.**

```sh
git add core/tests/mail_daemon_e2e.rs
git commit -m "test(mail): tier 2a — daemon planner dispatches mail.search end-to-end

Real daemon + real sandboxed mail worker + plain-HTTP mock_localmail + a scripted
planner: asserts mail is advertised in the planner <tools> block and mail.search
actually dispatches to completion. Force-routing off so the direct transport
reaches the plain-HTTP mock.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: `mail_daemon_e2e.rs` — tier 2b (live LLM selects mail.* unprompted, `#[ignore]`)

**Files:**
- Modify: `core/tests/mail_daemon_e2e.rs`

**Interfaces:**
- Consumes: same as Task 6, minus the scripted planner (uses a real local LLM URL from the environment).
- Produces: nothing.

- [ ] **Step 1: Add the `#[ignore]` live tier.**

Append to `core/tests/mail_daemon_e2e.rs`. Same bring-up as tier 2a, but the daemon's LLM points at a real local endpoint (`KASTELLAN_MAIL_LIVE_LLM_URL`, e.g. DGX Ollama `http://127.0.0.1:11434/v1`) and the plan is NOT scripted — the model must choose `mail.*` from a mail-ish prompt:

```rust
/// Live: a real local LLM, given a mail-ish question, must select mail.*
/// unprompted. Portable — the mock origin needs no localmail. Point the daemon
/// at a local OpenAI-compatible endpoint via KASTELLAN_MAIL_LIVE_LLM_URL
/// (e.g. http://127.0.0.1:11434/v1 for Ollama) + _MODEL. Run with --ignored.
#[test]
#[ignore = "needs a real local LLM (KASTELLAN_MAIL_LIVE_LLM_URL); non-deterministic"]
fn live_llm_selects_mail_unprompted() {
    let Ok(llm_url) = std::env::var("KASTELLAN_MAIL_LIVE_LLM_URL") else {
        eprintln!("\n[SKIP] set KASTELLAN_MAIL_LIVE_LLM_URL to a local OpenAI-compatible endpoint\n");
        return;
    };
    // <-- same binary/PG/supervisor skips + mock_localmail + data_dir + mail
    //     extra_env (with KASTELLAN_EGRESS_FORCE_ROUTING=0) as tier 2a, EXCEPT
    //     bring_up_daemon's mock_base_url = the real llm_url (strip a trailing
    //     /v1 if bring_up_daemon appends it — mirror how cli_ask sets
    //     KASTELLAN_LLM_LOCAL_URL), and also set KASTELLAN_MAIL_LIVE_LLM_MODEL
    //     via extra_env as KASTELLAN_LLM_LOCAL_MODEL. -->
    // Submit: cli ask "search my email for the invoice from north coast health".
    // Assert: a mail.* dispatch audit row appears within the plan cap (the exact
    // action pinned in Task 6 Step 2). Do NOT assert on wording — only that the
    // model reached for mail.*.
    let _ = llm_url; // (implementer fills the body per the note above)
}
```

- [ ] **Step 2: Verify it compiles (ignored by default) and, when a local LLM is available, run it.**

```sh
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && cargo build --tests -p kastellan-core 2>&1 | tail -3'
# Manual live run (DGX Ollama):
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && KASTELLAN_MAIL_LIVE_LLM_URL=http://127.0.0.1:11434/v1 KASTELLAN_MAIL_LIVE_LLM_MODEL=<chat-model> cargo test -p kastellan-core --test mail_daemon_e2e live_llm_selects_mail -- --ignored --nocapture 2>&1 | tail -30'
```
Expected: compiles (ignored in CI); the manual run shows the real planner selecting `mail.*`. If the model won't pick mail, that is signal the `tool_docs` summaries need sharpening — note it, do not force the plan.

> **Model-override caveat:** `bring_up_daemon` hard-codes `KASTELLAN_LLM_LOCAL_MODEL = "test-local-model"`. For a live run the real model must win. First verify an `extra_env` `KASTELLAN_LLM_LOCAL_MODEL` entry overrides it (last-write-wins when the supervisor materialises `spec.env` into the process env). If it does NOT override, add an optional `model` parameter to `tests-common::daemon::bring_up_daemon` (defaulting to `"test-local-model"`, so the `cli_memory_l3*` callers are unaffected) rather than duplicating the env key. Resolve this in Step 1 before running.

- [ ] **Step 3: Commit.**

```sh
git add core/tests/mail_daemon_e2e.rs
git commit -m "test(mail): tier 2b — live LLM selects mail.* unprompted (#[ignore])

Opt-in live tier: a real local LLM (KASTELLAN_MAIL_LIVE_LLM_URL) given a mail-ish
question must reach for mail.* on its own, proving the tool docs are good enough
for real model selection. Portable; runs on any host with a local LLM.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: `mail_daemon_e2e.rs` — Mac-only fidelity contract test (`#[ignore]`)

**Files:**
- Modify: `core/tests/mail_daemon_e2e.rs`

**Interfaces:**
- Consumes: `kastellan_tests_common::mock_localmail` (its shapes are the reference); a real localmail endpoint + bearer from the environment.
- Produces: nothing.

- [ ] **Step 1: Add the contract test.**

This is the test that closes the #487 drift failure mode: hit **real** localmail and assert its response *shapes* match what `mock_localmail` serves. Because the dev-Mac localmail is HTTPS self-signed (unreachable by the webpki-only Rust transports), the contract test uses a raw TLS-skipping client via `curl` (already on the Mac) rather than the mail worker's transport — it is checking localmail's shapes, not the worker's TLS path.

Append to `core/tests/mail_daemon_e2e.rs`:

```rust
/// Mac-only fidelity gate: assert real localmail's /v1 response SHAPES still
/// match what tests-common::mock_localmail serves, so the hermetic mock cannot
/// silently drift (the #487 failure mode: mock served `hits`/`text-plain` while
/// reality served `results`/JSON, masking a real decode bug). Uses `curl -k`
/// because the dev-Mac localmail is HTTPS self-signed and the worker's transport
/// is webpki-only (that TLS path is not what this test checks). Run with
/// --ignored on the Mac; skips as-pass without the endpoint + token env.
#[test]
#[ignore = "needs real localmail (KASTELLAN_MAIL_ENDPOINT + KASTELLAN_MAIL_TOKEN); Mac-only"]
fn mock_localmail_shapes_match_real_localmail() {
    let (Ok(endpoint), Ok(token)) = (
        std::env::var("KASTELLAN_MAIL_ENDPOINT"),
        std::env::var("KASTELLAN_MAIL_TOKEN"),
    ) else {
        eprintln!("\n[SKIP] set KASTELLAN_MAIL_ENDPOINT + KASTELLAN_MAIL_TOKEN to the live localmail\n");
        return;
    };

    // Helper: curl -k a path, return (content_type, parsed_json_or_none).
    let get = |method: &str, path: &str| -> (String, Option<serde_json::Value>) {
        let out = Command::new("curl")
            .args(["-sk", "-X", method, "-H", &format!("Authorization: Bearer {token}"),
                   "-H", "Content-Type: application/json", "-D", "-", "-o", "-",
                   &format!("{endpoint}{path}")])
            .arg(if method == "POST" { "--data" } else { "--url-query" })
            .arg(if method == "POST" { "{\"query\":\"invoice\"}" } else { "" })
            .output()
            .expect("curl");
        let text = String::from_utf8_lossy(&out.stdout);
        let ctype = text.lines()
            .find(|l| l.to_lowercase().starts_with("content-type:"))
            .unwrap_or("").to_lowercase();
        let body = text.split("\r\n\r\n").last().unwrap_or("");
        (ctype, serde_json::from_str(body).ok())
    };

    // 1. search → object with a `results` array (NOT `hits`).
    let (_ct, search) = get("POST", "/v1/search");
    let search = search.expect("search returns JSON");
    assert!(search.get("results").map(|r| r.is_array()).unwrap_or(false),
        "real localmail search must key hits under `results`: {search}");
    assert!(search.get("hits").is_none(), "real localmail must NOT use `hits` (the #487 drift)");

    // 2. accounts → JSON array.
    let (_ct, accounts) = get("GET", "/v1/accounts");
    assert!(accounts.expect("accounts JSON").is_array(), "accounts must be an array");

    // 3. attachment text → application/json {"text": …} (NOT text/plain).
    //    Use the first attachment sha from a real message; if the archive has no
    //    attachment, skip this leg with a printed note rather than failing.
    // <-- implementer: fetch a real message id from list, then its attachment
    //     sha, then GET /v1/attachments/{sha}/text and assert content-type is
    //     application/json and the body has a string `text` field. If no
    //     attachment exists in the archive, eprintln a note and return. -->
}
```

- [ ] **Step 2: Compile + (Mac) run against live localmail.**

```sh
source "$HOME/.cargo/env"
cargo build --tests -p kastellan-core 2>&1 | tail -3
# Mac live run (localmail on :8443; mint a token via POST /v1/auth/login):
KASTELLAN_MAIL_ENDPOINT=https://127.0.0.1:8443 KASTELLAN_MAIL_TOKEN=<bearer> \
  cargo test -p kastellan-core --test mail_daemon_e2e mock_localmail_shapes_match_real -- --ignored --nocapture 2>&1 | tail -30
```
Expected: compiles; the Mac live run PASSES, confirming the mock's `results`/JSON shapes still match reality. If it fails, real localmail has drifted — update `mock_localmail` to match AND note the drift (the test did its job).

- [ ] **Step 3: Commit.**

```sh
git add core/tests/mail_daemon_e2e.rs
git commit -m "test(mail): Mac-only fidelity contract — mock shapes vs real localmail

Asserts real localmail's /v1 response shapes (results / application/json {text})
still match tests-common::mock_localmail, closing the #487 drift failure mode
(the mock cannot silently diverge without this test naming the field). curl -k
because dev-Mac localmail is HTTPS self-signed (not the TLS path under test).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 4: Slice-2 acceptance gate.**

```sh
ssh dgx 'source ~/.cargo/env && cd ~/src/kastellan && cargo test --workspace 2>&1 | tail -5 && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3'
source "$HOME/.cargo/env" && cargo test -p kastellan-worker-mail 2>&1 | tail -3   # mac mail-crate sanity
```
Expected: DGX full workspace green (record counts), clippy clean, 0 `[SKIP]` regressions. Record the new baseline for the handover.

---

## Wrap-up (after both slices)

- [ ] Update `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md`: mail-worker live-test legs closed (sandbox 1a/1c, egress coupling 1b/1d, planner 2a + live 2b, fidelity contract); new DGX baseline counts; note the deferred force-routed round-trip (webpki wall) as a filed follow-up.
- [ ] File a GitHub issue for the deferred full force-routed mail round-trip (needs a publicly-trusted-cert localmail).
- [ ] Open the PR (branch `test/mail-worker-live-coverage`) via the finishing-a-development-branch flow; link the spec + this plan.

---

## Self-Review

**Spec coverage:**
- §A1 mock_localmail → Task 2. §A2 scripted_llm lift → Task 1. ✔
- §B tier 1a → Task 3; 1c → Task 4; 1b/1d → Task 5. ✔
- §C tier 2a → Task 6; 2b → Task 7. ✔
- §D contract test → Task 8; host/skip matrix → baked into each test's skip guards + `#[ignore]`s. ✔
- §Risks: Seatbelt-loopback (Task 3 Step 2 runs both hosts; if macOS 1a fails, 1b carries the macOS sandbox leg — noted). Lift destabilising cli_ask (Task 1 Step 4 gate). Mock rot (Task 8). ✔

**Placeholder scan:** Two intentional, clearly-flagged fill-ins remain — the exact audit-action predicate (Task 6 Step 2, with the grep to find it) and the attachment-leg completion in the contract test (Task 8, with precise instructions). Both are gated by a "pin the exact value" step, not vague "add error handling". No other TBDs.

**Type consistency:** `mail_entry(binary, endpoint, token_file) -> ToolEntry` (`.policy`) used consistently (Tasks 3/4/5). `spawn_scripted_llm(Vec<String>, Vec<String>) -> ScriptedLlm` with `.base_url`/`.chat_requests` (Tasks 1/6). `spawn_mock_localmail() -> MockLocalmail` with `.base_url` + the `CANNED_*` consts (Tasks 2/3/4). `NetWorkerSpawn`/`spawn_forced_net_worker`/`EgressAuditRow` match `egress_force_routing_e2e.rs` (Task 5). `apply_workspace_out(&mut SandboxPolicy, &Path)` matches `tool_host/scratch.rs` (Task 4). `bring_up_daemon(label, suffix, data_dir, mock_base_url, user, extra_env)` matches `daemon.rs` (Task 6).
