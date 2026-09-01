# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260826_624_pre-prune.md`](archive/handover_20260826_624_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here,
> including the full #619, #615/#616/#618 and live-bring-up write-ups compressed below.

**Last updated:** 2026-09-01 · **`main` HEAD:** `44e0f38d` — [#637](https://github.com/hherb/kastellan/pull/637)
squash-merged (#626, the saturating-first-sample defect: the probe's total budget is now twice one
sample's, so a cold model no longer ends the probe at one measurement), on top of `beb67062`
([#636](https://github.com/hherb/kastellan/pull/636), the #635 DGX-gate write-up), `d3f8ed3f`
([#635](https://github.com/hherb/kastellan/pull/635), the #633 fix) and `8040ca83`
([#631](https://github.com/hherb/kastellan/pull/631), #627). ·
**OPEN BRANCH: `fix/632-fastest-tok-per-s-rename`** — two commits, two independent issues:
[#632](https://github.com/hherb/kastellan/issues/632) (the `tok_per_s` → `fastest_tok_per_s` rename,
reporting vocabulary deliberately frozen) and
[#634](https://github.com/hherb/kastellan/issues/634) (the three hand-rolled `bring_up_daemon`
copies migrated onto `tests-common`, +11 CI-visible unit tests). ·
**Last gate: DGX at `06ea613d` (the branch tip, #632 + #634) — 3921 / 0 / 55, 176 suites,
`TEST_EXIT=0`; see [Test baseline](#test-baseline-authoritative).** Reconciles exactly: **3910 + 11**,
the eleven new `tests-common` unit tests, each grepped out of the log as `ok` rather than subtracted.
8 `[SKIP]`, all gliner-relex — *not* the bwrap-userns skip, so containment really ran. Both hosts on
rustc **1.98.0** (CI parity, re-checked this session on both). **#632 was gated separately first**
(`6d61f4e8`, 3910 / 0 / 55 — byte-identical to the `8d92c02b` baseline, which is what a correct pure
rename looks like), so the two issues' evidence is separable rather than pooled.

> ⚠️ **The Mac cannot currently run ANY daemon e2e, and it is a macOS host condition — not this
> branch, and not kastellan.** `guard_boot_row_e2e` failed with **both** daemon logs empty; so does
> `cli_ask_e2e`, which was green on this Mac two commits ago. Run directly, a freshly-linked
> `kastellan` **hangs in `_dyld_start`** — `sample` shows one frame and a ~112 KB footprint, so the
> process never reaches `main`. **It is newness, not size**, and the correction matters because the
> first hour of evidence pointed the other way: a ~10 MB test binary and the older 2026-08-24
> `kastellan-cli` both ran fine against the 40 MB daemon that hung, which looked like a size
> threshold — until **13 KB `build-script-build` binaries started hanging too** and wedged a cold
> `cargo clippy` indefinitely. A previously-assessed binary keeps working, so the host degrades
> gradually and an early "it worked" only means "that one had already been assessed". Nothing in the
> repo can fix this and no code change should be attempted for it.
> [[mac-fresh-large-binaries-hang-in-dyld]]
>
> **What this means for the NEXT branch, not just the last one:** #633 was merged with its two new
> e2e legs compiled but never executed, and only the DGX run the following day closed that. Plan for
> the same shape — **the DGX is the only host that can gate behaviour right now**, and the Mac is
> load-bearing solely for what the DGX structurally cannot compile (`cfg(target_os = "macos")` arms,
> reachable via `cargo check`/`clippy --all-targets` in a **warm** private target dir; a cold one is
> what wedges). Do not describe a Mac lint-only leg as "the Mac is green".

---

## Current state

### #632 + #634 — DONE on `fix/632-fastest-tok-per-s-rename`

Two independent issues, one branch, two commits — the rename is a `core` change and the harness
migration is a `tests-common` one, so they share no file.

**#632 — `tok_per_s` → `fastest_tok_per_s`, in `BootRates` *and* `TimeoutBasis::Probed`.** Both
moved together because renaming one alone leaves `fastest_tok_per_s: Some(*tok_per_s)` in
`from_basis` — reads like a bug, invites a later session to "restore" the old name.

- **The REPORTING vocabulary is deliberately frozen at `tok_per_s`, and this is the one judgement
  call the issue left open.** The durable `guard_tier.boot` key cannot move (live rows carry it;
  the operator query `slowest_tok_per_s < tok_per_s / 2` is written against it) — that much the
  issue states. What it did not settle is `main.rs`'s **two tracing field names**, which it counted
  among the rename sites. They stay: a `warn!` line naming this number differently from the audit
  row it accompanies reads as a *second measurement*, and the module's own contract is that the
  `info!`, the `warn!` and the row report the same facts. Both reporting sites now carry a comment
  saying so, and the divergence is visible in the code itself as
  `"tok_per_s": rates.fastest_tok_per_s`. **Cheap to overrule** — it is four lines.
- **The issue's site count was low and a blind `sed` would have broken production.** 62 raw
  occurrences across 12 files; only ~40 are Rust identifiers. The rest are wire keys, the operator
  query, the two tracing fields, a pseudocode symbol in `derive_guard_timeout`'s doc, and a local
  `tok_per_s` holding ONE sample's rate (left alone — `fastest` is the f32 that reaches the basis).
  `\btok_per_s\b` does not match inside `slowest_tok_per_s` (`_` is a word character) but **does**
  match `"tok_per_s"`, so the naive regex renames the durable key. The existing `CONFIGURED_KEYS`
  array is what makes that a test failure rather than a silent one — no new test was needed for the
  hazard, which is why this issue is cheap.

**#634 — the three hand-rolled `bring_up_daemon` copies now use the shared helper.**
`cli_ask_e2e`, `observation_capture` and `guard_boot_row_e2e`, ~70 identical lines each.

- **The parameters became a `DaemonSpec` builder** rather than a seventh, eighth and ninth
  positional argument. Three of the existing six were already adjacent `&str`s — the same
  transposition hazard #632 is about, one crate over.
- **Two divergences the issue's own table missed**, both found by reading the copies rather than
  the issue [[issue-as-filed-can-carry-a-regression]]:
  - `observation_capture` uses a **15 s** readiness budget. The issue documented only
    guard_boot_row's 20 against the shared 10, so the real spread is **three** values.
  - `observation_capture` passes `KASTELLAN_LLM_LOCAL_URL` **verbatim**. It is operator-supplied
    and documented as already carrying `/v1` (`http://127.0.0.1:8000/v1`), so the shared helper's
    unconditional append would have dialled `/v1/v1`. **This is the one migration hazard that
    fails silently** — that test drives a real LLM, so it would have surfaced as an unreachable
    backend naming nothing. Now unrepresentable: `LlmEndpoint::{Base, Verbatim}` are distinct types
    at every call site, and `mail_daemon_e2e`'s `strip_suffix("/v1")` workaround was deleted with
    it.
- **11 new `tests-common` unit tests, 15 mutants, all killed** — each by the test written for it.
  They matter out of proportion to their size: `linux-check.yml` runs
  `cargo test -p kastellan-tests-common` on **every PR and nothing else**, while the six daemon
  e2es these values configure run on no PR at all. Worth keeping from the mutation round:
  - **A deletion mutant is weaker than a transposition one.** Deleting the `extra_env` extend
    killed two tests; *moving it before the common keys* — the actual defect shape — killed exactly
    `extra_env_wins_over_a_default_it_names`. Only the second proves the ordering test.
  - **`the_defaults_…` pins LITERALS, not the constants.** Asserting `Some(DEFAULT_LLM_MODEL)` puts
    the constant on both sides and passes at any value; caught in my own first draft, and it is the
    #633 lesson repeating [[audit-sink-doubles-hide-storage-transforms]].
- **The `extra_env`-later-wins guarantee is now a property, not a comment.** `mail_daemon_e2e`
  depended on it with nothing testing it. Verified against what the backends actually render —
  systemd one `Environment=` line per entry, launchd duplicate plist dict keys, both
  order-preserving and last-wins — rather than carried from the comment
  [[handover-claims-verify-before-carrying]].
- **Also folded in:** the character-for-character `guard_tier_boot_payload` duplicate (now
  `tests_common::guard_tier_boot_payload`), and **#635's stderr-on-failure fix that `cli_ask_e2e`'s
  private copy had never received** — both its waits used a bare `.expect`, so a daemon dying
  before `main` reported only the last polled status. That is the drift #634 predicted, found
  in the act.
- **File sizes:** `guard_boot_row_e2e` 687 → **537**, `cli_ask_e2e` 858 → **736**,
  `observation_capture` 664 → **601**. New files all under cap: `daemon.rs` 292,
  `daemon/spec.rs` 283, `daemon/spec/tests.rs` 268.
- **Still open from #634's own text:** nothing. `scripted_llm.rs` (514) and
  `boot_report/tests.rs` (650) remain over cap and are untouched by this branch.

### #626 — a saturating FIRST probe sample ended the probe at one — MERGED `44e0f38d` ([#637](https://github.com/hherb/kastellan/pull/637))

Full prose in [`archive/handover_20260901_632_634_pre-prune.md`](archive/handover_20260901_632_634_pre-prune.md)
and in the ROADMAP's guard block. Kept here only for what still binds:

- **`PROBE_TOTAL_BUDGET_MS` equalled `PROBE_BUDGET_MS`**, so after any saturating sample
  `elapsed_ms >= PROBE_TOTAL_BUDGET_MS` and the probe stopped at one measurement wherever it fell.
  The fix is one constant: `2 * PROBE_BUDGET_MS`. **Nothing special-cases saturation and nothing
  should** — the rule is still elapsed wall clock, and `summarise`'s ranking (`Measured` outranks
  `Saturated`) is what turns the retry into a correct budget. The added wall clock never lands on a
  host that ends up healthy.
- **The budget relation is a COMPILE-time assertion beside the constant**, `>= 2 *` paired with
  `<= 2 *`. It was `>` and it was inside `#[cfg(test)] mod tests`, and both were wrong:
  `PROBE_BUDGET_MS + 1` passed the `>` guard and every test while still refusing the second sample
  in production, and a `cfg(test)` `const _` is stripped from `cargo build --release`
  [[cfg-test-const-assert-is-not-a-release-guard]]. **When a fix moves a threshold, the test must
  ask at a value the PRODUCER can emit, not at the threshold itself.**
- **#626 as filed quotes the wrong finding** (`Clamped::ToCeiling`'s sentence; this path yields
  `best = Saturated`), and its option 3 was rejected on more than cost — raising the total makes
  that option's trigger *unreachable* [[unreachable-success-path-proves-nothing]].
- **`TimeoutBasis::Saturated` does NOT mean every sample stalled.** `summarise` returns it whenever
  **no** sample measured and **at least one** saturated, so `[Saturated, Failed, Failed]` reaches
  the row as `attempted_samples: 3` off one stall. A row saying `attempted_samples: 1` is a
  **pre-#626 row, or a bug**.
- **`scripts/upgrade_from_git.sh`'s `CHANNEL_WAIT` is 120**, not 45: the probe *and* the fatal
  `/props` call both run before channel supervision, so the pre-scheduler bound is ~80 s and the
  script would otherwise `exit 1` blaming **Matrix** on a host whose real problem is the guard
  endpoint.
- **Six-plus-five mutants, all killed.** Two review rounds; the first review's own finding was a
  mutant never tried [[mutation-proof-counts-only-mutants-you-tried]].
- **[#639](https://github.com/hherb/kastellan/issues/639) filed from it:** `guard_tier_e2e.rs` is
  1558 lines and mixes hermetic probe cases with PG-dependent door cases — splitting them is
  [#622](https://github.com/hherb/kastellan/issues/622)'s cheapest option, since the probe half
  would then fit a CI gate with no Postgres service container.

### #633 — the CONFIGURED boot row had no seam pin — MERGED `d3f8ed3f` ([#635](https://github.com/hherb/kastellan/pull/635))

Full prose in [`archive/handover_20260831_626_pre-prune.md`](archive/handover_20260831_626_pre-prune.md).
Kept here only for what still binds:

- **The premise that kept it open was FALSE, and correcting it was most of the work.** #631's PR body
  and this file both said the configured arm "needs a live guard endpoint". It does not:
  `from_router_config` **skips the probe entirely** when `KASTELLAN_LLM_GUARD_TIMEOUT_MS` is pinned,
  so a configured boot needs only a mock answering `/props` — fully deterministic. The gap was real;
  documenting it as *unclosable* was the defect [[handover-claims-verify-before-carrying]].
- **`tests-common/scripted_llm` gained `EndpointKind::Props`**, matched on `/props` **with its
  leading slash and BEFORE** the chat fall-through — both halves load-bearing, each with its own
  unit test. Answered from **one stored body, not a FIFO**.
- **Literal assertions must sit beside a structural equality.** Equality puts `boot_payload` on
  *both* sides, so a defect inside it moves the two together and passes.
- **An UNDER-POWERED mutant is indistinguishable from a blind test in the result column.** The
  overlong-finding mutant survived first because it padded to 2.4 KiB against a 4 KiB cap — it never
  reached the condition it was aimed at. Compute what a mutant does to the quantity under test
  rather than trusting that "bigger" is big enough [[mutation-proof-counts-only-mutants-you-tried]].
- **Gated on the DGX 2026-08-31 at 3908 / 0 / 55** — the first execution of either
  `guard_boot_row_e2e` leg anywhere; the PR merged with both compiled but unrun.
- **Still open from it:** [#634](https://github.com/hherb/kastellan/issues/634) — a *migration*, not a
  build: `tests_common::daemon::bring_up_daemon` already exists with the `extra_env` seam. Three
  hand-rolled copies now share ~70 identical lines. Three files remain over the 500-LOC cap
  (`boot_report/tests.rs` 650, `guard_boot_row_e2e.rs` 687, `scripted_llm.rs` 514).

### #627 — the boot row's key set and rate assignment were untested — MERGED `8040ca83` ([#631](https://github.com/hherb/kastellan/pull/631))

Full prose in [`archive/handover_20260827_625_merged_pre-prune.md`](archive/handover_20260827_625_merged_pre-prune.md)
and in the ROADMAP's guard block. Kept here only for what still binds:

- **New pure module `cassandra::guard_model::boot_report`** — `BootRates::from_basis`,
  `boot_payload`, `not_configured_payload`, `timeout_ms`. **`boot_payload` takes `tau` + `n_ctx` as
  scalars and the budget for provenance, NOT a `&GuardTier` — and that IS the fix.** A `GuardTier`
  has no constructor but `from_router_config`, whose `/props` verification is fatal; that dependency
  is exactly why the payload was untestable [[unreachable-success-path-proves-nothing]].
- **A rate swap SILENCES the documented operator query, it does not invert it.** Since
  `slowest <= fastest` always holds, a swapped row asks `fastest < slowest / 2`, which no row can
  satisfy — the empty set on every host, forever. Four documents said "inverts" until `12809297`.
- **Mutation-proven thirteen for thirteen**, four of them found surviving by the five-agent review:
  `coverage_finding` routed only for `Probed` (the worst — *selective*, so invisible on a healthy
  host), `timeout_ms` returning the basis's `derived_ms`, `n_ctx` frozen to the one value all ten
  fixtures passed, and the durable `not_configured` token renamed with both assertions moving
  together. `main.rs` **824 → 771**.
- **Deferred, still open:** [#632](https://github.com/hherb/kastellan/issues/632) (rename
  `tok_per_s` → `fastest_tok_per_s` in `BootRates` **and** `TimeoutBasis::Probed` together; the
  durable wire key must not move).

### #624, #619, #615/#616/#618 — merged, compressed

Full prose in [`archive/handover_20260830_633_pre-prune.md`](archive/handover_20260830_633_pre-prune.md).
What still binds:

- **#624 (`4aee83ad`) — the probe measured the BOOT, not the host.** One sample ~3 s into startup
  measured *startup contention*: 6 073 / 269.6 / 1 582 tok/s on three consecutive boots of one
  unchanged DGX backend, against a reproducible ~7 000 minutes later — a **26x** under-measurement
  whose slowest boot fired a **false** ceiling finding. Fix: sample `PROBE_SAMPLES` times and keep
  the **FASTEST**, because prompt processing has a hardware ceiling and no floor, so contention can
  only make an observation slower and a mean is wrong for a one-sided error. **Each sample carries
  its OWN cache-buster**, or N identical prompts read as enormous throughputs on a backend that does
  not report `cached_tokens` — a fail-open manufactured by the fix. **The review's CRITICAL, and the
  rule it left:** `summarise(&samples)` → `summarise(&samples[..1])` silently reverted the whole fix
  and passed every guard test in the tree. **When a fix's value lives in a fold, pin the fold's
  *inputs*, not just its output shape.** Its remnant #626 is now fixed — see
  [Current state](#current-state).
- **#619 (`3bd45a36`)** — `classify_transport` folded the both-reqwest-flags-set case (a **connect
  timeout**) into `Timeout`, so a black-holed SYN read as 100% timeouts and sent an operator to
  #612's ~350 s pin, which cannot help: connect is capped at `min(timeout, 5 s)` independently.
  Fixed with `GuardErrorKind::ConnectTimeout`; `boot::is_timeout` is now
  `matches!(classify(e), Timeout)` so the two cannot diverge again. **The honest whole-fail-open
  query is `state NOT IN ('clear','block')`, not `error_kind IS NULL`.** Deferred:
  [#620](https://github.com/hherb/kastellan/issues/620),
  [#621](https://github.com/hherb/kastellan/issues/621),
  [#622](https://github.com/hherb/kastellan/issues/622) (`guard_tier_e2e` is in no gate and
  self-skips to a silent PASS — note it *did* run in this session's full sweep).
- **#615/#616/#618 (`e258ad3c`)** — `guard.error_kind` is a **closed discriminant** beside
  `guard.state` (never the backend's error text); `TimeoutBasis::Operator` carries a `PinBand` so an
  out-of-band pin reaches the `warn!` and the boot row while still being honoured verbatim (an
  **in-band** pin keeps the historic `"operator"` token — use `LIKE 'operator%'` to count all pins);
  `fetch_screen`'s Block arm withholds through a **total** function. **#616 is what unblocked #612's
  favoured option.** [#617](https://github.com/hherb/kastellan/issues/617) stays out of scope.

> ⚠️ **#624 does NOT close [#612](https://github.com/hherb/kastellan/issues/612), and merging the
> two is the mistake to avoid.** #624 is that the *sample* was taken under load on any host; #612 is
> that extrapolating from a ~1 KiB sample is non-linear on Metal *whatever* the load — a quiet Mac
> still reads 1 137 tok/s at 1 KiB and 260 at 64 KiB. **#626 narrowed it further and closes it no
> more than #624 did.** Both point at the same eventual remedy: measure from the `ms` /
> `body_byte_len` the guard rows carry since #616.

> ⚠️ **#614's merge wrongly CLOSED #612 and #615** via "Filed, **not fixed**: #N" — GitHub matches
> the `fixed: #N` substring and ignores the negation. Now in
> [Standing hazards](#standing-hazards-that-have-each-cost-a-session).


### #614's review rounds, the wiring slice, and the guard-model arc — compressed

Full prose in [`archive/handover_20260830_633_pre-prune.md`](archive/handover_20260830_633_pre-prune.md)
and [`archive/handover_20260823_wiring-slice_pre-prune.md`](archive/handover_20260823_wiring-slice_pre-prune.md).
What still binds:

- **`AuditSink::insert` is a provided method applying `truncate_payload` before delegating to
  `insert_stored`**, so no sink double can record a payload Postgres never stored
  [[audit-sink-doubles-hide-storage-transforms]]. That closed the CLASS; round one had kept half the
  defect by dropping an unaffordable preserved key *silently*.
- **The stated mitigation for an issue can disarm the instrument built to check it** — the live probe
  passed having measured nothing under a *pinned* timeout, precisely the configuration #612 tells a
  Metal operator to use. It now refuses a pin outright.
- **D10 — the tier is ADVISORY defence-in-depth, NOT a gate.** 65% recall (36/55) at FP-0; 6/6 on
  bare imperatives but **5/8 missed** on narrative framing. **Nothing downstream may relax on it** —
  no catalogue weight lowered, no allowlist widened, no sandbox constraint loosened.
- **τ = 0.79552656 is a REQUIRED operator input with no default**, and **five misconfigurations STOP
  THE DAEMON** (D6): half-configured keys, τ outside `(0.0, 1.0]`, a pinned timeout of 0, an
  unreachable `/props`, and a context below `SCAN_BYTE_CAP + 512 = 66 048` (D8).
  `KASTELLAN_REQUIRE_GUARD=1` makes the *unconfigured* case fatal too, because `install`
  regenerating `kastellan.env` drops all three keys at once and lands on the one non-fatal arm.
- **Measurement 3 ([#606](https://github.com/hherb/kastellan/pull/606))** — 133 cases, τ =
  0.79552656, FP-0 on both hosts. `best_tau` returns **NONE**: real captured content overlaps at
  every threshold. Its security-prose stratum was **catalogue-selected**, which is why **corpus
  growth from production is now the cheap path** — harvest it before designing another campaign.
- **`RouterConfig` lost its `Eq` derive** — `guard_tau: Option<f32>` can hold a NaN.
- **The other four `screen` call sites** (`fetch_screen`, `inner_loop/summary`, `channel/ingest`,
  `recall_assembly/pg_builder`) keep catalogue-only behaviour, as does the core-initiated
  `gliner-relex` dispatch. Widening is a separate slice with its own blast radius.

### Merged arcs, compressed

Full prose in [`archive/handover_20260821_pre-prune.md`](archive/handover_20260821_pre-prune.md) and the linked PRs. Kept here only for the lessons that still bind.

- **[#585](https://github.com/hherb/kastellan/pull/585) `f90631da` — Shieldstral adjudicator, guard-model slice 1.** Guard endpoint seam, adjudicator, offline calibration harness. **No production wiring** — five chokepoint files byte-identical to `main`, verified as a merge gate. **Two findings overturned the feasibility study** and must not be re-derived from it: (F1) its `0.45–0.70` band holds exactly one reachable value, so the tier is re-aimed at the catalogue **miss** at 0.0; (F2) `observation replay` is plan-level and cannot score a document-level tier. **The seeded 24-case corpus is a PROOF OF CONCEPT and does not discharge measurement 3** — any τ from it is provisional and must never become a default. Best review catch, and it generalises: *a mock that does not return what it was sent tests only your own canned response* — `guard_model_e2e`'s mock read only far enough to find `Content-Length`, leaving two tier-killing mutations green.
- **[#579](https://github.com/hherb/kastellan/pull/579) `bb937df7` — #564 slice 2, the ask channel.** `ChannelOutbox`, D16's peer-scoped `EXISTS` inside the guarded UPDATE (**the nonce is a BEARER token — reading, not guessing, was the real threat**), D17's `NONCE_BYTES` 32 → 5. Its five-agent review found eight things nine per-task reviews and 3522 tests had missed, all on the **argument-passing seams between layers** rather than in logic.
- **[#578](https://github.com/hherb/kastellan/pull/578) `af3e7e66` — #564 slice 1b, the ask path.** `Verdict::Escalate` stops degrading to `Block`; `Outcome::{AwaitingOperator, Denied}`, `final_state() -> Option<&'static str>`, the 60 s expiry sweep, `kastellan-cli inbox`, and **D11** (`asks.resume_state`, migration 0024) because a resumed task otherwise re-executed steps it had already run — approve a plan and an earlier step's email goes out twice.
- **[#572](https://github.com/hherb/kastellan/pull/572)/[#573](https://github.com/hherb/kastellan/pull/573) `fbe91c4d`+`e8ea4339` — mail attachments by `{message_id, filename}`.** Plus #574, the `/tmp`-wipe fix. Durable lesson: **a mutation score is only as good as the mutation set** — a reviewer's own 15 mutations left **11 surviving** with all 113 tests green, clustered exactly where per-module rounds had not looked.
- **[#569](https://github.com/hherb/kastellan/pull/569) `07b6451e` — guard measurement 2 + Q8.** Runtime and quantisation **PINNED**: llama.cpp + `Shieldstral-1.0-3B-Q8_0` on both hosts, so one fitted τ transfers.
- **Older arcs** (#555/#556/#558/#562, the channel-supervision arc, #549/#546/#540/#536/#528, email slice 1, the egress and micro-VM slices, the 0.2.0 release) — see the archive snapshots and [Recently merged](#recently-merged).

### Standing hazards that have each cost a session

> ⚠️ **Clippy parity is a `rustup update`, not a property of the hosts — check it, don't assume it.**
> CI pins nothing: both workflows use `dtolnay/rust-toolchain@stable`, so CI is whatever stable is on
> the day it runs. Both dev hosts are rustup on the same floating `stable` channel, so they drift out
> of parity simply by not being updated, silently. That is what bit #573: clippy-clean on the Mac
> *and* the DGX, then a CI failure on a lint that did not exist in the older toolchain.
>
> **State as of 2026-08-31: BOTH hosts are on 1.98.0 (2026-08-18) — CI parity**, updated this
> session from 1.96.0 (2026-05-25), two releases behind. `rustc --version` on the host you are
> gating on and compare against `rustup check` before trusting a clippy gate; `rustup update stable`
> if behind. Nothing pins them, so this parity will decay again on its own.
>
> The bump surfaced **no new lints on either host**: cold `--workspace --all-targets -D warnings`
> exit 0 with all 27 crates and zero warnings on the DGX (345 `Checking`+`Compiling`) *and* on the
> Mac (303 — fewer because the Linux-gated deps compile out). The Mac's 7 rustup targets, including
> the `aarch64-unknown-linux-gnu` used for cross-checking `cfg(linux)` code, survived the update.
> `kastellan-supervisor --lib` on the Mac: **113 / 0**, with 44 `launchd` tests observed and **0**
> `systemd` — the platform split, confirmed rather than assumed [[mac-compiles-zero-systemd-tests]].
>
> **Verified against reality the same day:** CI's `cargo check + clippy (linux)` and the
> matrix-worker job both passed on #636, which is the first time the local gate and CI have run the
> same compiler. A green DGX clippy now says something about CI; before this it did not.
> `rust-version = "1.78"` in the root `Cargo.toml` is the MSRV and constrains none of this.
>
> The whole gate was re-run on 1.98.0 and is **byte-identical to the 1.96.0 run**: **3908 / 0 / 55**,
> 176 suites, `TEST_EXIT=0`, the same 8 gliner-relex `[SKIP]`s, and cold clippy exit 0 over 27 crates
> with zero warnings. So the bump is cheap and the earlier fear that it "can surface unrelated lints
> across 27 crates" did not materialise — but that is a fact about *this* tree at *this* pair of
> versions, not a reason to skip the check next time.

> ⚠️ **A cached `cargo clippy` reports a full-workspace pass it never ran.** Exit code alone does not distinguish it — **count the `Checking` lines**. Honest from a cold `CARGO_TARGET_DIR` is ~217–303; a warm dir can report exit 0 having linted 4. Count against the *reverse-dependency set*, not against 27, or a correct incremental lint reads as a failure.

> ⚠️ **`cargo check`/`clippy --all-targets` do NOT warm the target dir for `cargo test`** — they emit metadata-only artifacts, no linked binaries. A full sweep after a lint-only leg pays a cold link (11m on the Mac vs 29s on the DGX). **Run the sweep first, lint after.**

> ⚠️ **A private `CARGO_TARGET_DIR` does not build `examples/`,** so `email_channel_e2e`'s 6 tests fail with `fixture not built` at a perfectly green commit. Fix: `cargo build -p kastellan-core --example fake_email_worker`. Same family as the daemon-e2e breakage a custom target dir causes ([[custom-cargo-target-dir-breaks-daemon-e2e]]) — read the failure text before believing a regression.

> ⚠️ **"Filed, not fixed: #N" in a PR body or commit message CLOSES #N.** GitHub matches the `fixed: #N` substring and has no notion of negation. It has cost three issues: #539 (2026-08-11, noticed), then **#612 and #615 together** (2026-08-24, unnoticed until the next session reconciled this file against `gh issue list`). Write **"deferred to #N"** or **"#N — filed, unfixed"**, and before merging run `gh pr view <n> --json body --jq .body | grep -oiE '(close[sd]?|fix(e[sd])?|resolve[sd]?)[[:space:]:,]+#[0-9]+'` over the body *and* the commit message.

> ⚠️ **Squash-merge caveat:** every PR lands as one squash commit, so its *branch-tip* SHA (where the gate ran) is **not** an ancestor of `main`. Check content, not `merge-base`.

> ⚠️ **Freshly-linked executables can hang forever in `_dyld_start` on macOS**, so every daemon e2e fails with the daemon's stdout **and** stderr **completely empty** — which reads exactly like a code defect. **Newness, not size:** it took the 40 MB daemon first and 13 KB cargo `build-script-build` binaries later, wedging a cold `cargo clippy` indefinitely, while anything already assessed kept running. Not the target dir and not signing — the hanging bin, a working old bin and a working fresh test bin are all identically `adhoc,linker-signed`. Rule it in **before** touching code: run a sibling daemon e2e, then the binary directly, then `sample <pid> 2` — one `_dyld_start` frame with a ~112 KB footprint is conclusive, on a build script as readily as on the daemon. **A warm `CARGO_TARGET_DIR` still works**, so `check` and `clippy --all-targets` remain available; a cold one is what wedges. Distinct from [[custom-cargo-target-dir-breaks-daemon-e2e]], which a `cargo build --workspace` fixes and this does not. [[mac-fresh-large-binaries-hang-in-dyld]]

> ⚠️ **`kastellan-worker-egress-proxy` leaks on the Mac.** Five orphans were live in one sweep, four of them 1–7 days old, across three target dirs. Test runs are not reaping them. Not investigated — flagged for whoever next touches sidecar lifecycle.

---

## Read these first

1. [`docs/architecture.md`](../../architecture.md) — process model, cross-platform table
2. [`docs/threat-model.md`](../../threat-model.md) — the invariant, scenarios in scope, defence layers
3. [`docs/devel/ROADMAP.md`](../ROADMAP.md) — the master sequenced TODO with commit hashes for shipped items
4. The design plan (outside the repo) — `~/.claude/plans/i-d-like-to-design-logical-starlight.md`
5. Memory notes (auto-loaded) — `~/.claude/projects/-Users-hherb-src-kastellan/memory/MEMORY.md`
6. [`archive/handover_20260803_pre-prune.md`](archive/handover_20260803_pre-prune.md) — the full prose for everything this file now summarises

---

## Next TODO

> Only *open* work is listed. Shipped items move to [Recently merged](#recently-merged) or the ROADMAP.

**With #626, #632 and #634 done, the guard arc's remaining work is one item and it is the one that
matters:** [#612](https://github.com/hherb/kastellan/issues/612), a design call rather than a patch
— **#616 unblocked its favoured option**, so it is now reachable rather than merely filed. Read the
measurement in the issue before proposing a fix; every cheap one is closed off there. Beside it,
newly filed and both cheap: [#639](https://github.com/hherb/kastellan/issues/639) (split
`guard_tier_e2e.rs`, which is also #622's cheapest option) and
[#638](https://github.com/hherb/kastellan/issues/638) (214 rustdoc warnings, 67 of them broken
intra-doc links, in a tree that treats doc comments as the design record). **The DGX redeploy is now UNBLOCKED and is the first action of the next session.** Its
gating condition — the operator's 2026-08-31 call that the deploy waits until
[#637](https://github.com/hherb/kastellan/pull/637) is code-reviewed and any resulting fixes land —
is **satisfied**: two review rounds ran against the PR, their five fixes are in the branch, and it
squash-merged as `44e0f38d`. So the whole guard arc
(`4aee83ad`/`8040ca83`/`d3f8ed3f`/`44e0f38d`) now ships in one movement, as intended. Until it does,
no live `guard_tier.boot` row carries `slowest_tok_per_s` / `measured_samples` /
`attempted_samples`, and that is expected rather than a gap. When it happens: `install` REGENERATES `kastellan.env` and
silently reverts tuned values, so re-add the four keys and repair the model tag afterwards
[[dgx-deploy-env-clobber-and-missing-workers]]; then verify at the **installed binary** with
`strings`, never at the checkout [[handover-claims-verify-before-carrying]]. The first cold-backend
boot after it is also the first chance to watch #626's retry on a real stalled `/v1/chat/completions`.

**Next up — operator's choice, each roughly one session:**

- **Three closed, two facts survive them.** ~~#561~~ (fixed upstream in localmail), ~~#506~~ (`cb33005c`), ~~#552~~ (`76ac51f5`); detail in [`archive/handover_20260824_diagnostics_pre-prune.md`](archive/handover_20260824_diagnostics_pre-prune.md). **#506's `floor_resolved` branch could not be exercised by the live gate** (the planner never omits the field on this host), so its PG e2e is that branch's only evidence. And **#561 leaves a latent, unfiled hazard: paging a `mail.search` with a *different* `query`** continues the date walk with the new filter and returns `200`, silently skipping anything newer than the cursor — keyset semantics working as designed, but it means don't change the query while paging.
- **[#560](https://github.com/hherb/kastellan/issues/560) — the planner fabricates a 16-hex `message_id`.** Do **not** close this by rewriting the parameter description: #536 already did exactly that ("not a placeholder"), deployed 2026-08-09, and both later runs still fabricated. The lead worth measuring is in the issue — with keys stripped by `extract_scannable_text`, `"20973"` reaches the planner as a bare line among subjects and dates, with nothing marking it as *the id* [[tool-output-reaches-planner-key-stripped]] [[opaque-ids-are-unusable-tool-params]].
- **[#550](https://github.com/hherb/kastellan/issues/550) — the *generated* `kastellan.env` gets no end-to-end check.** #531 verifies the optional overlay most hosts do not have and skips the required file every host does have; on a no-overlay host a dropped directive for it renders as the reassuring `none at …` line at `info!`. **The naive fix is wrong** — the overlay legitimately overrides `kastellan.env` keys, so per-file comparison false-positives; it has to compare the *folded* environment (later file wins), which `fold_env_files` already computes for launchd.
- **[#551](https://github.com/hherb/kastellan/issues/551) — no path directive escapes systemd's `%` specifier.** Pre-existing and workspace-wide (`ExecStart=`, `Environment=`, not just `EnvironmentFile=`): a literal `%` in `$HOME` renders a directive systemd mis-expands, dropping it with the same fail-open shape #530 fixed. Measure first, then either escape `%%` or reject at install.
- **[#548](https://github.com/hherb/kastellan/issues/548) — PG e2e tests install units into the operator's *real* `~/.config/systemd/user/`.** Filed 2026-08-13 while verifying #529 on the DGX: a unit from a hard-killed `channel_bus_pg_e2e` run on **2026-06-21** was still sitting beside the three production units. Not a teardown bug — `PgCluster`'s `Drop` guards are correct and simply cannot run on SIGKILL — so the fix is about blast radius, not cleanup. Cheapest option is a sweep of stale `kastellan-supervisor-test-*` units at bring-up; a scratch units dir is cleaner isolation but breaks anything that needs the manager to actually start the unit. Low priority, no correctness or containment impact. **Confirmed still accruing 2026-09-01**: the DGX now also carries a `failed` `kastellan-test-seccli-1-726614-…` unit — a *different* test from the 2026-06-21 one, so this is a slow leak rather than a single historical accident. `systemctl --user list-units --type=service --all | grep -i kastellan` shows them.
- **[#519](https://github.com/hherb/kastellan/issues/519) — `kastellan-microvm-run` is not deployable** (filed while fixing #504). It is resolved from `$PATH`, not as an exe-relative sibling, so the installer cannot reach it and `KASTELLAN_<WORKER>_USE_MICROVM=1` cannot work on a deployed host. Two candidate fixes in the issue; pick one. Small, but it is a design call, and the micro-VM backend has only ever run from a dev tree.
- **[#554](https://github.com/hherb/kastellan/issues/554) — `tool_allowlists` enforcement is kind-blind.** Split out of #541 rather than folded into it: the advertisement now withholds a row whose `kind` disagrees with its tool's declaration, but every worker still receives that row in its enforced allowlist. Filtering enforcement too would make the two agree *by construction* and is fail-closed, but it narrows what a deployed worker may do, so it needs a live DGX gate and a deliberate call on what `allowlist_len` in the `registry.loaded` row then means.
- **[#534](https://github.com/hherb/kastellan/issues/534) — give `ToolParam` a type.** Split out of #527 once the measurement showed it would have prevented at most 7 of that issue's 14 failures, and only by asking the model to contradict its own input. Smaller than #527 assumed: **38 `ToolParam` literals across 10 files**, and rendering is one pure function (`render_tools_block`). Two design calls to settle first, both in the issue: what a declared type *means* when the value round-trips through another tool's output, and whether an unenforced declaration is worth having (nothing would validate params against it, so a wrong annotation is a silent lie to the planner — the `full_headers` no-op shape).
- **[#564](https://github.com/hherb/kastellan/issues/564) — slices 1a, 1b and 2 are all MERGED** (slice 2 = [#579](https://github.com/hherb/kastellan/pull/579), `bb937df7`, 2026-08-20). What remains under this heading, none of it blocking:
  - **The producer. Still nothing, and the guard wiring slice did NOT change this** — say so explicitly, because it is the obvious wrong inference to draw from it. The tier lands at `tool_host`'s **output-screening** chokepoint and its outcome enum is `Block`/`Allow`/`AllowUnadjudicated`; it never constructs a `Verdict`, let alone `Escalate`. A guard-model `ReviewStage` (plan-level, escalating) would be a *different* consumer of the same adjudicator and is not built. So the whole ask path remains dormant. Whatever emits `Escalate` first — that stage, or the deferred DP severity-split — is slice 2's first real consumer, and the live Matrix round trip should be verified *then*, since it cannot be provoked before.
  - **Follow-ups filed from slice 1b, still open:** [#575](https://github.com/hherb/kastellan/issues/575) (three fail-closed error branches with no test that can fail — the issue carries a reviewer's hermetic recipe using a `Notify`-parked `ReviewStage`, which would *also* be the cheapest way to provoke an escalation on the live host), [#576](https://github.com/hherb/kastellan/issues/576) (`asks.resume_state` has no size bound), [#577](https://github.com/hherb/kastellan/issues/577) (the fail-closed read reports `plan_count: 0` for a task that ran N plans).
  - **[#583](https://github.com/hherb/kastellan/issues/583) + [#584](https://github.com/hherb/kastellan/issues/584) are FIXED and MERGED** (`47ba5b4f`, [#587](https://github.com/hherb/kastellan/pull/587)) together with [#582](https://github.com/hherb/kastellan/issues/582) — see [Current state](#current-state). **Still unfiled, from slice 2's reviews:** `via: "cli"` on the CLI's `ask.resolved` row is untested (`cli_inbox_e2e` only counts rows), and nothing drives `/deny` end to end, so `json!([Approve.as_str(), Approve.as_str()])` would pass.
  - **[#581](https://github.com/hherb/kastellan/issues/581) is answered on its load-bearing half** (Element does send a leading `/`; see the Current-state note). It stays open only for Q2/Q3, neither load-bearing.
  - **Still deliberately excluded:** the `ask_user` planner tool and `propose_plan` (same inbox item, different `kind`), a configured fallback destination for non-channel tasks, the autonomy-ceiling axis (`tasks.autonomy`), and the dead-letter store.
  - **Email delivery is wired but inert** — `EmailChannel::send` still refuses unconditionally, so an email-originated ask produces an honest `ask.delivery_failed` row rather than a silent drop. Correct until outbound SMTP lands.

- **Shieldstral guard-model — WIRED (`8736f559`, [#607](https://github.com/hherb/kastellan/pull/607)) and RUNNING LIVE on the DGX** (see [Current state](#current-state)). **Deployed 2026-08-25 and verified at the binary** — `strings` on the *installed* binary carries all five era markers (`guard_tier.boot`, `_dropped_preserved`, `error_kind`, `connect_timeout`, `operator-below-floor`), and task 178's `web.fetch` row read `{"p": 0.0081, "state": "clear", "error_kind": null}`. The lesson generalises: a DGX checkout can look current while the running daemon predates it by hours, because the tree was pulled and never rebuilt — `strings` on the installed binary beats every timestamp argument [[handover-claims-verify-before-carrying]]. **The whole guard arc since that deploy — `4aee83ad`, `8040ca83`, `d3f8ed3f`, `44e0f38d` — is on `main` and NOT yet deployed; the redeploy is the first unblocked action.** What remains:
  - ~~[#624](https://github.com/hherb/kastellan/issues/624)~~ **MERGED as `4aee83ad`** ([#625](https://github.com/hherb/kastellan/pull/625)) — the probe now takes up to 3 samples and keeps the fastest; see [Current state](#current-state). **The DGX has NOT been redeployed onto it** — that is the one outstanding operator action from this arc; expect `slowest_tok_per_s`, `measured_samples` and `attempted_samples` in the next `guard_tier.boot` row. Two follow-ups were filed from the review and both are now done: ~~[#626](https://github.com/hherb/kastellan/issues/626)~~ — **MERGED as `44e0f38d`** ([#637](https://github.com/hherb/kastellan/pull/637)), see [Current state](#current-state); the total budget is now `2 * PROBE_BUDGET_MS`, and the "weaken the finding instead" option was rejected because raising the total makes its trigger unreachable. ~~[#627](https://github.com/hherb/kastellan/issues/627)~~ — **MERGED as `8040ca83`** ([#631](https://github.com/hherb/kastellan/pull/631)). ~~[#633](https://github.com/hherb/kastellan/issues/633)~~ — **MERGED as `d3f8ed3f`** ([#635](https://github.com/hherb/kastellan/pull/635)), see [Current state](#current-state); it closed the configured arm's seam and, more usefully, retracted the claim that closing it needed a live endpoint. ~~[#632](https://github.com/hherb/kastellan/issues/632)~~ — **DONE on `fix/632-fastest-tok-per-s-rename`** together with ~~[#634](https://github.com/hherb/kastellan/issues/634)~~; see [Current state](#current-state).
  - **[#612](https://github.com/hherb/kastellan/issues/612) is the one that matters, and it is a design call, not a patch.** D9's boot probe extrapolates linearly from a ~1 KiB sample; on Metal that is 4.4× optimistic and a worst-case document fails **open**. Every cheap fix is closed off by the measurement in the issue — read it before proposing one. The four live options are: probe nearer the cap (correct, unaffordable at boot), fit a curve (inherits the cost), raise the safety factor (another D2 constant), or **measure at runtime from the `ms`/`body_byte_len` the guard row already carries** — which is the only one whose evidence is the real workload, and which reuses D5's own "let production be the measurement" move. Until it lands, a Metal host pins `KASTELLAN_LLM_GUARD_TIMEOUT_MS`. **#624 narrowed the problem but did not touch this one** — it removed the *contention* error from the sample; the *extrapolation* error is untouched and is Metal-specific. Do not read #624's merge as progress on #612 beyond better input data.
  - **A Mac daemon deployment is a deliberate decision now, not a task.** The tier boots fine there (91.4 s derived, `n_ctx` 66 048) but #612 means it fails open on large documents. Decide #612 first, or deploy with a pinned timeout and say so.
  - ~~[#615](https://github.com/hherb/kastellan/issues/615), [#616](https://github.com/hherb/kastellan/issues/616), [#618](https://github.com/hherb/kastellan/issues/618)~~ **DONE on `fix/615-616-618-guard-diagnostics`** — see [Current state](#current-state). **#616 is what unblocks #612's favoured option**, so the two are now in sequence rather than independent.
  - **[#617](https://github.com/hherb/kastellan/issues/617) is the remaining one of that four**, and it is bigger than its siblings: `req` is lost wholesale above the 4 KiB cap, and for `shell.exec` `req.argv` *is* the audited act. The allowlist is the wrong tool (unbounded); a bounded **producer-side** summary is the right one, which makes it a change in every tool's dispatch path rather than in `db::audit`.
  - **Then, in rough priority:** [#605](https://github.com/hherb/kastellan/issues/605) (the `PROVISIONAL` banner is unconditional — until it lands no report can say a τ is fitted); [#602](https://github.com/hherb/kastellan/issues/602) (an empty body pinned as the page — fail-**open** under `--record`); [#601](https://github.com/hherb/kastellan/issues/601) (profile divergence, quantified **inert** for this run but still wrong); [#603](https://github.com/hherb/kastellan/issues/603) (the URL inside the hash); [#608](https://github.com/hherb/kastellan/issues/608)–[#611](https://github.com/hherb/kastellan/issues/611) from #607's review; [#599](https://github.com/hherb/kastellan/issues/599)/[#600](https://github.com/hherb/kastellan/issues/600) from #598's.
  - **#604 is addressed, not closed.** D8 makes the 400 unreachable on a correctly sized host; it does not make it unrepresentable. Option 2 (cap by tokens) still wants a core-side tokeniser the guard seam deliberately does not have; option 3 (chunk and combine) changes what a score means.
  - **Corpus growth is now cheap, and that is new.** D5's per-dispatch `p` is live and — since the audit-cap fix — survives on large documents too, so production is finally a score source with no catalogue selection in it. Measurement 3's security-prose stratum was **catalogue-selected** (15 of 121 captures refused under the production `Relaxed` profile, including every GitHub-hosted candidate); nothing about that is true of the production distribution. Harvest it before designing another capture campaign.
  - **The other four `screen` call sites** (`fetch_screen`, `inner_loop/summary`, `channel/ingest`, `recall_assembly/pg_builder`) keep catalogue-only behaviour, and so does the core-initiated `gliner-relex` dispatch. Widening is a separate slice with its own blast radius.
  - **Live-host facts** (verified 2026-08-23): the DGX guard server is `llama-server … Shieldstral-1.0-3B-Q8_0.gguf --alias shieldstral --port 8081 -c 131072 -ngl 99`; `/props` reports the per-request context at `default_generation_settings.n_ctx` with **no top-level `n_ctx`**. Restart it with **at least `-c 66048`** or the daemon refuses to boot. The three guard keys live in `~/.config/kastellan/kastellan.env.local`, which `install` never rewrites.
- **Email channel — slices 2 and 3.** Slice 1 (gated inbound) MERGED, and #503 closed its MITM gap. Spec `docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`. **Slice 2** = SMTP outbound (`lettre`, MIT-verified) + full round trip; today `EmailChannel::send` refuses and every refusal is audited `channel.reply_undelivered`. **Slice 3** = DGX deploy + live tier: re-verify both negative controls (plan Task 4 Step 5 and Task 5 Step 5) on the deployed host, and **restart `localmail-serve` (+ `localmail-daemon`) on the DGX first** — see [Deployment facts](#deployment-facts-dgx). Its deploy blocker is gone: `41b21f36` (#520) installs `kastellan-worker-email-in`, so the channel is deployable for the first time.
- **[#497](https://github.com/hherb/kastellan/issues/497) — unify the per-family `ChannelBus` instances into one bus.** `main.rs` spawns a Matrix bus and an email bus, each with its own `PgCompletedTasks` LISTEN, so every bus sees every completed channel task and `handle_completed`'s `senders` miss is the normal case (the misleading `warn!("…dropping")` was demoted to `debug!` in `658924ae`). `ChannelBus::spawn` already takes a `Vec<Box<dyn Channel>>`, so this is mostly rewiring the two boot modules to return a *channel* rather than a *bus*; it also drops a redundant LISTEN connection and lets that log line be a real `warn!` again. Worth doing before a third channel family lands.
- **macOS Seatbelt-loopback verification of mail tier 1a** (carried from #490, non-blocking) — needs a Mac run with working launchd-PG; the Linux/bwrap leg is validated and tier 1b carries the macOS sandbox leg.
- **Telegram inbound** — still rejected as primary (no bot E2E, centralized, ban risk); revisit only as an additional `Channel` impl if a concrete need appears.
- **MITM-of-browser** (deferred egress slice-#2 follow-up): in-Chromium trust of the per-instance proxy CA via a proper NSS trust-store import — **not** `--ignore-certificate-errors-*`, since production must not be loosened to make a test pass. Only once a concrete inspection benefit justifies enlarging the sidecar blast radius.
- **File-split backlog (Item 9b)** — **re-`wc -l` before picking; the numbers drift and this list is
  a pointer, not a census.** The rule the tree follows, and the reason this list keeps growing rather
  than shrinking: **split BEFORE the change that grows a file**, in a movement-only commit whose
  `#[test]` name set is verifiable either side, so the movement diff is reviewable on its own.
  Folding a move in afterwards is the worst of both. `timeout.rs` (four files, 27 tests before and
  after), `tier/boot.rs` → `tier/probe.rs`, and `boot_supervisor/tests.rs` are the worked examples;
  `boot_report/tests.rs` at **515** is the counter-example this session added deliberately.
  - **Best first picks, each a pure test-lift** (production code untouched, count verifiable either
    side): `core/src/channel/ask_message.rs` **956** (~330 production, ~620 test),
    `workers/mail/src/handler.rs` **670** (~305 production),
    `sandbox/src/linux_firecracker/plan.rs` ~**1160** (~485 production; `cfg(linux)`, so DGX-gated),
    and `core/tests/guard_tier_e2e.rs` **1351**, whose ~200-line multi-request HTTP mock lifts to
    `tests/guard_tier_e2e/{main,mock}.rs`.
  - **Bigger, because any split is a production reorganisation** (small `mod tests`, so a test-lift
    saves nothing): `db/src/asks.rs` **1127**, `db/src/tasks.rs` **533**, `db/graph.rs` **926**
    (design-gated Item 23b — deferred until a 2nd `WalkedEdge` consumer).
  - **Clean seam already visible:** `core/src/scheduler/asks.rs` **801** — its pure half
    (`resolution_choice` / `decide` / `ask_deadline_seconds` / the resume-state codec) separates from
    its async half.
  - **Also over-cap, no seam called yet:** `core/src/scheduler/inner_loop.rs` 778,
    `core/src/channel/bus.rs` 742, `workers/matrix/src/sdk_live.rs` 722 (live-matrix-gated → DGX),
    `core/src/cassandra/guard_model/tier/error_kind.rs` 449 and `tier/tests.rs` 432 (both
    approaching), `llm-router/src/config.rs` 843, `llm-router/src/messages.rs` 586,
    `core/src/scheduler/inner_loop/summary.rs` 533, `core/src/scheduler/runner/task_exec.rs` 561,
    `core/src/main.rs` 771 (next lift: the bring-up block). Over-cap **test** files:
    `gliner_relex/tests.rs` 1083, `python_exec/tests.rs` 844, `inner_loop/tests.rs` 767,
    `scheduler/audit/tests.rs` 713, `cassandra/types/tests.rs` 654. Large e2e binaries the tree
    tolerates: `secret_vault_e2e` 813, `cli_ask_e2e` 858.
  - **≤27-over, a lift saves little:** `db/src/lib.rs`, `supervisor/src/launchd_agents.rs`,
    `core/src/scheduler/tool_dispatch.rs`, `db/src/memories/search.rs`,
    `entity_extraction/batch_upsert.rs`.

**Standing deferrals (no owner; pick up when a consumer appears):**

- **Egress** — [#242](https://github.com/hherb/kastellan/issues/242) tunnel idle/resolve timeouts (folds in the missing read idle-deadlines on `copy_bidirectional` + `peek_first_byte`); [#251](https://github.com/hherb/kastellan/issues/251) stale-scratch crash-sweep (needs cross-platform pid-liveness); [#304](https://github.com/hherb/kastellan/issues/304) real-sandbox cert-pin enforcement e2e (needs a controllable TLS origin); [#260](https://github.com/hherb/kastellan/issues/260) literal-IP HTTPS origins requiring an IP-SAN cert under MITM; transparent gzip/brotli if an origin refuses `Accept-Encoding: identity`; `pg_decision_sink` back-pressure decoupling before high-rate load.
- **True `jailer`** (root chroot + dedicated-uid drop) stays deferred to a privileged-tier `VmmConfinement::Jailer` sibling (seam already in `confine.rs`). **Generalizing net-worker-in-VM** needs no new work: 5c's `NetClientTransport`/`spawn_net_transport` IS the reusable mechanism; a 2nd consumer can adopt it directly.
- **5c/5b minors** — `spawn_net_transport`'s fail-closed-path doc-comment is subtly worded; DGX `net_demo_firecracker_egress_e2e` leaves `cpu_ms` at default (unused by the FC backend); [#381](https://github.com/hherb/kastellan/issues/381) (`size_mib` resize + mkfs↔flock TOCTOU); the `respawns_on_death_and_serves_again` unbounded-retry test wants a deadline guard.
- **python-exec Phase 4** — curated-wheels RO dir if/when skills demand third-party packages (stdlib-only today); tiered delegation policy (ROADMAP). Operator flip: `KASTELLAN_PYTHON_EXEC_ENABLE=1`.
- **web-search / web-research** — stand up a local SearxNG (`scripts/web-search/setup-searxng.sh`), set `KASTELLAN_WEB_SEARCH_ENDPOINT` + the `web-search` `tool_allowlists` row, run the `#[ignore]` `web_search_e2e::real_search_against_searxng`. web-research polish (all opus-triaged DEFER): `http.rs` trait doc stale; `search_err_to_rpc` gives a "search"-worded error on an *embed* misconfig; `embed_note` conflates three conditions under first-wins, so a benign cap note can mask a genuine embed failure (severity-rank it: failure > cap).
- **Entity-embedding** — an ANN index (ivfflat/hnsw) on `entities.embedding` once cardinality warrants it (sequential cosine scan today); a batch-embed seam behind the `Embedder` trait if embed latency becomes a recall-path cost.
- **handoff-cache** (ROADMAP:129) — on-disk Workspace-backed store, only once a per-task `Workspace` is wired into the live scheduler (it isn't today).
- **Older** (ROADMAP:130) — core-side caller wiring for `insert_memory_light` (lands with the first high-frequency writer); per-namespace caps + oldest-eviction on `memories.metadata`; graph-lane degradation test ([#196](https://github.com/hherb/kastellan/issues/196)).
- **Test-infra / small** — [#510](https://github.com/hherb/kastellan/issues/510) **CI never exercises #508's regression guard** (see [What CI does not cover](#what-ci-does-not-cover)) — its first step, a `REQUIRE_USER_MANAGER=1` knob turning a silent `[SKIP]` into a hard failure, is the same shape as the `KASTELLAN_GLINER_RELEX_REQUIRE_E2E=1` knob below and they should probably land together; [#134](https://github.com/hherb/kastellan/issues/134) `bring_up_pg_cluster` doc example or a real `_with_timeout` caller; [#104](https://github.com/hherb/kastellan/issues/104) systemic de-doubling of the `pid+nanos` suffix — **six** places, counted properly: `tests-common::unique_suffix`, three `TestRoot`s (`systemd_user`, `launchd_agents`, `atomic_write`), both supervisor smoke binaries, plus `atomic_write::tmp_path_for` and `install::run::staging_path` (#511 collapsed the two backend copies of the first into one, and added the last); [#353](https://github.com/hherb/kastellan/issues/353) route read-only `launchctl print` through `run_capped`; a `KASTELLAN_GLINER_RELEX_REQUIRE_E2E=1` CI knob.
- **Operator actions (no code)** — recapture observation fixtures (`cargo test -p kastellan-core --test observation_capture -- --ignored --nocapture`); real-model relation-extraction validation (`KASTELLAN_GLINER_RELEX_ENABLE=1 cargo test … entity_extraction_e2e`); live-pin direct-VM + literal-loopback-embed hybrid ranking ([#456](https://github.com/hherb/kastellan/issues/456)); live-verify the #454 forced-synthesis path ([#455](https://github.com/hherb/kastellan/issues/455)).

---

## Load-bearing findings that still bind

Full prose in the [`archive/`](archive/) snapshots — most recently
[`archive/handover_20260831_626_pre-prune.md`](archive/handover_20260831_626_pre-prune.md).

- **The four faults (2026-08-02).** Driven end to end from one real Matrix message: **four
  independent faults, only one a kastellan bug in the layer everyone suspected**, each masking the
  next. The durable lesson is the shape, not the four — a green stack with a silent output means
  look at every layer, and fix them one at a time so each fix's evidence is separable.
- **The fail-open `data_ceiling` correction — CLOSED, kept for the shape.** A cap that silently
  dropped what it could not fit produced a row indistinguishable from one where the thing never
  happened. **Absence and loss must not render identically**; name the refusal.
- **Egress / MITM traps (from #491–#503) — read before touching the proxy.** The proxy's MITM
  upstream trusts **webpki roots only**, so no hermetic self-signed origin is possible for a MITM'd
  worker's e2e; `extra_ca` is worker-side, for transparent-tunnel workers
  [[egress-proxy-upstream-trusts-webpki-only]]. A force-routed loopback endpoint needs an **IP SAN**
  — a DNSName holding an IP literal never matches, and the symptom looks like a sandbox failure
  [[macos-force-routed-loopback-needs-ip-san]]. A bare-host `Net::Allowlist` entry with no `:port`
  is an **all-port grant** [[bare-host-net-allowlist-is-all-port-grant]].
- **Deployment facts (DGX).** `install` REGENERATES `kastellan.env` and silently reverts tuned
  values; re-add the four keys and repair the model tag afterwards
  [[dgx-deploy-env-clobber-and-missing-workers]]. Force-routing IS baked into the generated unit and
  survives install [[dgx-force-routing-deploy-facts]]. Daemon logs are in
  `~/.local/state/kastellan/*.out`, not the journal. `scripts/upgrade_from_git.sh` does the whole
  build+install+restart+verify and is hardcoded to `main`. A kernel upgrade can drop the NVIDIA
  module and silently put Ollama on CPU, which looks exactly like a router bug
  [[dgx-apt-upgrade-drops-nvidia-module]].
- **Process lessons that have each cost a re-run.** A truncated gate log is not a gate — keep the
  full sweep in a file under `$HOME` and parse `test result:` with a regex
  [[truncated-gate-log-is-not-a-gate]]. Mutation testing contaminates the git **index**; `git diff
  --stat` afterwards is the only proof index == tree [[mutation-testing-contaminates-the-index]], and
  revert by copying the file, never `git checkout` [[mutation-revert-never-git-checkout]]. A PR body
  saying "not fixed: #N" **auto-closes** #N [[pr-body-not-fixed-autocloses-issue]]. Plan text is a
  defect source: subagents transcribe brief prose verbatim [[plan-text-is-a-defect-source]].

## Working state

### Test baseline (authoritative)

| Host | Commit | Result | clippy `-D warnings` | `[SKIP]` |
| --- | --- | --- | --- | --- |
| **DGX** (native aarch64, real bwrap + KVM + live PG 18) | **`06ea613d`** — the tip of `fix/632-fastest-tok-per-s-rename`, #632 + #634 | **3921 / 0 / 55**, **176** suites, `TEST_EXIT=0`, `--no-fail-fast --nocapture`. **Reconciles exactly: 3910 at `6d61f4e8` + 11**, the eleven new `daemon::spec::tests::` unit tests, **all eleven grepped out of the log as `... ok`** rather than subtracted. All six daemon e2es ran against real PG: `cli_ask_e2e` 2/0, `guard_boot_row_e2e` 2/0, `cli_memory_l3_run_daemon_e2e` 2/0, `cli_memory_l3py_run_daemon_e2e` 6/0, `mail_daemon_e2e` 1/0/2. ⚠️ **`observation_capture` is `#[ignore]` (0/0/1) and `mail_daemon_e2e`'s live-LLM leg likewise**, so the two `LlmEndpoint::Verbatim` call sites are **compiled but not executed anywhere** — which is precisely why `a_verbatim_llm_url_gains_nothing` exists as a hermetic unit test. `kastellan-tests-common --lib` **108**, against the Mac's 110: the delta is the two `cfg(target_os = "macos")` `serial::tests`, reconciled rather than assumed [[mac-compiles-zero-systemd-tests]]. Ignored unchanged at 55 | exit 0 from a **cold** private dir (`CARGO_TARGET_DIR=~/clippy-cold-634`, `rm -rf`d first): **345** `Checking`+`Compiling` lines, **zero** `warning` and **zero** `error` lines, **all 27 kastellan crates named** (counted `sort -u`; 330 distinct crates in total). 345 is the same figure the `8d92c02b`, `c0255cd7` and `d3f8ed3f` cold runs produced, which is what says this was a real full-workspace lint rather than a cached pass. rustc **1.98.0** | **8**, all gliner-relex (4 venv-shim, 4 `ENABLE != "1"`) — **zero** non-gliner skips, counted from a `--nocapture` run |
| **DGX** (native aarch64, real bwrap + KVM + live PG 18) | `6d61f4e8` — #632 alone, gated before #634 was written so the rename's evidence stands on its own | **3910 / 0 / 55**, **176** suites, `TEST_EXIT=0`, `--no-fail-fast --nocapture`. **Byte-identical to the `8d92c02b` baseline, and that is the point**: a pure rename adds and removes no tests, so an unchanged count is what correctness looks like here rather than a weak result. The instrument that *would* have caught the one real hazard — renaming the durable wire key with the field — is the pre-existing `CONFIGURED_KEYS` array, not a new test | folded into the `06ea613d` run below rather than run twice | **8**, all gliner-relex |
| **Mac** (aarch64 darwin) | **`06ea613d`** — the branch tip | **PARTIAL by design.** `kastellan-tests-common --lib` **110 / 0** (99 + the 11 new, all observed by name), and **15 mutants applied to `daemon/spec.rs`, all 15 killed**, each by the test written for it — the ordering transposition, the `/v1/v1` append, a `data_dir`/`user` swap, the removed 200-char cap, and eleven more. No daemon e2e ran: the `_dyld_start` blocker below still holds on this host | `check -p kastellan-core --all-targets` exit 0 and `clippy -p kastellan-tests-common -p kastellan-core --all-targets -D warnings` exit 0, zero warnings, warm private dir. **Still the load-bearing Mac leg**: the only host that compiles the `cfg(target_os = "macos")` arms of the three migrated e2es | n/a — no integration suite ran |
| **DGX** (native aarch64, real bwrap + KVM + live PG 18) | **`8d92c02b`** — the tip of `fix/626-probe-total-budget`, after the SECOND review round | **3910 / 0 / 55**, **176** suites, `TEST_EXIT=0`, `--nocapture`. **Reconciles exactly: 3909 at `c0255cd7` + 1**, the one new e2e `a_probe_that_stalls_once_then_measures_drops_the_finding`, grepped out of the log as `... ok`. Its sibling `a_probe_that_overruns_its_budget_derives_the_ceiling` also observed `ok`. `guard_tier_e2e` went 40.02 s / 20 cases → **40.03 s / 21**: the new case's 20 s runs concurrently with the overrun case's 40 s, so the suite absorbs it for 10 ms. Ignored unchanged at 55 | exit 0 from a **cold** private dir (`CARGO_TARGET_DIR=~/626r-clippy-target`, `rm -rf`d first): **345** `Checking`+`Compiling` lines, **zero** `warning` and **zero** `error` lines, **all 27 workspace crates named** (counted `sort -u`). 345 is the same figure the `c0255cd7` and `d3f8ed3f` cold runs produced, which is what says this was a real full-workspace lint rather than a cached pass. rustc **1.98.0** on both hosts, checked rather than assumed | **8**, all gliner-relex (4 venv-shim, 4 `ENABLE != "1"`) — **zero** non-gliner skips, counted from a `--nocapture` run so the count is evidence rather than an artifact of captured output |
| **DGX** (native aarch64, real bwrap + KVM + live PG 18) | `c0255cd7` — superseded by the row above, kept for the reconciliation chain | **3909 / 0 / 55**, **176** suites, `TEST_EXIT=0`, `--no-fail-fast --nocapture`. **Reconciles exactly: 3908 at `d3f8ed3f` + 1**, the single new `#[test]`, grepped out of the log as `... ok` rather than subtracted. **Unchanged from the pre-review gate at `a88691a4`, and that is structural rather than luck** — the review fix turned one assertion into a three-value loop instead of adding a test, so a stable count is what a correct fix looks like here. All four affected names observed running, including `a_probe_that_overruns_its_budget_derives_the_ceiling`, the one test whose *wall clock* moved (20 s → 40 s) and which carries no skip guard. Ignored unchanged at 55 | exit 0 from a **cold** private dir (`CARGO_TARGET_DIR=~/clippy-cold-626b`, `rm -rf`d first): **345** `Checking`+`Compiling` lines, **zero** `warning`/`error` lines, **all 27 workspace crates named** (counted `sort -u`). 345 is the same figure the `d3f8ed3f` cold run produced, which is what says this was a real full-workspace lint rather than a cached pass. rustc **1.98.0** on both hosts — CI parity, checked this session rather than assumed | **8**, all gliner-relex (4 venv-shim, 4 `ENABLE != "1"`) — **zero** non-gliner skips, counted; *not* the bwrap-userns skip, so containment really ran |
| **DGX** (native aarch64, real bwrap + KVM + live PG 18) | **`d3f8ed3f`** — merged `main`, #635 including its review round | **3908 / 0 / 55**, **176** suites, `TEST_EXIT=0`, `--no-fail-fast --nocapture`. **Reconciles exactly: 3907 at `64587ee9` + 1 net**, and all three deltas are *measured* rather than subtracted — `the_basis_table_covers_every_state_exactly_once` (+1) and `an_in_band_pin_stores_a_configured_row_with_a_null_coverage_finding` (+1) each grepped out of the log as `... ok`, and `classify_endpoint_wins_over_the_chat_fallthrough` (−1, the deleted tautology) grepped for and confirmed **absent**. **This is the first run in which either `guard_boot_row_e2e` leg has ever executed** — both pass. Ignored unchanged at 55 | exit 0 from a cold private dir (`CARGO_TARGET_DIR=~/clippy-cold-635main`): **345** `Checking`+`Compiling` lines (238 + 107), zero `warning`/`error` lines, **all 27 workspace crates named** (counted `sort -u`). rustc **1.96.0**; re-run at CI parity on **1.98.0** after `rustup update` — also exit 0, 0 warnings, 27 crates, 345 lines, so the two-release gap was hiding nothing here | **8**, all gliner-relex (4 venv-shim, 4 `ENABLE != "1"`) — *not* the bwrap-userns skip, so containment really ran |
| **Mac** (aarch64 darwin) | **`1e53bc9d`** — the branch tip, tree-identical to `d3f8ed3f` | **PARTIAL, and it stayed partial.** Unit legs only: `tests-common --lib` **99 / 0** (100 − the deleted tautology) with all four `classify_endpoint` cases observed by name, and `kastellan-core --lib` **1979 / 0 / 1**. **No daemon e2e ran** — a freshly-linked `kastellan` hangs in `_dyld_start` on this host, as do 13 KB build scripts, and `cli_ask_e2e` fails identically at a commit where it was green. The DGX row above is what closed that gap | `check --all-targets` exit 0 and clippy exit 0 over `kastellan-core` + `kastellan-tests-common` `--all-targets`, zero warnings, warm dir, both changed crates confirmed present in the `Checking` lines. **Still the load-bearing Mac leg**: the only thing that compiles the e2e's `#[cfg(target_os = "macos")] serial_lock()` arm | n/a — no integration suite ran |
| **DGX** (native aarch64, real bwrap + KVM + live PG 18) | **`12809297`** — the tip of `fix/627-guard-tier-boot-payload`, after the five-agent review round | **3901 / 0 / 55**, 175 suites, `TEST_EXIT=0`, `--no-fail-fast --nocapture`. **Reconciles exactly: 3900 at `33029e32` + 1** — the one *net* new `#[test]` the review round adds (`a_clamped_row_reports_the_enforced_budget_not_the_derived_one`; the coverage-finding test was renamed and table-driven rather than added, so the diff's raw `#[test]` count is misleading and the name set is the honest instrument). All **11** `boot_report::tests::` names observed running. Ignored unchanged at 55 | exit 0 over **236** `Checking` lines from a cold private dir (`CARGO_TARGET_DIR=~/clippy-cold-631fix`), zero `warning`/`error` lines. **All 27 workspace crates named**, counted `sort -u` | **8**, all gliner-relex (4 venv-shim, 4 `ENABLE != "1"`) — *not* the bwrap-userns skip, so containment really ran |
Older rows (`33029e32` DGX 3900, `020b0e53` Mac 3778, `b65e44ab` DGX 3890, the `fix/615-616-618-guard-diagnostics` Mac 3748, `8cb8cfb7` DGX 3854, `09c6231f` 3840/3718, `69834357` 3823, `0bae6b2c` 3759, `f46c67cf` 3749, `2ab6612c` 3686, `b58edc77` 3668, and 3047 back to 2950) are in the [`archive/`](archive/) snapshots — most recently [`handover_20260830_633_pre-prune.md`](archive/handover_20260830_633_pre-prune.md) § Test baseline.

**Both hosts are load-bearing, in opposite directions — always check both.** The two supervisor backends compile on one host each: a `launchd_agents.rs` change is invisible to the DGX and a `systemd_user.rs` change is invisible to the Mac, so the two hosts legitimately report different counts. This is sharper than it sounds — `cargo test` on the Mac compiles **zero** `systemd_user` tests, so a Mac-green run can be missing the test that pins a Linux fix entirely (it was, in #530). The mirror direction is just as real: Mac clippy compiles `cfg(target_os = "linux")` items out, so an unused cfg-linux helper fails only the DGX `-D dead-code` gate. [[cfg-linux-e2e-deadcode-dgx-clippy]]

**This is why shared, `cfg`-free modules keep winning.** #458's gate predicted 3067 and landed 3069 — investigated rather than accepted, and the +2 was exactly two `env_file` tests **running on Linux for the first time**, having lived inside the macOS-only launchd backend where they had never compiled on the DGX or in CI. Same argument as #511's `atomic_write` fold, empirically confirmed. Prefer one `cfg`-free module with tests that run on both hosts over two backend copies neither host fully sees.

**Predict the count, then reconcile the delta exactly.** Every gate in the table above was predicted from the diff's new `#[test]` count and investigated when it missed. That is the cheapest available detector for "a test I think I added is not being compiled" — which is precisely the failure the platform split produces.

**Mac verification runs under a private `CARGO_TARGET_DIR`** (the IDE's rust-analyzer holds `target/debug/.cargo-lock` — [[mac-cargo-buildlock-prefer-dgx]]), and **it must live under `$HOME`, not `/tmp`**: macOS scrubbed a scratchpad target dir *mid-run* once, so a test binary vanished between build and exec (`TEST_EXIT=101`) while every `test result:` line still said `ok`. Same `/tmp` hazard as the run logs, one layer down — [[dgx-run-logs-tmp-scrubbed]]. A private target dir avoids the lock but not the slowness, so prefer the DGX for full runs and the Mac for targeted suites.

The single Mac failure in the historical `#507` row was `egress_force_routing_e2e::forced_coupling_enforces_allowlist_and_ingests_decisions` — the load-sensitive sidecar-budget flake; it passed on the DGX, confirming host/load specificity rather than a code defect.
**Two standing reading rules.** A green run with `[SKIP]` lines means tests *skipped*, not that the sandbox contained anything — always re-check with `-- --nocapture` (CLAUDE.md's "when tests pass but feel suspicious"). And skip-as-pass counts as passed, so counts stay comparable with or without `--nocapture`.

#### What CI does not cover

`linux-check.yml` is **compile-only** — `cargo check --workspace --all-targets`, `clippy -D warnings`, and `cargo test -p kastellan-tests-common` (hermetic structural guards only). The workflow header argues that scope deliberately, and it is the right default. But it means a green PR check says *nothing* about behaviour, and two consequences are worth holding in mind rather than rediscovering:

- **`cargo test -p kastellan-supervisor` never runs on a PR.** So #508's regression guard — the tests pinning that `install`/`install_target` actually call `systemctl --user enable` — is enforced only by an operator running the DGX suite. Both tests are `cfg(target_os = "linux")` *and* `[SKIP]` without a live user manager, which `ubuntu-latest` does not have, so simply adding the job would not fix it. [#510](https://github.com/hherb/kastellan/issues/510) tracks the options (a `REQUIRE_USER_MANAGER=1` knob first, then a runner that can satisfy it).
- **There is no macOS job at all** — `.github/workflows/` is `linux-check.yml`, `docs.yml`, `release.yml`. So for `cfg(target_os = "macos")` code the gap is not "never run", it is **never compiled**: the launchd backend, its tests, and every macOS arm of a dual-platform file are invisible to CI, and the Mac is the only host that sees them (the mirror of [[cfg-linux-e2e-deadcode-dgx-clippy]]). That is the concrete reason #511 folded the two backends' staging helpers into one `cfg`-free module: shared code gets compiled and tested on a PR, per-backend code does not.
- **The one lever that does work: `tests-common`.** Its unit tests *are* run on every PR, which makes it the right home for hermetic *structural* guards over things CI otherwise cannot see — the provisioning sha256 pins, and since #504 the installer-coverage guard (`installable.rs`) that fails when any binary the workspace builds is neither installed nor explicitly exempted. If a defect's character is "survives every check not specifically looking for it", that crate is where the check belongs.
- More generally, **any invariant whose only guard is an integration test is DGX-gated**, and integration tests that skip-as-pass are indistinguishable from tests that ran. The #508 arc is the worked example of why that matters: correct-looking code, green everything, broken only after a reboot nobody performs during development.

**Standing macOS test-infra gotcha (not a regression):** a *full-workspace* run under `KASTELLAN_PG_BIN_DIR` flakes ~4 tests in `core/tests/embedding_recall_e2e.rs` at PG bring-up (`tests-common/src/pg.rs`) — parallel `initdb`/launchd churn (issue [#130](https://github.com/hherb/kastellan/issues/130) territory); they pass single-threaded and in isolation. Use skip-as-pass for the whole workspace on the Mac and run live-PG suites individually or on the DGX. Two `error[E…]` blocks in a full log are the **expected output of two pre-existing `compile_fail` doctests** (`Lifecycle::IdleTimeout` non-exhaustive, `WorkerCommand::new` private), both reporting `... ok` — not failures.

### Build & test

```sh
source "$HOME/.cargo/env"          # cargo isn't on the PATH for non-interactive shells
cargo build --workspace
cargo test --workspace             # authoritative counts in the table above
cargo test --workspace -- --nocapture   # required to verify [SKIP] lines
cargo clippy --workspace --all-targets -- -D warnings
./target/debug/kastellan           # the core daemon
```

**Required one-time Linux host setup (Ubuntu 24.04+):** `sudo scripts/linux/install-bwrap-apparmor-profile.sh` — without it every sandbox integration test skips silently. For the Firecracker backend: `sudo scripts/linux/install-firecracker-vsock.sh` (also a hard prerequisite for every `build-*-rootfs.sh`, which only *verify* the pinned guest kernel and never create one). macOS needs no setup.

**FC e2e gotchas (DGX) — read before running any Firecracker e2e:** rebuild the **release** launcher (`cargo build --release -p kastellan-microvm-run`) AND the affected rootfs (the init is baked in) AND `export PATH=$HOME/.local/bin:$PATH` (firecracker is off the non-interactive ssh PATH → the e2e silently skips-as-passes otherwise). `kastellan-core` won't cross-compile on the Mac (`ring` C dep), so core e2e are compile+run on the DGX only. `/var/lib/kastellan/microvm/` carries `vmlinux` + the four `*.ext4` images. `net_demo_firecracker_egress_e2e` also needs the egress-proxy binary built + a loopback origin (in-test); the CA rides `fs_read` under `/tmp` (a SHARE_ANCHOR) into the guest, and `KASTELLAN_NETDEMO_EXTRA_CA` must be in **both** `base_policy.env` and `NetTransportSpawn.extra_ca`. A VM worker's `WorkerSpec.program` must be the **in-rootfs** `/usr/local/bin/kastellan-worker-<name>`, never the host target-dir path ([[vm-worker-in-rootfs-binary-path]]).

### The tree — 27 crates

Full layout in the root [`README.md`](../../../README.md) § Layout, and the load-bearing crates in
[`CLAUDE.md`](../../../CLAUDE.md) § Project shape. Not duplicated here — it drifts, and the README is
the one a fresh reader finds first.


### Integration-suite map

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `sandbox` unit (linux / macos) | 16 / 14 | bwrap + cgroup argv builders; Seatbelt profile builder, path canonicalization, TinyScheme-injection rejection, mach-lookup guard |
| `sandbox` integration (`linux_smoke` / `macos_smoke` / `macos_container_smoke`) | 7 / 10 / 7+ | **real** jails: fs invisibility, net deny, relative-path reject, OOM-kill under MemoryMax, per-spawn `/tmp` tmpfs, fresh session leader, bind-mount-readonly |
| `core` (`shell_exec_e2e`, `python_exec_e2e`, `python_exec_container_e2e`) | 4 / 4 / 4 | **real** core→sandbox→worker round-trips under production policy; jail-contained socket attempt; per-spawn scratch; secret-scrub to `[redacted:]`; macOS micro-VM `mem_mb` cap + `Net::Deny` + >64 KiB params file channel |
| `core` (`web_fetch_e2e`, `web_search_e2e`) | 1+1 / 1+1 | **real** sandbox deny-paths (off-allowlist host denied; endpoint off-allowlist ⇒ worker refuses at startup); `#[ignore]` real-network tiers |
| `core` (`egress_proxy_e2e`, `egress_force_routing_e2e`) | 2+1 / 3+1 | **real** sandboxed sidecar + CONNECT client: allowed round-trip, 403, `decision_to_audit`, `ca.pem` export, 1:1 teardown, Linux-only no-direct-route, PG-gated `pg_decision_sink`→`audit_log` |
| `core` (`email_mitm_e2e`) | 2 | **real, hermetic, MITM**: force-routed `email-in` polls a self-signed HTTPS mock; asserts the **round-tripped event** plus `tls_intercepted:true`. Negative control pinned to `mitm_failed: origin TLS handshake`, not any `mitm_failed:` |
| `core` (`mail_e2e`, `mail_daemon_e2e`) | — | jailed `mail.search`, attachment delivery across the `fs_write` boundary, force-routing coupling; scripted planner advertises + dispatches `mail.*` (`#[ignore]` real-LLM tier) |
| `core` (`email_channel_e2e`) | 8 | hermetic channel loop incl. the header-order-bypass and skipped-id-cursor-wedge regressions |
| `core` (`injection_guard_e2e` / `_fixtures`, `secret_vault_e2e`) | 6 / 4 / 9 | **PG-required**: policy rows, privacy invariant, per-tool profiles (#142), materialize/redeem, fail-closed redemption, opaque-ref-not-plaintext |
| `core` (`memory_recall_e2e`, `cli_ask_e2e`, `cli_memory_l3*`) | 1 / 2 / 17 | three-lane RRF recall + 1-hop expansion; full prod chain against a queued mock LLM; L3 list/remove/approve/revoke/pin + operator `run` |
| `core` (`guard_boot_row_e2e`) | 1 | **PG-required, hermetic:** a real daemon boots a **configured** guard tier against a `/props`-only mock with `KASTELLAN_LLM_GUARD_TIMEOUT_MS` pinned above the ceiling, and the stored `policy / guard_tier.boot` row is asserted equal to `boot_payload(..)` plus five literals. Zero guard chat requests proves the pin skipped the probe; one `/props` proves the boot verified the context once |
| `core` (`handoff` unit + `handoff_dispatch_e2e`) | 19 + 3 | cache budget/eviction/purge; dispatcher-level `fetch` intercept |
| `db` unit + `postgres_e2e` | 71+ / 8+ | builders, SQL pins, secrets AES-GCM; probe idempotency, runtime-role REVOKE, audit NOTIFY, cascade + journalling |
| `llm-router` unit + integration | 41 + 8 | config, wire shapes, `compose_url`, `pick_backend`; hand-rolled TCP mock chat + embed chokepoints |
| `egress-proxy` unit | 37 | `decide`, real-UDS `handle_conn`, CA round-trip, leaf cache, `looks_like_tls`, hermetic two-leg TLS with only-CA worker trust |
| `prelude` unit + smoke | 21 | env/profile parse, BPF builds, landlock + seccomp smoke |
| `supervisor` unit + integration | 44–52 + 2–4 | unit/plist builders, name validation, driver round-trips (macOS serialised via a reentrant mutex) |
| `web-fetch` / `web-search` / `web-common` unit | 21 / 24 / 8 | extraction, redirect-drive caps, SearxNG parse, loopback/scheme truth tables, allowlist matcher |

Older rows (3668 back to 3327, covering the guard slice-1 arc, #587, #579 and #578) are in [`archive/handover_20260823_pre-prune.md`](archive/handover_20260823_pre-prune.md) § Test baseline, and in the archive snapshots before it.

---

## Key design decisions locked in

- **Vendor-neutral, AGPL-compatible deps only.** Apache-2.0 / MIT / BSD / MPL / LGPL / (A)GPL fine; CDDL, BUSL, SSPL, Elastic and "source-available" are blocked.
- **Cross-platform first-class.** Linux (DGX Spark primary) + macOS. No Linux-only code without a macOS counterpart of equivalent guarantee.
- **Rust core, Python only inside sandboxed workers.** No PyO3, no in-process Python; the core never executes untrusted code in-process.
- **One process per worker, one OS sandbox per worker.** No "spawn unsandboxed" escape hatch in `tool_host` — don't add one.
- **Hybrid LLM with policy routing.** Local-first over OpenAI-compatible HTTP; Frontier only via the Phase-5 policy gate, through the egress proxy.
- **Single-host deployment via OS-native user-level supervisors** (`systemd --user` / launchd). No k3s.
- **Fixed core tools, sandbox-bound agent-authored Python.** Named/persisted skills get a human-approve gate (the L3 arc).
- **JSON-RPC 2.0 over stdio** — MCP-stdio compatible, so a richer MCP client can be swapped in without moving the trust boundary.
- **The operator→daemon command channel is the Postgres `tasks` queue + `LISTEN/NOTIFY`**, not a new IPC socket. `ask` and `memory l3 run` both ride it (#179 Opt-3).
- **Threat-model invariant:** worst-case compromise reaches *at most* the agent's own OS user, its own Postgres role, its own scratch FS, and the allowlisted endpoints for the *one* compromised tool. Nothing else.

---

## Recently merged

Newest first. Older entries live in the [`archive/`](archive/) snapshots and in git history.

- **`44e0f38d`** ([#637](https://github.com/hherb/kastellan/pull/637)) — #626, the
  saturating-first-sample defect. See [Current state](#current-state).
- **`d3f8ed3f`** ([#635](https://github.com/hherb/kastellan/pull/635)) — #633, the configured
  `guard_tier.boot` seam pin.
- **`8040ca83`** ([#631](https://github.com/hherb/kastellan/pull/631)) — #627, `boot_report` extracted
  as a pure module.
- **`4aee83ad`** ([#625](https://github.com/hherb/kastellan/pull/625)) — #624, the probe takes up to
  three samples and keeps the fastest.
- **`3bd45a36`** ([#623](https://github.com/hherb/kastellan/pull/623)) — the connect-timeout fold.
- **`e258ad3c`** ([#619](https://github.com/hherb/kastellan/pull/619)) — `guard.error_kind` as a
  closed discriminant; `TimeoutBasis::Operator` carries a `PinBand`.
- **`8736f559`** ([#607](https://github.com/hherb/kastellan/pull/607)) — the guard tier WIRED and
  running live on the DGX.
- **`bb937df7`** ([#579](https://github.com/hherb/kastellan/pull/579)) — #564 slice 2, the ask channel.
- **`af3e7e66`** ([#578](https://github.com/hherb/kastellan/pull/578)) — #564 slice 1b, the ask path.


### Earlier history

One bullet per session, newest first, in [`archive/handover_20260803_pre-prune.md`](archive/handover_20260803_pre-prune.md) § "Earlier history" — covering the Firecracker micro-VM slices 1–5c, the python-exec warm/idle arc, the Matrix worker hardening + live-channel arc, the planner-feedback arc (#337–#340), the entity/L1-embedding arc, the L3 skill arc, the egress-proxy slices #1–#4, the comms/channel-bus slices, the crates.io 0.1.0 release and the hhagent→kastellan rename. Older snapshots: [`20260727`](archive/handover_20260727_pre-prune.md), [`20260719`](archive/handover_20260719_pre-prune.md), [`20260629`](archive/handover_20260629_pre-prune.md), [`20260615`](archive/handover_20260615_pre-prune.md), [`20260611`](archive/handover_20260611_pre-prune.md), [`20260605`](archive/handover_20260605_pre-prune.md), [`20260529`](archive/handover_20260529_pre-prune.md), [`20260510`](archive/handover_20260510_pre-prune.md).

---

## Open follow-up issues (filed but not picked)

Beyond those already listed under [Next TODO](#next-todo). Only currently-open issues; closed-issue detail lives in the archive snapshots and git history.

- **From [#587](https://github.com/hherb/kastellan/pull/587)'s review, all four deferred with reasoning rather than folded in:** [#588](https://github.com/hherb/kastellan/issues/588) — the shared `live_ask_for_claimant!` fragment's `$2`/`$3` bind contract is doc-only, and because both binds are `text` a transposition **type-checks and returns zero rows**, which is fail-*closed* in `resolve_with_nonce` and fail-**open** in `any_live_nonce_for_claimant`; the agreement test that would catch it is `harness()`-gated skip-as-pass. [#589](https://github.com/hherb/kastellan/issues/589) — `AskRejectReason` enum + a `Containment` newtype, so `?` inside the inverted-polarity `containment_refusal` becomes a **type error** instead of a paragraph-prohibited idiom that reads as ordinary Rust. [#590](https://github.com/hherb/kastellan/issues/590) — seal `AskResolver`: a public trait in a published crate whose external impl decides a containment outcome, and could be fail-open. [#591](https://github.com/hherb/kastellan/issues/591) — the char-boundary truncation walk is hand-written in five places and its *correct* test exists in only one.
- ~~[#592](https://github.com/hherb/kastellan/issues/592)~~ **CLOSED by `abb3d3a7`** ([#598](https://github.com/hherb/kastellan/pull/598)) — the pin is checked at use, so the measurement-3 spec's D6 is unblocked. See [Current state](#current-state).
- **From [#598](https://github.com/hherb/kastellan/pull/598)'s review, both deferred because each changes another tool's contract:** [#599](https://github.com/hherb/kastellan/issues/599) — `--weights-unpinned` still exits **0**, so nothing machine-readable separates a τ fitted on the pinned bytes from one fitted against a server we could not identify at all; the artefact says `UNPINNED` loudly but the exit status does not, and `guard_calibrate_cli_e2e` now passes the flag on every leg, which is how a flag becomes habitual. [#600](https://github.com/hherb/kastellan/issues/600) — `scripts/eval/run-shieldstral-llamacpp.sh`, **the one script in the tree that launches a Shieldstral server**, still checks only that `$MODEL` exists; `require_guard_weights` has no automated caller at all, which is weaker than the `require_guest_kernel` precedent it cites.
- **From measurement 3 (2026-08-23), five filed with the evidence that found each:** [#601](https://github.com/hherb/kastellan/issues/601) — `guard capture` admits a document under `Relaxed` (production's profile for `web-fetch`, via `for_tool`) and `guard calibrate` then excludes on `Strict` (`screen()`), so the corpus is filtered by one gate and scored for exclusion by a stricter one; **quantified as inert for this run** (0 captured cases excluded), still wrong. [#602](https://github.com/hherb/kastellan/issues/602) — a rate-limited **200 with an empty or truncated body** is hashed and, under `--record`, pinned *as the case*; measured `e3b0c442…` (the empty-string sha256) from a real fetch with curl exiting 0. #596 closed this for 404s and never checked the body. Fail-**open**. [#603](https://github.com/hherb/kastellan/issues/603) — the pin covers the **final URL**, so a Wayback redirect to an equivalent snapshot reads as `The source has drifted` when the document is byte-identical; fail-noisy, and it trains an operator to look past the campaign's loudest signal. [#604](https://github.com/hherb/kastellan/issues/604) — **`SCAN_BYTE_CAP` bounds bytes, not tokens**: 65,536 bytes tokenised to **44,437** and the adjudication died on HTTP 400, because the byte→token ratio is **attacker-controlled** (M1's prose 6.5 B/token, dense jailbreak text 1.47). [#605](https://github.com/hherb/kastellan/issues/605) — the `PROVISIONAL` banner is an unconditional `push_str` stating a criterion it does not check, so the one line separating a proof-of-concept τ from a fitted one can never change.
- **From [#625](https://github.com/hherb/kastellan/pull/625)'s five-agent review, two filed rather than folded in because each is a decision rather than a fix — both now DONE:** ~~[#626](https://github.com/hherb/kastellan/issues/626)~~ (the saturating-first-sample defect; fixed by `PROBE_TOTAL_BUDGET_MS = 2 * PROBE_BUDGET_MS`, merged `44e0f38d` — see [Current state](#current-state), and note the issue's own text quotes the wrong finding). [#627](https://github.com/hherb/kastellan/issues/627) — `report_guard_tier` is private to the binary with **no `cfg(test)` module**, so swapping `tok_per_s` and `slowest_tok_per_s` in the payload (which inverts the documented `slowest < tok_per_s / 2` operator query) is silent, as is deleting any of the three new keys. Extract a pure `guard_tier_boot_payload(...) -> Value` into the lib.
- **From [#614](https://github.com/hherb/kastellan/pull/614)'s round-two review, four filed rather than folded in because each changes behaviour beyond the branch:** [#615](https://github.com/hherb/kastellan/issues/615) — an operator-pinned `KASTELLAN_LLM_GUARD_TIMEOUT_MS` **below `TIMEOUT_FLOOR_MS` or above `TIMEOUT_CEILING_MS` is accepted in silence** (`validate_operator_timeout` refuses only `0`, and `TimeoutBasis::Operator` yields no `coverage_finding`). Not clamping is deliberate and should stay; saying *nothing* is the defect — sharpened by #612 telling Metal operators to pin ~3× the ceiling. [#616](https://github.com/hherb/kastellan/issues/616) — `guard.state` collapses timeout / connect / HTTP-status / decode into `"router_error"`, so the durable record **cannot count the fail-open** that #612 is entirely about; a closed enum discriminant (`error_kind`) carries no attacker-controlled bytes and would fix it without weakening the no-backend-text rule. [#617](https://github.com/hherb/kastellan/issues/617) — `req` is still lost wholesale above the cap, and for `shell.exec` **`req.argv` *is* the audited act**; the allowlist is the wrong tool (unbounded), a bounded producer-side summary is the right one. [#618](https://github.com/hherb/kastellan/issues/618) — `fetch_screen`'s Block arm has an else-less `as_object_mut`, a silent fail-open *shape* on a screening path (unreachable today via the `get("data")` guard three lines up).
- **From the Headlong cross-project study (2026-08-27), both design-input rather than defects:** [#628](https://github.com/hherb/kastellan/issues/628) — `audit_log`'s causal structure lives in payload prose, not columns: `task_id` is a payload key in 58 places across 19 files, enforced nowhere, and there is no grouping key for one plan iteration and no link from a row to the row that caused it. This is the #616 defect one level up ("could not be counted — only inferred, by correlating…"), and #619's `LIKE 'operator%'` is the same shape again; proposed fix is `task_id` + a generic `caused_by` stamped in `audit::insert`, with Headlong's reader rule — *readers must tolerate absent fields, never reconstruct membership heuristically* — landing **with** the migration. Also hands Phase 5's audit UI a tree for free. [#629](https://github.com/hherb/kastellan/issues/629) — `MemoryLayer::L4` (session digests) is declared, accepted by the DB, and **written by nothing**, while `prompt_assembly::assemble` has no episodic block at all, so a task started today knows nothing of yesterday beyond whatever L2 row someone wrote. Fill it with `tiered_memory.md`'s logarithmic rollup pyramid over `audit_log` (fanout 10, tier *k* covers 10^k rows, built forward-only from a start marker, coarse tiers cite `audit_log.id` ranges and are *an index, not testimony*). Depends on #628 for cheap citation. The `<history>` block is model-authored, so it escapes like `<recalled>`, not verbatim like `<l0_meta_rules>`.
- [#634](https://github.com/hherb/kastellan/issues/634) — **a third hand-rolled `bring_up_daemon` now exists in `core/tests/`** (`cli_ask_e2e`, `observation_capture`, and #633's new `guard_boot_row_e2e`), sharing ~70 identical lines: the log + state dirs behind `PathGuard`s, the `core_service_spec` naming, four env vars, install/start, `wait_for_status(Active)` and `wait_for_log_match("scheduler spawned")`. Every divergence is one `env()` call, so a `tests_common::daemon::DaemonSpec` builder covers all three with no `cfg`. Two copies was arguably under the threshold; three is not, and #530 is the worked example of the cost — a bring-up fix landing in one copy and not the other. Bonus: `tests-common`'s unit tests are the one thing CI runs on every PR.
- [#638](https://github.com/hherb/kastellan/issues/638) — **`cargo doc --workspace` emits 214 rustdoc warnings** and exits 0, so nothing has ever forced a look. 115 private-link, **67 broken intra-doc links**, 6 function/module ambiguities that may already resolve to the wrong item, 4 unclosed HTML tags that can corrupt rendering; `core/src` holds 135. Matters here more than in most trees because doc comments *are* the design record and `[`intra-doc`]` links are how a reader moves between them. **Not** proposing a CI gate or fixing the 115 first — the issue carries a cheapest-signal-first order. Found while checking #626's own new links resolved; none of it comes from that branch. ⚠️ **A warm target dir under-reports this badly** — one run said 3 errors having documented 4 of 27 crates; count the `Documenting` lines, same rule as clippy.
- [#597](https://github.com/hherb/kastellan/issues/597) — **#592's shape one artefact along:** the two hosts hold different *projectors* (Mac `mmproj-F16`, DGX `mmproj-BF16`, different sizes), and ROADMAP claimed they matched. Inert while the guard tier runs `vision:false`; pin it the same way if a guard path ever loads one. The mechanism already exists — `require_guard_weights` takes a path.
- [#564](https://github.com/hherb/kastellan/issues/564) — also carries a [Next TODO](#next-todo) bullet; listed here too because it is the blocker under `ask_user`, plan-approval, and the deferred `Escalate` severity-split alike.
- [#515](https://github.com/hherb/kastellan/issues/515) — the channel supervisor awaits its audit sink (deliberately: row ordering + test determinism), and the Postgres sink inherits sqlx's 30 s pool-acquire timeout, so an unreachable PG can delay daemon shutdown by that much. Fix is a 5 s `tokio::time::timeout` around the sink call; testable with a `std::future::pending()` sink.
- [#501](https://github.com/hherb/kastellan/issues/501) — no long-lived channel sidecar (Matrix or email) ever gets leak-scanner fingerprints, **and the proxy fails open**, so it looks scanned.
- [#535](https://github.com/hherb/kastellan/issues/535) — committed source comments point at `.superpowers/sdd/` reports, which are gitignored and deleted with the workspace; carries a suggested `tests-common` guard and the note that `mock_localmail.rs` is now 653 lines.
- **The jail has no NSS, and that is now documented rather than fixed** (fallout from #539, closed by [#546](https://github.com/hherb/kastellan/pull/546) / `d41654ce`). Any command resolving a user or group name — `ls -l`, `id`, `whoami`, a bare `python3` — dies by SIGSYS on `socket(2)` inside a `WorkerStrict` worker. The kill is now *loud*, but the commands still cannot run. Deliberately NOT fixed by widening `BASE_ALLOW` (that is `Net::Deny` itself) and NOT by giving the jail a `HOME` (measured to fix python3 and **not** `ls -l`/`id`/`whoami`). If a real need appears, the options and their evidence are in `docs/superpowers/specs/2026-08-11-signal-killed-child-is-loud-design.md` §1.2 and §2.
- [#537](https://github.com/hherb/kastellan/issues/537) / [#538](https://github.com/hherb/kastellan/issues/538) — the #536 `/fixall` leftovers: `mail.search`'s `filters.account_ids`/`folder_ids` bypass the `LocalmailId` widening entirely (advertised to the planner, forwarded as an opaque `Value`) — **measure live before deciding the fix**, the way #527 was; and `workers/mail` has **no `[dev-dependencies]` at all, so its e2e hand-rolls a second localmail mock (signposted in both files rather than consolidated — the honest fix is a third dependency-free fixture crate, not a dev edge from a leaf worker to `core`).
- [#485](https://github.com/hherb/kastellan/issues/485) / [#484](https://github.com/hherb/kastellan/issues/484) — enforce `SingleUse` for `wants_workspace_out` tools in release builds (not just a `debug_assert`); lazy per-task out dir instead of eager mkdir+rmdir.
- [#456](https://github.com/hherb/kastellan/issues/456) / [#455](https://github.com/hherb/kastellan/issues/455) — live-pin direct-VM hybrid ranking; live-verify the #454 forced-synthesis path.
- [#442](https://github.com/hherb/kastellan/issues/442) — consolidate datetime crates (`time` + `jiff` coexist).
- [#438](https://github.com/hherb/kastellan/issues/438) — guard `tool_doc().method` against drift from each worker's real JSON-RPC handler.
- [#426](https://github.com/hherb/kastellan/issues/426) — `ProxyConnectGet` spins 4 worker threads for single-request workers.
- [#407](https://github.com/hherb/kastellan/issues/407) — extend the crash-orphan scratch sweep to `pyexec-<pid>-<seq>` dirs (macOS).
- [#396](https://github.com/hherb/kastellan/issues/396) — harden the VM-mode matrix bootstrap password file against `/tmp` symlink races.
- [#378](https://github.com/hherb/kastellan/issues/378) / [#372](https://github.com/hherb/kastellan/issues/372) / [#356](https://github.com/hherb/kastellan/issues/356) — FC `MemoryMax == mem_mb` leaves no VMM headroom; unconditional `mkfs.ext4` probe gate; python-exec container mode registers without validating the image exists.
- [#334](https://github.com/hherb/kastellan/issues/334) — stream chat completions so slow planner calls don't hit the timeout.
- [#332](https://github.com/hherb/kastellan/issues/332) / [#328](https://github.com/hherb/kastellan/issues/328) — PgListener + `pool.close()` deadlock isolation test; the `cli_ask_e2e` load flake.
- [#330](https://github.com/hherb/kastellan/issues/330) — matrix worker: detect + recover from an incompatible on-disk crypto store after an SDK upgrade.
- [#317](https://github.com/hherb/kastellan/issues/317) — installer `--assets-from` override.
- [#298](https://github.com/hherb/kastellan/issues/298) — full-DAEMON python-exec output secret-scrub e2e (design-first: needs a Vault-ref test seam in `main.rs`).
- [#286](https://github.com/hherb/kastellan/issues/286) — macOS browser-driver `localhost:*` loopback widening is host-shared (no netns). Latent; fix is scoping to the shim's bound port, a UDS-only transport, or the `MacosContainer` VM-netns backend.
- [#277](https://github.com/hherb/kastellan/issues/277) — `invoke_skill`: a templated/Python same-name collision is silent to the planner.
- [#243](https://github.com/hherb/kastellan/issues/243) — verify `net_client` seccomp permits AF_UNIX accept + UDS path identity on Linux.
- Long-tail hygiene: [#3](https://github.com/hherb/kastellan/issues/3), [#4](https://github.com/hherb/kastellan/issues/4), [#8](https://github.com/hherb/kastellan/issues/8), [#13](https://github.com/hherb/kastellan/issues/13), [#14](https://github.com/hherb/kastellan/issues/14), [#20](https://github.com/hherb/kastellan/issues/20), [#21](https://github.com/hherb/kastellan/issues/21), [#24](https://github.com/hherb/kastellan/issues/24), [#37](https://github.com/hherb/kastellan/issues/37), [#39](https://github.com/hherb/kastellan/issues/39), [#40](https://github.com/hherb/kastellan/issues/40), [#42](https://github.com/hherb/kastellan/issues/42), [#47](https://github.com/hherb/kastellan/issues/47), [#50](https://github.com/hherb/kastellan/issues/50), [#55](https://github.com/hherb/kastellan/issues/55), [#62](https://github.com/hherb/kastellan/issues/62), [#63](https://github.com/hherb/kastellan/issues/63), [#73](https://github.com/hherb/kastellan/issues/73), [#76](https://github.com/hherb/kastellan/issues/76), [#78](https://github.com/hherb/kastellan/issues/78), [#104](https://github.com/hherb/kastellan/issues/104), [#107](https://github.com/hherb/kastellan/issues/107), [#127](https://github.com/hherb/kastellan/issues/127), [#134](https://github.com/hherb/kastellan/issues/134).

---

## Design notes for parked work

**Option P — entity↔memory linkage + graph lane (Phase 1 cont.).** The `memory_entities` join table shipped and the production caller wiring is DONE (2026-05-19 Slice F, PR #91): `RouterAgent::formulate_plan` populates `seed_entity_ids` from `entity_extractor.extract(&ctx.instruction)` each iteration, and `main.rs` wires the real `GlinerRelexExtractor`. **The remaining parked work is the quarantine review gate, not the wiring:** freshly-extracted entities default `quarantine=TRUE` and `graph_search` filters `quarantine=FALSE`, so seed entities surface no memories until an operator un-quarantines them ([#40](https://github.com/hherb/kastellan/issues/40) tracks the policy question). Secondary: `entities.embedding` is NULL for all entities; populating it would seed an entity-similarity lane (the column already exists).

## Open questions parked for later

1. Embedding model on-device — bge-m3 vs nomic-embed-text vs ColBERT (Phase 1).
2. ~~Channel approval~~ **Resolved 2026-05-06:** pairing flow with WebAuthn-or-OTP fallback.
3. ~~Egress proxy separate worker vs in-process~~ **Resolved 2026-05-06:** separate worker, leak scanner co-located.
4. Skill review workflow for *named* agent-authored Python (Phase 4) — trust enum + per-level capability ceiling; the L3 arc is the first concrete implementation for templated tool-call skills.
5. Worker keep-alive vs spawn-per-call — idle-timeout lifecycle shipped for GLiNER-Relex; revisit for other workers when latency matters.
6. ~~Worker binary discovery / install convention~~ **Resolved 2026-06-20** (`kastellan-cli install`, PR #316). Residual: an FHS `libexec` / multi-user layout if packaging ever wants it.

## Inspirations / things to read before each milestone

Two adjacent OpenClaw-derived projects ship AGPL-compatible code worth reading before a new milestone:

- **ZeroClaw** ([`zeroclaw-labs/zeroclaw`](https://github.com/zeroclaw-labs/zeroclaw), 100 % Rust) — [`crates/zeroclaw-runtime/src/security/`](https://github.com/zeroclaw-labs/zeroclaw/tree/main/crates/zeroclaw-runtime/src/security) has working `bubblewrap.rs`, `landlock.rs`, `seatbelt.rs`, `pairing.rs`, `webauthn.rs`, `leak_detector.rs`. **Don't copy its in-process tool model** — tools run as in-process Rust traits with the OS sandbox around the whole runtime, a weaker boundary than process-per-worker.
- **IronClaw** ([`nearai/ironclaw`](https://github.com/nearai/ironclaw)) — read its dispatcher chokepoint pattern (`ToolDispatcher::dispatch()` as the single audit/safety funnel for every action). Drawbacks: WASM-as-boundary is software-only containment; the Postgres+libSQL dual backend is overkill at our stage.

The *defining* architectural difference: kastellan enforces **one OS process + one bwrap/Seatbelt jail per worker**. Both reference projects retreated from that. Don't.

**openworker** ([`andrewyng/openworker`](https://github.com/andrewyng/openworker), MIT — Python sidecar + Tauri shell) and its engine **aisuite** ([`andrewyng/aisuite`](https://github.com/andrewyng/aisuite)), surveyed 2026-08-14. **Read it for consent ergonomics, never for containment** — it has no OS sandbox at all (`coworker/tools/shell.py` runs on the host; isolation is path-scoping plus an in-process permission engine), so its threat model is strictly weaker than ours and taking its security architecture would be a regression. What it *has* done far more work on than kastellan is everything around an agent that runs while nobody is watching, which is our default posture and its edge case. Five modules earned ROADMAP entries — the ask channel (`coworker/inbox.py`, [#564](https://github.com/hherb/kastellan/issues/564), Phase 3), declared tool risk + operator-local overrides (`risk.py`/`overrides.py`, Phase 5), target-bound standing grants (`permissions.py`, Phase 5), auto-compaction (`compaction.py`, Phase 1 `context_manager`), and `SKILL.md` progressive disclosure (`skills/`, Phase 4). Two things they have that we already do **better**, so don't re-import them: aisuite's `artifact_store` message dehydration is a weaker `handoff.rs` (preview + ref, no range-fetch with `eof`), and their careful shell-metacharacter rejection before allowlist matching exists only because `run_shell` takes a command *string* — `shell-exec` takes an argv array and never invokes a shell, so the whole bug class is absent. The one refinement worth noting from that code: we allowlist `argv[0]` only, so `git status` and `git push` are the same permission; their `shlex`-parsed **token-prefix** match (`git status` matches `git status -s`, never `git statusfoo` or a bare `git`) is the right algorithm if sub-command granularity is ever wanted.

**Headlong** ([`laude-institute/headlong`](https://github.com/laude-institute/headlong), Apache-2.0 — <10K lines of Bash), surveyed 2026-08-27; full write-up in [`docs/devel/notes/2026-08-27-headlong-borrowings.md`](../notes/2026-08-27-headlong-borrowings.md). **Read it for memory, context and loop pacing; never for containment.** Its threat model is the inverse of ours and says so (`deploy/SECURITY.md`: the agent *"runs arbitrary bash on its box with its API keys"*, the box is *"dedicated and burnable"* with all outbound traffic allowed, and *"prompt injection is therefore always possible"*). Four of its defining features are things kastellan exists to refuse — Bash-as-the-only-tool, agent self-modification of its own harness, one shared multi-user mind, Docker-as-sandbox — so do not import them. Its `shellm-docker-broker` (host-side policy server, *"never present in the mind's environment"*) is convergent evidence that the dispatcher chokepoint is right; ours is stronger. What it has done far more work on than we have is **an agent that has lived for months**: [#628](https://github.com/hherb/kastellan/issues/628) (`trajectory_spec.md`'s *"writers stamp exact links; readers must not guess"* — the structural version of the #616/#619 fix) and [#629](https://github.com/hherb/kastellan/issues/629) (`tiered_memory.md`'s logarithmic rollup pyramid, which is what goes in the declared-but-unwritten `MemoryLayer::L4`) came out of it. Three more, not filed: `design/monolith_backoff.md`'s pacing table for whenever routines land — reactivity never throttled, spontaneity backing off geometrically with a dwell, and three bugs already paid for (an in-step sleep holds the worker slot; their `setsid` timer *silently never ran on macOS*; a thought-only run counting as "work" made a ruminating mind re-fire at full speed forever); `THINKERS_spec.md`'s framing that *"liveness is a dispatcher guarantee, not a property code paths must each preserve"*, which lands on our startup-only `crash_recovery::sweep_and_audit` — `runner::sweep_loop` already exists and already made that exact argument for ask deadlines, so moving the crash sweep into it is a few lines; and blob spilling (`stdout_ref` + `stdout_bytes` + `sha256`) as the optional second half of [#617](https://github.com/hherb/kastellan/issues/617), whose bounded `req_summary` is the load-bearing half and should ship first.

---

## How to update this document at session end

**Header first, prose last.** The header is what the next session treats as authoritative; stale header fields mislead silently even when the prose is right.

1. **Bump the header before writing any prose:** `Last updated:` → today; `main` HEAD → `git log --oneline -1`; `Last gate:` → the passed/failed/ignored/`[SKIP]` counts from a fresh `cargo test --workspace`. Then fix **every test count embedded elsewhere that changed** — a fresh agent greps for them and trusts whatever it finds.
2. **Move the picked TODO into [Recently merged](#recently-merged)** with enough detail (file paths, why-not-X, gotchas, count delta) to start cold, and update the [Test baseline](#test-baseline-authoritative) table.
3. **Write a fresh [Next TODO](#next-todo)** with options sized for one session each — file paths, gotchas, verification step.
4. **Refresh [Working state](#working-state)** — anything new, anything that became real.
5. **Tick the matching ROADMAP items** with the commit hash.
6. **Commit both files together** with a `docs(handover): …` message.
7. **If a milestone shipped:** does `site/roadmap.html` (timeline + "Last updated" stamp, and the landing-page status numbers) need a one-line update? See `site/README.md`.

### Pruning convention

This file stays focused on **what the next session must act on**: current state, the last 2–3 sessions, and the next TODO. Prune when it grows past what a fresh session can absorb cold — judge by *reading weight*, not line count; the 2026-08-03 prune was triggered at 546 lines / ~73 k tokens.

1. **Snapshot first** — copy to `archive/handover_<YYYYMMDD>[_<slug>].md`. The archive is the audit trail: never edited after the fact, never deleted.
2. **Keep:** the header, "Read these first", "Working state" (current truth), the most recent 1–2 sessions, "Key design decisions", "Next TODO", open issues, open questions, "Inspirations", and this section.
3. **Compress everything else** into one bullet per session, or into the archive pointer if it is no longer load-bearing.
4. **Cross-link** every compressed bullet to the archive snapshot.
5. **Commit the prune separately** (`docs(handover): prune older sessions, archive pre-prune snapshot`) so the diff is reviewable.

The archive directory is the historical record; this file is the working brief.
