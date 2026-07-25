# Full force-routed mail round-trip e2e — design (#491)

**Date:** 2026-07-25
**Issue:** [#491](https://github.com/hherb/kastellan/issues/491) — "mail worker: full
force-routed round-trip e2e (needs a trusted-cert localmail)"
**Follow-up from:** PR [#490](https://github.com/hherb/kastellan/pull/490) (mail-worker
live-test coverage). Tier 1b there covers the egress leg only at the coupling/policy
level; a full `mail.*` JSON round-trip through the MITM tunnel was deferred.

---

## Problem

PR #490's tier 1b proves mail's real policy force-routes and that the sidecar enforces
mail's derived allowlist (off-host + wrong-port `403`, `egress.allowed` on the endpoint).
It deliberately does **not** drive a full `mail.search` JSON round-trip through the
tunnel, because that was structurally impossible against a hermetic origin:

- The egress-proxy's MITM **upstream re-origination leg**
  (`workers/egress-proxy/src/pins.rs::build_upstream_client_config`) trusts **webpki
  roots only**. Pins only *strengthen* validation; there is **no extra-root seam**.
- localmail's cert is **self-signed** (a personal loopback service). The proxy's
  re-origination TLS therefore rejects it — the #473 wall
  ([[egress-proxy-upstream-trusts-webpki-only]]).

**Finding that widens the scope beyond "just a test":** this is not only a test gap. A
*deployed* force-routed mail worker cannot reach a self-signed localmail **at all**
today, because the same wall blocks the real re-origination leg. The handover's standing
deployment note ("a co-located loopback localmail needs the force-routed/MITM egress
path …") is therefore **false as written** — the MITM path is blocked by the webpki-only
upstream just as the direct transport is. This design's capability is what makes that
claim true.

## Goal

A **hermetic** e2e that drives a real `mail.search` from the real mail worker, through a
force-routed egress-proxy sidecar in **MITM mode** (mail's production posture), to a
self-signed HTTPS localmail mock, and asserts:

1. the JSON response round-trips (`results` array), and
2. the proxy **MITM-terminated** the connection (`tls_intercepted: true`).

Plus a **negative control** proving the new trust seam is load-bearing (the round-trip
fails without it), and a **live `#[ignore]` DGX tier** driving the same round-trip against
the real localmail now running on the DGX (self-signed cert) — the real-origin complement
to the mock.

## Decisions locked (operator-approved)

1. **Trust approach — add an upstream extra-CA seam** to the egress proxy (not "require a
   real publicly-trusted cert"). Off by default, fail-closed, host-provisioned. This
   makes the e2e hermetic **and** closes the real deployment gap.
2. **Production scope — capability + hermetic test only this PR.** The production
   operator-config wiring (a deployed mail worker reaching self-signed localmail) is
   **deferred to a new follow-up issue** with its own config-shape + gating design.
3. **Hermetic mock tier — always-run + skip-guarded, NOT `#[ignore]`.** #491's `#[ignore]`
   wording assumed a real *external* localmail; the extra-CA seam makes the mock tier
   hermetic, so it runs on every DGX `cargo test --workspace` (DGX-authoritative;
   skip-as-pass on the Mac, where the Seatbelt-loopback question is still open — same
   posture as tiers 1a/1b).
4. **Add a live `#[ignore]` DGX tier** against the real localmail now running on the DGX
   (self-signed cert at `~/.config/localmail/tls/cert.pem`). Env-gated, skip-as-pass when
   the live vars are unset. Validates the extra-CA seam against a **real** self-signed
   cert (not just an `rcgen` mock). Needs **no** production operator-env wiring — the test
   passes `upstream_extra_ca` directly via `NetWorkerSpawn`, exactly like the mock tier —
   so it stays inside the approved scope.

## Threat-model justification for the seam

The egress proxy is a separate, sandboxed worker; its env is set by **core** (trusted),
never by the worker it fronts. A compromised worker cannot set the extra CA. The extra CA
only affects which origin certs the proxy accepts on the re-origination leg **for the
same allowlisted endpoint** the worker was already confined to. Each sidecar is
per-worker and allowlist-scoped, so the blast radius is unchanged: a compromised mail
worker still reaches only mail's one allowlisted endpoint. This is the symmetric
counterpart to the worker-side `ProxyConnectGet::with_extra_ca` that transparent-tunnel
workers already use. No "spawn unsandboxed" escape hatch is introduced.

---

## Component 1 — Upstream extra-CA seam (production code)

Follows the exact rails the `cert_pins_json` / `ENV_PINS` operator config already uses.
**Unset ⇒ byte-identical to today (webpki-only).** Fail-closed: a set-but-unreadable /
unparseable / zero-cert PEM aborts proxy startup (never silently degrades to no-extra-CA).

### `workers/egress-proxy/src/pins.rs`
- `build_upstream_client_config(pins_env: Option<&str>, extra_ca_path: Option<&Path>)`.
- Factor the trust logic so a **pure helper** takes the extra-CA PEM **bytes** and
  augments the `RootCertStore` (webpki **+** extra CA), unit-testable without the
  filesystem. The outer fn does the file read (fail-closed on I/O), then calls the pure
  helper.
- The `PinningVerifier` pin overlay composes unchanged: its inner `WebPkiServerVerifier`
  is built over the **augmented** roots, so pins + extra-CA coexist.
- Reuse the existing fail-closed PEM-loader semantics (mirror
  `web-common::proxy_connect::add_ca_pem`: unreadable/unparseable/zero-cert ⇒ `Err`).

### `workers/egress-proxy/src/main.rs`
- Read `KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA` (an absolute path) and pass it to
  `build_upstream_client_config`.
- Emit a **loud startup WARN to stderr** when an extra CA is loaded (visibility /
  defense-in-depth, mirrors the #388.2 manifest under-lock WARN). Stderr, not the stdout
  decision stream.
- The read happens at startup **before** `lock_down()`, so only the Seatbelt/bwrap
  `fs_read` bind matters for reading it — Landlock is applied after.

### `core/src/egress/spawn.rs`
- `const ENV_UPSTREAM_EXTRA_CA = "KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA"`.
- `proxy_policy(..., upstream_extra_ca: Option<&Path>)` and
  `spawn_sidecar(..., upstream_extra_ca: Option<&Path>)` gain the parameter. When `Some`:
  push the env **and** add the CA path to `proxy_policy.fs_read` (so the sandboxed proxy
  can read it). Omit-when-`None` ⇒ byte-identical (mirrors the pins/disable-MITM
  omit-when-absent pattern).

### `core/src/egress/net_worker.rs`
- `NetWorkerSpawn.upstream_extra_ca: Option<&'a Path>`, threaded into the `spawn_sidecar`
  call inside `spawn_net_worker`.

### All other `NetWorkerSpawn` constructors
- Production `force_route.rs::spawn_worker_maybe_forced` and every existing e2e that
  builds a `NetWorkerSpawn` get `upstream_extra_ca: None` — mechanical, exactly how
  `disable_mitm` / `cert_pins_json` were introduced. **`force_route.rs` stays `None`**
  (no production operator-env wiring this PR).

---

## Component 2 — HTTPS `mock_localmail` + cert reuse (test infra)

### `tests-common/src/mock_localmail.rs`
- Extract the per-request handling into a **shared pure router**
  (`route(method, path, auth_ok, body) → response_bytes`) used by both the existing
  plain-HTTP `spawn_mock_localmail` and a new **`spawn_mock_localmail_tls()`**. No
  canned-response duplication (the #475 de-dup ethos). Response shapes are unchanged, so
  the existing Mac-only fidelity contract test still pins them.
- `spawn_mock_localmail_tls()` wraps the router in a rustls `TlsAcceptor` and returns
  `(MockLocalmail, cert_pem)`. The PEM is written by the caller to a tempfile used as the
  proxy's `upstream_extra_ca`.

### Shared cert-gen helper
- Factor the `rcgen::generate_simple_self_signed(vec!["127.0.0.1"])` **IP-SAN** cert
  generation (currently inline in `tls_origin.rs`) into one reusable helper returning
  `(cert_der, key_der, cert_pem)`. Both `tls_origin::spawn_loopback_tls_origin` and the
  new TLS mock use it. The self-signed leaf **is** its own trust anchor — `tls_origin`'s
  own unit test already proves a client trusting only that PEM completes the handshake.

---

## Component 3 — Round-trip e2e tier + negative control

### `core/tests/mail_e2e.rs` — `force_routed_search_round_trips_through_mitm_sidecar`
Always-run, skip-guarded on: sandbox available + egress-proxy bin + PG bin (the union of
tiers 1a and 1b's guards). Steps:

1. `spawn_mock_localmail_tls()` → HTTPS mock at `https://127.0.0.1:<port>`; write its
   cert PEM to a tempfile `ca_path`.
2. Build mail's real policy via `mail_entry(worker, "https://127.0.0.1:<port>", token)`;
   the derived `Net::Allowlist` entry is `127.0.0.1:<port>`.
3. `spawn_forced_net_worker` with:
   - the **real mail worker binary** as `spec.program` (not `/bin/sleep`),
   - `disable_mitm: false` (mail's production posture — MITM stays on),
   - `upstream_extra_ca: Some(&ca_path)`,
   - a capturing decision sink.
   `rewrite_worker_policy` sets the worker's `proxy_uds` + injects the per-instance MITM
   CA, so the **real worker** drives the request (not the host). The 127.0.0.1 literal is
   dialed via the proxy's allowlisted-IP carve-out (tier 1b already proves the CONNECT
   establishes).
4. `dispatch(&pool, &Vault, &mut worker, "mail", "mail.search", {"query": …})`. Path:
   worker → (trusts per-instance CA) → proxy MITM-terminates → proxy re-originates TLS to
   `127.0.0.1:<port>` → (trusts extra-CA) → mock returns canned `results`.
   Assert `result["results"].is_array()`.
5. Assert the sink captured an `egress.allowed` row whose `payload["tls_intercepted"]`
   is `true` (proves real MITM, not a transparent tunnel — `payload` carries the flag,
   `core/src/egress/audit.rs::decision_to_audit`).
6. 1:1 teardown (worker drop tears the sidecar down), scratch removed.

### Negative control — `force_routed_search_fails_without_upstream_extra_ca`
Identical flow with `upstream_extra_ca: None`. The `mail.search` dispatch must **error**
(the proxy's re-origination TLS rejects the self-signed origin under webpki-only trust).
Proves the seam is load-bearing — the repo's "prove it fails against un-hardened code"
discipline. Kept tight to avoid coupling to the exact error text (assert `is_err`, not a
specific string).

### Live tier — `#[ignore]`, DGX-only, env-gated

`live_force_routed_search_against_real_localmail` — the same MITM round-trip driven
against the **real** localmail on the DGX, using its real self-signed cert as the proxy's
`upstream_extra_ca`. This is the "real origin" complement to the hermetic mock (validates
the seam against a production-shaped cert + real 36k-message archive). `#[ignore]` +
env-gated (skip-as-pass when unset), the established live-verification pattern
(`web_search_e2e::real_search_against_searxng`, `mail_daemon_e2e` tier 2b).

**Gating env (operator-set on the DGX):**
- `KASTELLAN_MAIL_LIVE_ENDPOINT` — the live localmail `/v1` base (e.g.
  `https://127.0.0.1:8443`; **not** the `/mcp/` endpoint — the worker uses `/v1` REST).
- `KASTELLAN_MAIL_LIVE_CA` — path to the real cert PEM
  (`~/.config/localmail/tls/cert.pem`), passed as `upstream_extra_ca`.
- Credentials for the one-time `POST /v1/auth/login {username,password}` → `token`; the
  test writes the returned bearer to a 0600 `KASTELLAN_MAIL_TOKEN_FILE` before spawning.
  (Passed as `KASTELLAN_MAIL_LIVE_USER` / `KASTELLAN_MAIL_LIVE_PASSWORD`, or a
  pre-obtained `KASTELLAN_MAIL_LIVE_TOKEN` — decided in the plan; a pre-obtained token is
  simpler and keeps the password out of the test process.)

**Assertions:** `results` array round-trips; the captured `egress.allowed` decision has
`tls_intercepted: true`. The mail worker's own `/v1/auth/login` is host-side test setup,
not part of the worker's tool surface.

**Open details to resolve on the DGX during implementation (flagged, not blocking the
design):**
- **SSRF vs the endpoint IP.** The proxy SSRF-blocks private ranges; the allowlisted-IP
  carve-out is documented for operator-allowlisted literals. `10.0.0.3` is RFC1918. Since
  the worker runs *on* the DGX, prefer **`127.0.0.1:8443`** (loopback, already proven
  dialable by tier 1b's carve-out). Verify the private-IP path only if a non-loopback
  address is required.
- **Cert SANs.** The proxy validates the re-origination server-name against the cert. The
  endpoint host (`127.0.0.1` vs a DNS name) must match a SAN in
  `~/.config/localmail/tls/cert.pem`. Inspect the cert's SANs on the DGX and choose the
  endpoint host to match.
- **Token freshness.** localmail tokens may expire; the test obtains/refreshes one at
  setup rather than pinning a stale token.

---

## Testing / TDD

Tests-first, each proven to fail before the code exists:
- **Pure unit tests** (`workers/egress-proxy/src/pins/tests.rs`): a valid extra-CA PEM
  adds a trust anchor; an invalid / empty / unreadable path errors;
  `build_upstream_client_config(None, None)` stays byte-identical (existing tests still
  pass).
- **`proxy_policy` unit tests** (`core/src/egress/spawn.rs`): env set when `Some` / omitted
  when `None`; the CA path lands in `fs_read` when `Some`.
- **e2e** (`mail_e2e.rs`): the positive round-trip (`tls_intercepted: true`) + the
  negative control.

Verification: **DGX** `cargo test --workspace` + `clippy --workspace --all-targets -D
warnings`, **0 `[SKIP]`**; macOS per-crate for the Mac-testable parts; cross-clippy the
`cfg`-touched bits. Then run the **live tier** on the DGX with the gating env set
(`cargo test -p kastellan-core --test mail_e2e -- --ignored --nocapture`) and confirm the
real round-trip + `tls_intercepted: true`. Expected always-run test-count delta: the new
pure/`proxy_policy` unit tests + 2 always-run e2e (positive + negative); the live tier is
`#[ignore]` (+1 ignored).

## Out of scope / follow-ups

- **Deferred → new issue:** production operator-config wiring in
  `ForceRoutingConfig::from_env` (a host-keyed `{origin-host: ca-path}` map, mirroring how
  cert-pins are host-selected via `pins_for`), so a *deployed* force-routed mail worker
  reaches self-signed localmail. Filed referencing #491.
- **Docs (rule 7):** fold the `#490`-merged HANDOVER/ROADMAP reconciliation into this
  branch's session-end update.
- **Institutional-fact correction:** the handover's + `[[mail-worker-localmail-verification]]`
  memory note's "force-routed/MITM path reaches self-signed localmail" claim is false
  *today*; correct the wording (this PR's capability + the deferred wiring is what makes
  it true).

## Files touched (summary)

| File | Change |
|---|---|
| `workers/egress-proxy/src/pins.rs` | extra-CA param + pure augment helper |
| `workers/egress-proxy/src/pins/tests.rs` | extra-CA unit tests |
| `workers/egress-proxy/src/main.rs` | read env, startup WARN |
| `core/src/egress/spawn.rs` | `ENV_UPSTREAM_EXTRA_CA`, param, fs_read, env, unit tests |
| `core/src/egress/net_worker.rs` | `NetWorkerSpawn.upstream_extra_ca`, thread through |
| `core/src/worker_lifecycle/force_route.rs` | `upstream_extra_ca: None` (no prod wiring) |
| existing e2e files building `NetWorkerSpawn` | `upstream_extra_ca: None` (mechanical) |
| `tests-common/src/mock_localmail.rs` | shared router + `spawn_mock_localmail_tls` |
| `tests-common/src/tls_origin.rs` | factor shared cert-gen helper |
| `core/tests/mail_e2e.rs` | hermetic round-trip tier + negative control + live `#[ignore]` DGX tier |
