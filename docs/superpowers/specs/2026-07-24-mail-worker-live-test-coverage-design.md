# Mail-worker live-test coverage — design

**Date:** 2026-07-24
**Status:** approved (brainstorm), pre-plan
**Issue:** mail-worker live-test gaps (follow-up to #483 / #487; see [[mail-worker-localmail-verification]])

## Problem

`kastellan-worker-mail` (#483) was first live-verified in #487, but only over
**plain JSON-RPC stdio** against a real localmail archive — the worker binary
driven directly, no sandbox, no egress proxy, no daemon. Three legs of the
production path have **no test coverage at all** (there is no `core/tests/mail*`
file; `core/src/workers/mail.rs` has unit tests only):

1. **OS-sandbox leg** — the worker running under the real platform jail
   (macOS Seatbelt / Linux bwrap), reaching its endpoint and delivering an
   attachment through the jail's `fs_write` boundary.
2. **Egress-proxy leg** — the worker force-routed through a real egress-proxy
   sidecar (the production `KASTELLAN_EGRESS_FORCE_ROUTING` path), reaching the
   allowlisted endpoint via proxy-CONNECT with zero direct route.
3. **Daemon/planner leg** — an LLM, running through the real daemon, actually
   selecting and using a `mail.*` tool: registration → `<tools>` advertisement
   → dispatch → result-to-completion.

### Known constraint (must not be re-litigated)

The mail worker's **direct** transport (`web-common::make_get → ReqwestGet`,
used when NOT force-routed) trusts **webpki roots only** with no CA knob, so it
cannot TLS to a self-signed localmail. The MITM egress upstream is *also*
webpki-only ([[egress-proxy-upstream-trusts-webpki-only]]). Therefore a
self-signed HTTPS test origin is impossible in both transports. **This design
uses a plain-HTTP loopback mock origin**, which sidesteps the gotcha entirely
(it only bites TLS) and lets both transports round-trip hermetically.

## Goals / non-goals

**Goals**
- Close all three legs with automated tests that run in CI where hermetic.
- Prove the planner both *can* (scripted, deterministic) and *does* (live,
  opt-in) choose `mail.*`.
- Guarantee the hermetic mock cannot silently drift from real localmail — the
  exact failure mode #487 hit (mock served `hits`/`text-plain`, reality serves
  `results`/`application/json {"text":…}`, masking a real decode bug).
- Share test infrastructure rather than copy it (the #475 duplication lesson).

**Non-goals**
- No production-code change to the mail worker or its manifest. This is
  test-and-test-infra work. (If a leg surfaces a *real* worker bug — as #487
  did — that is fixed under this umbrella, but none is anticipated.)
- No new sandbox/egress capability. All three legs drive **existing production
  couplings** (`spawn_worker`, `spawn_forced_net_worker`, `bring_up_daemon`).
- Not re-verifying the plain-stdio path #487 already covered.

## Endpoint contract (what the mock must serve)

The six tools hit exactly these localmail `/v1` endpoints (from
`workers/mail/src/{client,handler}.rs`):

| Tool | Request | Response shape |
|---|---|---|
| `mail.search` | `POST /v1/search` | `{"results":[…], "next_cursor"?}` |
| `mail.get_message` | `GET /v1/messages/{id}?full_headers={bool}` | message object (headers, body, `attachments:[{filename,sha256,content_type,size}]`) |
| `mail.list_messages` | `GET /v1/messages?{account_ids,folder_ids,limit,cursor}` | `{"results":[…], "next_cursor"?}` |
| `mail.list_accounts` | `GET /v1/accounts` | `[{"id":…}]` |
| `mail.get_attachment_text` | `GET /v1/attachments/{sha}/text` | `application/json {"text":…}` |
| `mail.get_attachment` | `GET /v1/attachments/{sha}` | raw bytes + `Content-Type` |

Auth: `Authorization: Bearer <token>` on every request (from the `0600` token
file bound into the jail). The mock records/accepts the bearer; it need not
enforce a specific value, but MUST assert a non-empty bearer is present so the
auth wiring is covered.

## Architecture

### Section A — Shared test infrastructure (`kastellan-tests-common`)

Two new modules, built first; both slices depend on them.

**A1. `mock_localmail`** — a canned-response HTTP listener.
- Binds an ephemeral **plain-HTTP** loopback port; returns its `base_url`.
- Serves the six `/v1` endpoints above in localmail's **real** shapes (as #487
  corrected them). Bodies are small committed constants / builders, not
  captured private mail.
- Asserts a non-empty `Authorization: Bearer` header on each request.
- RAII: aborts its listener task on drop (mirrors `daemon::MockLlm`).
- Deliberately **not** `cfg`-gated to one OS — it is a portable TCP listener.

**A2. `scripted_llm`** — lift the reusable multishot-LLM primitives out of
`core/tests/cli_ask_e2e.rs` into a shared module:
- `spawn_url_routed_mock` (queued multi-shot HTTP listener routing embed vs.
  chat by request path), `plan_json`, `envelope_for`, `embedding_envelope`.
- **Re-point `cli_ask_e2e.rs` at the shared module.** Its continued green is the
  safety net proving the lift is behaviour-preserving (same pattern as the
  `daemon.rs` lift and the #475 microvm-helper lift). Any genuinely ask-specific
  helpers stay in `cli_ask_e2e.rs`.

### Section B — Slice 1: sandbox + egress legs → `core/tests/mail_e2e.rs`

New file, `#![cfg(any(target_os = "linux", target_os = "macos"))]`, skip-as-pass
posture (`skip_if_no_supervisor` / `skip_if_sandbox_unavailable` /
`pg_bin_dir_or_skip` / worker-binary-exists). Template: `web_fetch_e2e.rs` +
`egress_force_routing_e2e.rs`. All tiers use `mock_localmail` and a temp `0600`
token file. Always-on (no `#[ignore]`):

- **1a — direct under the real jail.** `mail_entry(binary, mock.base_url,
  token_file).policy` → `spawn_worker(backend, spec)` →
  `dispatch(pool, vault, worker, "mail", "mail.search", {query})` → assert
  `result["results"]` is present. Proves the worker runs under real
  Seatbelt/bwrap and the **direct** transport reaches the allowlisted loopback
  origin.
- **1b — force-routed through a real sidecar (the egress leg).**
  `spawn_forced_net_worker` (production coupling; requires the egress-proxy
  binary — skip if absent) brings up a per-worker sidecar, force-routes the mail
  worker onto it, and the same `mail.search` round-trips via proxy-CONNECT.
  Mirrors `egress_force_routing_e2e`'s cross-platform "allowlisted loopback
  round-trips through the coupling's sidecar". Assert the decision reaches the
  ingest sink and teardown is 1:1.
- **1c — attachment delivery under the jail.** Apply the production Phase-A path
  `tool_host::apply_workspace_out(&mut policy, out_dir)` (pushes `fs_write` +
  `KASTELLAN_WORKER_OUT`); `dispatch(mail.get_attachment, {sha256})` → assert the
  returned path is under `out_dir` and the file exists with the mock's bytes.
  Exercises the durable-out-dir + `fs_write` Landlock/Seatbelt-write leg unique
  to mail. Mail is `SingleUse`, satisfying `apply_workspace_out`'s documented
  constraint.
- **1d — allowlist scoping.** Assert `mail_entry(...).policy.net` is exactly
  `Net::Allowlist([endpoint host:port])` (defence-in-depth; mail has no
  LLM-supplied-URL surface, so no live "off-allowlist" egress attempt is
  meaningful — this is a policy-shape assertion, cheap and hermetic).

### Section C — Slice 2: planner leg → `core/tests/mail_daemon_e2e.rs`

New file. Real daemon (`tests-common::bring_up_daemon`) + real sandboxed mail
worker + `mock_localmail` origin, registered via the daemon's env
(`KASTELLAN_MAIL_ENDPOINT` = mock base_url, `KASTELLAN_MAIL_TOKEN_FILE`,
`KASTELLAN_MAIL_BIN`). Two tiers:

- **2a — scripted, always-on.** `scripted_llm` returns a plan whose step calls
  `mail.search`. Assert, via the audit multiset (the `cli_ask_e2e` pattern):
  the mail tool is advertised in the planner `<tools>` block, ≥1
  `agent/plan.formulate` row, the `mail.search` dispatch row, and a successful
  `scheduler/plan.outcome`/task completion. Deterministic, CI-safe.
  **Force-routing note:** the supervised deployment turns
  `KASTELLAN_EGRESS_FORCE_ROUTING` on by default (`core_service_spec`), so the
  daemon-spawned mail worker is force-routed through a sidecar. A force-routed
  worker reaching a plain-HTTP loopback origin through the proxy is exactly what
  `egress_force_routing_e2e` tier (a) already proves, so this works when the
  egress-proxy binary is present; if it is absent, the tier sets
  `KASTELLAN_EGRESS_FORCE_ROUTING=0` in `extra_env` to take the direct path (the
  plan picks one and states it).
- **2b — live LLM, `#[ignore]`.** Real local LLM (DGX Ollama / Mac MLX) given a
  mail-ish question must select `mail.*` unprompted. Portable — the mock origin
  needs no localmail, so this runs on either host that has a local LLM. Asserts
  a `mail.*` dispatch row appears without the plan being scripted.

### Section D — Fidelity contract + host/skip matrix

- **Contract test (`#[ignore]`, Mac-only).** Hits **real** localmail `/v1`
  (`https://127.0.0.1:8443`, operator-provided bearer) and asserts each live
  response's **shape** (JSON keys / content-types) matches what `mock_localmail`
  serves for the same endpoint. This is what closes the #487 drift failure mode:
  the mock cannot diverge from reality without this test naming the field. Skips
  as-pass when localmail / the token are absent (i.e. everywhere but the dev
  Mac).
- **Host/skip matrix (verified 2026-07-24):**
  - Mac: live localmail on `127.0.0.1:8443` → the contract test is Mac-only.
  - DGX: no localmail (skip the contract test) but has Ollama → tier 2b runs.
  - All hermetic tiers (1a–1d, 2a) run on both hosts; the DGX
    `cargo test --workspace` is the Linux acceptance gate.

## Slicing

- **Slice 1 = Section A + Section B** (tiers 1a–1d) — one shippable PR: the shared
  infra plus the sandbox/egress legs. `mock_localmail` is needed here; the
  `scripted_llm` lift can land with Slice 1 (low risk, re-points `cli_ask_e2e`)
  or defer to Slice 2 — recommend landing it in Slice 1 so the lift's safety-net
  test (`cli_ask_e2e`) is exercised early and Slice 2 is pure additive.
- **Slice 2 = Section C + Section D** — the planner tiers + the fidelity
  contract test. May run into a follow-up session.

## Risks

- **macOS Seatbelt loopback under `Net::Allowlist` (tier 1a).** Unverified
  whether Seatbelt `Net::Allowlist(["127.0.0.1:port"])` permits the loopback
  origin (cf. the browser-driver `localhost:*` widening, #286). Verify during
  implementation: if it blocks, tier 1a becomes Linux-only and tier **1b**
  (force-routed, already proven cross-platform in `egress_force_routing_e2e`)
  carries the macOS sandbox leg. No design change either way.
- **`scripted_llm` lift destabilising `cli_ask_e2e`.** Mitigated by re-pointing
  that test at the shared module and keeping it green (the whole point of the
  lift is a shared, verified primitive). Lift only the genuinely-reusable
  helpers; leave ask-specific ones in place.
- **Mock fidelity rot.** Mitigated by the Section-D contract test. Document in
  `mock_localmail` that its shapes are pinned by that test.

## Testing summary

| Tier | Kind | Runs | Proves |
|---|---|---|---|
| 1a | hermetic | all hosts* | worker under real jail + direct transport reaches endpoint |
| 1b | hermetic | all hosts | force-routed through real egress sidecar (egress leg) |
| 1c | hermetic | all hosts | attachment delivery through the jail `fs_write` boundary |
| 1d | hermetic | all hosts | allowlist derived to exactly the one endpoint |
| 2a | hermetic | all hosts | daemon registers + advertises + dispatches `mail.*`, result completes |
| 2b | `#[ignore]` | host w/ local LLM | a real model chooses `mail.*` unprompted |
| contract | `#[ignore]` | Mac (live localmail) | mock shapes still match real localmail |

\* tier 1a macOS pending the Seatbelt-loopback risk above.

## Out of scope / follow-ups

- Vault-backed token materialization for mail (already a documented follow-up in
  `core/src/workers/mail.rs`; unrelated to these legs).
- A real-archive-through-the-jail `#[ignore]` tier (drive tiers 1a/2a against
  real localmail instead of the mock) — nice-to-have, largely covered by #487's
  stdio verification plus the Section-D contract test; add later if wanted.
