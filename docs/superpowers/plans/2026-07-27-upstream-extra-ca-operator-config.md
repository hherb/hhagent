# Plan — upstream extra-CA operator config (#492)

Design: [`../specs/2026-07-27-upstream-extra-ca-operator-config-design.md`](../specs/2026-07-27-upstream-extra-ca-operator-config-design.md).
Branch `feat/492-upstream-extra-ca-wiring`. All slices done.

| # | Slice | Outcome |
| - | ----- | ------- |
| 0 | Docs reconcile — HANDOVER/ROADMAP still said PR #493 was open | `82fa33a9`. HANDOVER 509 → 447 lines, 176 KB → ~110 KB; pre-prune snapshot archived. |
| 1 | Pure `core/src/egress/upstream_ca.rs` (parse / select / PEM probe) + tests | 20 unit tests. Sibling of `cert_pins.rs`; reuses its `host_of_endpoint`. |
| 2 | Enforce the trust-scope rule | `MixedAllowlist` / `MultipleKeyedHosts` at selection; `NotPrivateOrigin` at *parse* (a construction invariant of `UpstreamCaMap`), via `kastellan-net-classify::is_denied_range` (core's first use of that crate). |
| 3 | Wire `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA` into `from_env` + startup PEM read + trust-widening WARN | Fail-closed on unreadable / certificate-less. `with_upstream_cas` builder rather than a 5th positional arg to `new` (leaves the 5 e2e call sites untouched). |
| 4 | Select per worker in `spawn_worker_maybe_forced`; a selection error refuses the spawn | Error names the env var + the worker, and fires before the backend is touched. |
| 5 | `kastellan.env` operator docs (`render_upstream_ca_help`) | Both traps documented; a test pins that every line stays commented. |
| 6 | Prove the security assertions | All six fail against deliberately weakened code (trust-scope + private-origin rules deleted), then restored. |
| 7 | Review pass (self-review of this PR) | Six findings, all fixed in-branch — see below. |

## Review findings, and what changed

| # | Finding | Fix |
| - | ------- | --- |
| 1 | Keying is per-host, so co-located services on one private address share the anchor — the "single origin" claim didn't hold at service granularity | Documented as a known limitation in module docs / startup WARN / `kastellan.env`, and **pinned by a test** so it can't silently change shape. Per-`host:port` keying rejected (diverges from the cert-pins shape, fights the bare-host all-port grant). §4a. |
| 2 | Key shape validated only at selection, so a port-carrying key, hostname key, untrimmed key or non-canonical IPv6 spelling was silent or late dead config | `NotPrivateOrigin` moved to `parse_upstream_cas`; keys trimmed + stored canonically, allowlist hosts canonicalized the same way. `UpstreamCaSelectError::NotPrivateOrigin` deleted — unreachable by construction. |
| 3 | `is_denied_range` is a deny list, not a "private" list (spans multicast / broadcast / class-E), while the doc claimed "the operator's own network" | Wording corrected in code + spec; predicate deliberately left shared to prevent drift. |
| 4 | The `disable_mitm` + anchor refusal named neither the env var nor the worker, reading like an internal wiring bug | Refused in `spawn_worker_maybe_forced`, where both are in scope; `spawn::check_upstream_extra_ca` stays as backstop. |
| 5 | No test proved a *selected* anchor actually reaches the sidecar — only the refusal paths were covered | `a_selected_extra_ca_reaches_the_sidecar_policy_env_and_fs_read`: captures the sidecar's `SandboxPolicy` and asserts both the proxy env key and the jail `fs_read` bind. |
| 6 | Smaller: identical `Display` prefixes on two `ForceRoutingError` variants; startup WARN overstated ("trust WIDENED" before any sidecar exists); `push_str` chain; undocumented END-marker gap | All fixed / documented. |

## Verification

* **Mac (pre-review):** `cargo test -p kastellan-core --lib` **1303 / 0 / 1**;
  `cargo clippy -p kastellan-core --lib --tests -- -D warnings` clean.
* **Mac (post-review fixes):** `cargo test -p kastellan-core --lib` **1308 / 0 /
  1**, exit 0 — +5 = exactly the net new tests; clippy `-D warnings` clean, exit 0.
* **#479 house rule, re-run for the new assertions.** All five fail against
  deliberately weakened code, then pass restored: the three parse-time origin
  rules (weakened by falling back to a lowercased key instead of refusing),
  `a_selected_extra_ca_reaches_the_sidecar_policy_env_and_fs_read` (weakened by
  hardcoding `upstream_extra_ca: None` in the params — the failure output shows
  the sidecar env genuinely lacking the key, so the assertion is not vacuous),
  and `spawn_refuses_an_extra_ca_for_a_transparent_tunnel_worker` (weakened by
  disabling the refusal — whose failure message, `io: egress sidecar: spawn
  egress-proxy sidecar: backend error`, is itself the evidence for finding 4:
  the backstop's wording names neither the env var nor the worker).
* **DGX (authoritative):** full-workspace `cargo test` + `clippy --all-targets
  -D warnings` — see HANDOVER for the recorded figure.

## Follow-ups not taken here

* Per-host (SNI-selected) upstream root sets — the most correct scoping, most
  work; §4's single-private-origin rule reaches the same safety property today.
* A live DGX tier through `from_env` against the real localmail. **Not blocked:**
  the deployed cert was regenerated `CA:FALSE` on 2026-07-26 (the #491 live tier
  passes against it), so this is config-and-run — set
  `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA={"10.0.0.3":"…/cert.pem"}`, restart the
  daemon, dispatch a `mail.search`. Kept out of this PR to hold it to the config
  layer.
