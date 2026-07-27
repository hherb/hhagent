# Plan — upstream extra-CA operator config (#492)

Design: [`../specs/2026-07-27-upstream-extra-ca-operator-config-design.md`](../specs/2026-07-27-upstream-extra-ca-operator-config-design.md).
Branch `feat/492-upstream-extra-ca-wiring`. All slices done.

| # | Slice | Outcome |
| - | ----- | ------- |
| 0 | Docs reconcile — HANDOVER/ROADMAP still said PR #493 was open | `82fa33a9`. HANDOVER 509 → 447 lines, 176 KB → ~110 KB; pre-prune snapshot archived. |
| 1 | Pure `core/src/egress/upstream_ca.rs` (parse / select / PEM probe) + tests | 20 unit tests. Sibling of `cert_pins.rs`; reuses its `host_of_endpoint`. |
| 2 | Enforce the trust-scope rule in selection | `MixedAllowlist` / `MultipleKeyedHosts` / `NotPrivateOrigin`, via `kastellan-net-classify::is_denied_range` (core's first use of that crate). |
| 3 | Wire `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA` into `from_env` + startup PEM read + trust-widening WARN | Fail-closed on unreadable / certificate-less. `with_upstream_cas` builder rather than a 5th positional arg to `new` (leaves the 5 e2e call sites untouched). |
| 4 | Select per worker in `spawn_worker_maybe_forced`; a selection error refuses the spawn | Error names the env var + the worker, and fires before the backend is touched. |
| 5 | `kastellan.env` operator docs (`render_upstream_ca_help`) | Both traps documented; a test pins that every line stays commented. |
| 6 | Prove the security assertions | All six fail against deliberately weakened code (trust-scope + private-origin rules deleted), then restored. |

## Verification

* **Mac:** `cargo test -p kastellan-core --lib` **1303 / 0 / 1**;
  `cargo clippy -p kastellan-core --lib --tests -- -D warnings` clean.
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
