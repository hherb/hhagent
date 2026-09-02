# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260826_624_pre-prune.md`](archive/handover_20260826_624_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here,
> including the full #619, #615/#616/#618 and live-bring-up write-ups compressed below.

**Last updated:** 2026-09-02 · **`main` HEAD:** `466ca7ff` —
[#640](https://github.com/hherb/kastellan/pull/640) squash-merged (#632 the `fastest_tok_per_s`
rename, #634 the daemon bring-up harness lift), on top of `44e0f38d`
([#637](https://github.com/hherb/kastellan/pull/637), #626). ·
**OPEN BRANCH: `fix/641-642-643-daemonspec-and-service-name`** — the three follow-ups #640's own
review filed: [#642](https://github.com/hherb/kastellan/issues/642) (one shared
`validate_service_name`), [#641](https://github.com/hherb/kastellan/issues/641) (`DaemonSpec::new`
down to three non-transposable parameters) and
[#643](https://github.com/hherb/kastellan/issues/643) (one `ReportedRates` mapping for all three
guard reporting sites), plus a movement-only `LlmEndpoint` split. **[PR #645](https://github.com/hherb/kastellan/pull/645), open.**

**Last gate: DGX over `main` at `466ca7ff` — 3928 / 0 / 55, 176 suites, `TEST_EXIT=0`**; cold clippy
exit 0, **345** `Checking`+`Compiling` lines over 330 distinct crates with all **27** kastellan
crates named and zero warnings. 8 `[SKIP]`, all gliner-relex — *not* the bwrap-userns skip, so
containment really ran. Both hosts on rustc **1.98.0** (CI parity, re-checked; `rustup check` says
stable is up to date). See [Test baseline](#test-baseline-authoritative).

> **The 3928 reconciles exactly as 3921 + 1 + 6, and the handover's predicted 3927 was wrong by
> one.** The prediction counted only the six new `tests-common` unit tests; the same review-fix
> commit also added `boot_report::tests::the_durable_wire_key_did_not_follow_the_rust_field_rename`
> to `kastellan_core`. Reconciled by diffing **per-suite pass counts** between the two gate logs,
> not by name: `--nocapture` interleaves output, so a `test … ok` name grep loses ~19 lines and
> invents "removed" tests that are merely mangled. Pair each `Running <binary>` header with the
> `test result:` line that follows it.

> ⚠️ **#640 merged WITHOUT the DGX re-gate the last handover demanded.** It merged as `466ca7ff` at
> 2026-09-01 21:18Z; the gate above is that owed gate, run a day late against `main`. Nothing was
> wrong — but the sequence to avoid repeating is "gate required before merge" followed by a merge.

> ⚠️ **A slow Mac cargo build is CONTENTION, not the `_dyld_start` wedge — and I misdiagnosed it
> this session.** A `cargo test -p kastellan-supervisor --lib` that sat ~25 minutes with no visible
> progress, plus `sample` on a `build-script-build` showing a single `_dyld_start` frame, looked
> exactly like the known macOS hazard below. It was not. The operator confirmed the Mac builds fine,
> and re-running proved it: **115 / 0**, and the timing is the tell —
> `6.89s user, 31.62s system, 4% cpu, 13:34.55 total`.
>
> **The two states are not distinguishable by `sample` alone**, which is the trap: a thread that is
> never *scheduled* has the same one-frame `_dyld_start` stack as one dyld has genuinely wedged,
> because `sample` reports where the stack is, not why it is not moving. What separates them is
> **load, not stacks**:
>
> * `uptime` — this box was at **load average 22.68** with a *second* project's `cargo` running 16
>   `rustc` processes on a different toolchain (1.96.0, while ours is `stable`);
> * `%cpu` in `time` output — **4%** means starved; a real dyld wedge burns no CPU *and* never
>   finishes, whereas this finished.
>
> **Check `uptime` and the toolchain mix BEFORE concluding a wedge**, and prefer `time` over
> patience. The hazard below is real and has cost a session; it is just not the first hypothesis for
> "slow", and I anchored on it because this file named it.

---

## Current state

### #641 + #642 + #643 — DONE on `fix/641-642-643-daemonspec-and-service-name`

The three follow-ups [#640](https://github.com/hherb/kastellan/pull/640)'s review filed. All three
are about the same failure mode — **a same-typed neighbour that can be transposed in silence** —
attacked at three different layers, which is why they belong in one branch.

**#642 — one `validate_service_name`, un-`cfg`'d at the supervisor crate root.**
`MAX_NAME_LEN` was a private `const` in *each* backend and the predicate a character-identical copy
behind each platform `cfg`.

- **Neither host ever ran the other's copy**, so "identical" was a belief, not a checked property —
  and the tree's stated contract is that one service name is portable to either OS *without* a
  rename step. A per-platform gate cannot state that contract; it can only be it, one platform at a
  time. Both backends now `pub use` the shared predicate, so
  `systemd_user::validate_service_name` and `launchd_agents::validate_service_name` keep their
  public paths, and both keep their install-level `InvalidName` tests, so the re-export stays
  exercised end to end.
- **The cap is deliberately NOT re-exported by the backends.** `builder`/`builders` are private
  modules, so a `pub use` of a constant nothing there names is just an unused import — which the
  DGX caught as an `-D warnings` failure on the systemd side. The launchd copy was identical, and
  the **Mac clippy leg confirmed it**: the same fix, machine-checked on the platform that compiles
  it, rather than argued from symmetry.
- **Two tests the copies never had**, both from the tree's own lessons: the cap asserted in **both**
  directions (`MAX_NAME_LEN` *and* `+1` — the copies only checked the reject side, so `>` mutated
  to `>=` passed all of them), and the cap pinned to a **literal** rather than to itself
  [[audit-sink-doubles-hide-storage-transforms]].
- **What the hand-copy in `tests-common` actually missed:** it checked the half that essentially
  cannot fire (a real name is ~60 chars, so tripping 200 needs a 140-char label — hence the
  `"x".repeat(250)` the test had to use) and skipped the half that can. A label with a space or a
  `/` sailed past it and died much later inside `install`, naming a *service name* rather than the
  label that produced it.
- **⚠️ #642 undercounted: it was the third, fourth AND fifth copy.**
  [#646](https://github.com/hherb/kastellan/issues/646) records the two still hand-rolled —
  `tests-common/src/pg.rs:207` and `core/tests/supervisor_e2e.rs:143`, both guarding a name that is
  installed as a real unit. **Not folded into this branch deliberately:** the shared predicate is
  *stricter*, and `bring_up_pg_cluster` has ~200 call sites, a good number building the name from a
  `{label}`/`{tag}`/`{test_label}` variable. Tightening without auditing every one risks turning a
  passing test into a panic, and folding an unaudited change in would have put it behind an
  already-green DGX gate. The `supervisor_e2e` one is a single call site and can go first.

**#641 — `DaemonSpec::new(label, data_dir, llm)`: three parameters, no two the same type.**
It took five, of which `label`, `suffix` and `user` were all `impl Into<String>`.

- **The apparent barrier was accidental.** `data_dir` sitting between them only blocks a swap while
  callers pass a `Path`-typed value; `impl Into<PathBuf>` accepts a `&str` just as happily.
- **Deleting beat newtyping** because `suffix` and `user` were the *same expression* at all six call
  sites (`unique_suffix()` / `current_username()`), so deriving them loses no choice any caller was
  making. **No `.suffix()`/`.user()` setters were added** — an unused setter is a hatch that
  re-opens what the signature closes.
- **The name is now validated at construction** against the supervisor's own predicate, replacing
  the hand-rolled `len() <= 200` in `service_spec`. That buys the charset half, cannot drift from
  what `install` applies, names the wrong `label` at the line that supplied it, and covers
  `service_name()` — which is `pub`, and which a check living only in `service_spec` left unguarded.
- **⚠️ `new` is now the one function in the module that reads the environment**, eagerly and once,
  precisely so `service_spec` stays a pure function of stored data. The module doc's blanket "Pure
  throughout" was corrected rather than left to quietly become false.
- **⚠️ One property was given up, deliberately:** the unit's suffix is no longer the same string as
  its sibling Postgres cluster's, because each is now drawn separately. Nothing reads that
  correspondence today; if [#548](https://github.com/hherb/kastellan/issues/548)'s stale-unit sweep
  ever wants to correlate a leaked unit with its leaked cluster, a `.suffix()` setter restores it.
  Recorded here because an issue-as-filed can carry a cost it did not price
  [[issue-as-filed-can-carry-a-regression]].
- **The test changes are consequences, not cosmetics.** `base_spec()` now returns a different name
  per call, so the one test that pinned a whole literal name split into three properties that are
  actually true: the name carries prefix-then-label, one spec keeps one name, and **two specs with
  the same label do not collide** — the whole reason a suffix exists, which no test had ever stated
  while the caller supplied it.
- **A mutant found by inventory, not by diff** [[mutation-proof-counts-only-mutants-you-tried]]:
  `validate_service_name(&spec.label)` instead of `&spec.service_name()` survives **both**
  `#[should_panic]` tests, since a 250-char label and a label with a space are each illegal on their
  own. Killed by `the_cap_is_applied_to_the_whole_name_not_just_the_label`, which uses a **163-char**
  label — legal in isolation (asserted in the test body, or it proves nothing), illegal once the
  31-char prefix and ~28-char suffix are added.
- Also added: the **accepting** arm (`a_legal_label_constructs`), without which a mutant that panics
  unconditionally in `new` passes both `#[should_panic]` tests
  [[unreachable-success-path-proves-nothing]]. And `USER` is cross-checked against `$USER` rather
  than against `current_username()`, which would put the same helper on both sides.

**#643 — one `ReportedRates` mapping, shared by all three guard reporting sites.**
The same four probe numbers reached an operator through the `info!` line, the `warn!` finding and
the durable row, and **each renamed them by hand**. Only the payload copy was guarded; `tracing`
fields cannot be read back without a subscriber, so no test in the tree could have caught a swap in
the other two.

- **The swap does not produce a visibly wrong row.** Since `slowest <= fastest` always holds, a
  transposed pair reports a **contended** boot as a quiet one — exactly the diagnostic #624 was
  filed to make visible, silenced by the line meant to carry it, and `main.rs`'s own comment calls
  the `warn!` "the line an operator actually reads".
- **Not introduced by #632, but made legible by it:** the line used to read
  `tok_per_s = rates.tok_per_s`, where a transposition was visible on its face.
- **The struct beat the subscriber test**, which is the option #643 listed first: a subscriber test
  would *detect* a divergence between three sites; moving the mapping leaves no second site to
  diverge. `tracing` cannot spread a struct, so each macro still names its fields — but the
  right-hand sides are now name-for-name identity, which restores #632's lost self-evidence
  **without giving the rename back**.
- Its **own module** rather than an addition to `boot_report/tests.rs`, which is 686 lines and
  already over cap. Five tests, every fixture using **four distinct values** because any fixture
  where two are equal lets the swap through; the numbers are #624's real DGX boots (6 073 / 269.6,
  a 22.5x spread) so a reader can see what a swapped pair would claim.

**Plus a movement-only split.** `spec.rs` hit 538, and the last handover had already called it
("the next addition to either should split rather than append") — #641 *was* that addition.
`LlmEndpoint` lifted to `spec/llm_endpoint.rs`: **538 → 438**, character-for-character except a
module header and `fn url` → `pub(super) fn url` (its only caller now lives one level up). The
`pub use` keeps every path resolving, so no caller and no test changed.
**`spec/tests.rs` is 599 and stays over cap** — splitting it means deciding whether the endpoint
cases belong with the type or with the spec that wires it, which is a judgement rather than a
movement, so it went to the file-split backlog instead of being folded in here.

**Still open, and unchanged by this branch:** [#644](https://github.com/hherb/kastellan/issues/644)
(the launchd duplicate-plist-key question for every *other* `ServiceSpec` producer — `tests-common`
itself is safe, since `service_spec` collapses duplicates last-wins).

### #632 + #634 — MERGED `466ca7ff` ([#640](https://github.com/hherb/kastellan/pull/640))

Full prose in
[`archive/handover_20260902_641_642_643_pre-prune.md`](archive/handover_20260902_641_642_643_pre-prune.md).
Kept here only for what still binds:

- **The REPORTING vocabulary is frozen at `tok_per_s`** — the durable `guard_tier.boot` key cannot
  move (live rows carry it; the operator query `slowest_tok_per_s < tok_per_s / 2` is written
  against it), and a log line naming this number differently from the row it accompanies would read
  as a second measurement. #643 above is what makes that freeze cost nothing.
- **A blind `sed` would have broken production.** `\btok_per_s\b` does not match inside
  `slowest_tok_per_s` but **does** match `"tok_per_s"`, so the naive regex renames the durable key.
  `CONFIGURED_KEYS` is what makes that a test failure rather than a silent one.
- **#634's two divergences the issue's table missed**, both found by reading the copies rather than
  the issue: `observation_capture`'s 15 s readiness budget (so the real spread was **three** values,
  not two), and its verbatim `KASTELLAN_LLM_LOCAL_URL`, where the shared helper's unconditional
  append would have dialled `/v1/v1`.
- **The first fix for that was itself a regression** and the review caught it: a bare `Verbatim`
  narrowed an operator variable that a `strip_suffix`+append pair had been *normalising*. Corrected
  by `LlmEndpoint::from_operator_url`, which classifies rather than assumes. The general lesson:
  **making a distinction representable is not the same as making the wrong side of it unreachable**,
  and a type that only names the caller's promise still lets the caller promise wrongly.
- **`extra_env` later-wins is a property, not a comment** — and `service_spec` now *collapses*
  duplicates rather than relying on the renderer, because systemd documents last-wins for a repeated
  assignment while launchd gets a plist dict with a **duplicate key**, whose resolution the format
  does not define. A containment control (`force_routing(false)`) rested on a belief about
  `CFPropertyList` [[handover-claims-verify-before-carrying]].
- **A deletion mutant is weaker than a transposition one.** Deleting the `extra_env` extend killed
  two tests; *moving it before the common keys* — the actual defect shape — killed exactly one.

### #626, #633, #627 — merged, compressed

Full prose in the [`archive/`](archive/) snapshots. What still binds:

- **#626 (`44e0f38d`)** — `PROBE_TOTAL_BUDGET_MS` equalled `PROBE_BUDGET_MS`, so any saturating
  sample ended the probe at one measurement. Fix is one constant, `2 * PROBE_BUDGET_MS`; nothing
  special-cases saturation and nothing should. **The budget relation is a compile-time assertion
  beside the constants**, and it was `>` inside `#[cfg(test)] mod tests` — both wrong, since
  `PROBE_BUDGET_MS + 1` passed the `>` guard while still refusing the second sample, and a
  `cfg(test)` `const _` is stripped from `--release`
  [[cfg-test-const-assert-is-not-a-release-guard]]. **`TimeoutBasis::Saturated` does NOT mean every
  sample stalled** — a row saying `attempted_samples: 1` is pre-#626, or a bug.
  `scripts/upgrade_from_git.sh`'s `CHANNEL_WAIT` is **120**, not 45.
- **#633 (`d3f8ed3f`)** — **the premise that kept it open was FALSE**, and correcting it was most of
  the work: `from_router_config` skips the probe entirely under a pinned timeout, so the configured
  arm needs only a mock answering `/props`. The gap was real; documenting it as *unclosable* was the
  defect [[handover-claims-verify-before-carrying]]. **Literal assertions must sit beside a
  structural equality** — equality puts `boot_payload` on both sides. **An UNDER-POWERED mutant is
  indistinguishable from a blind test in the result column.**
- **#627 (`8040ca83`)** — `boot_report` extracted as a pure module. **`boot_payload` takes `tau` +
  `n_ctx` as scalars, NOT a `&GuardTier` — and that IS the fix**, since `GuardTier`'s only
  constructor has a fatal `/props` dependency [[unreachable-success-path-proves-nothing]]. **A rate
  swap SILENCES the documented operator query, it does not invert it**: since `slowest <= fastest`,
  a swapped row asks `fastest < slowest / 2`, the empty set on every host, forever.
- **#624 (`4aee83ad`)** — the probe measured the BOOT, not the host: 6 073 / 269.6 / 1 582 tok/s on
  three consecutive boots of one unchanged backend, a **26x** under-measurement whose slowest boot
  fired a **false** ceiling finding. Keep the **FASTEST** sample, because prompt processing has a
  hardware ceiling and no floor, so a mean is wrong for a one-sided error. **Each sample carries its
  OWN cache-buster.** The review's CRITICAL and the rule it left: **when a fix's value lives in a
  fold, pin the fold's *inputs*, not just its output shape.**
- **#619 (`3bd45a36`)** — `classify_transport` folded a **connect timeout** into `Timeout`, sending
  an operator to #612's ~350 s pin, which cannot help (connect is capped at `min(timeout, 5 s)`).
  **The honest whole-fail-open query is `state NOT IN ('clear','block')`, not `error_kind IS NULL`.**
- **#615/#616/#618 (`e258ad3c`)** — `guard.error_kind` is a **closed discriminant** beside
  `guard.state`; `TimeoutBasis::Operator` carries a `PinBand` (an **in-band** pin keeps the historic
  `"operator"` token — use `LIKE 'operator%'`). **#616 is what unblocked #612's favoured option.**

> ⚠️ **#624 and #626 do NOT close [#612](https://github.com/hherb/kastellan/issues/612), and merging
> them is the mistake to avoid.** #624 removed the *contention* error from the sample; #612 is that
> extrapolating from a ~1 KiB sample is non-linear **on Metal whatever the load** — a quiet Mac
> still reads 1 137 tok/s at 1 KiB and 260 at 64 KiB [[metal-prompt-processing-is-nonlinear]]. Both
> point at the same remedy: measure from the `ms` / `body_byte_len` the guard rows carry since #616.

> ⚠️ **#614's merge wrongly CLOSED #612 and #615** via "Filed, **not fixed**: #N". See
> [Standing hazards](#standing-hazards-that-have-each-cost-a-session).

### The guard-model arc and older merged work — compressed

Full prose in the [`archive/`](archive/) snapshots. What still binds:

- **`AuditSink::insert` is a provided method applying `truncate_payload` before delegating to
  `insert_stored`**, so no sink double can record a payload Postgres never stored
  [[audit-sink-doubles-hide-storage-transforms]]. Round one had kept half the defect by dropping an
  unaffordable preserved key *silently* — **absence and loss must not render identically**.
- **The stated mitigation for an issue can disarm the instrument built to check it** — the live
  probe passed having measured nothing under a *pinned* timeout, precisely the configuration #612
  tells a Metal operator to use. It now refuses a pin outright.
- **D10 — the tier is ADVISORY defence-in-depth, NOT a gate.** 65% recall (36/55) at FP-0; 6/6 on
  bare imperatives but **5/8 missed** on narrative framing. **Nothing downstream may relax on it.**
- **τ = 0.79552656 is a REQUIRED operator input with no default**, and **five misconfigurations STOP
  THE DAEMON** (D6): half-configured keys, τ outside `(0.0, 1.0]`, a pinned timeout of 0, an
  unreachable `/props`, and a context below `SCAN_BYTE_CAP + 512 = 66 048` (D8).
  `KASTELLAN_REQUIRE_GUARD=1` makes the *unconfigured* case fatal too.
- **Measurement 3 ([#606](https://github.com/hherb/kastellan/pull/606))** — 133 cases, FP-0 on both
  hosts. `best_tau` returns **NONE**: real captured content overlaps at every threshold. Its
  security-prose stratum was **catalogue-selected**, which is why **corpus growth from production is
  now the cheap path** — harvest it before designing another campaign.
- **`RouterConfig` lost its `Eq` derive** — `guard_tau: Option<f32>` can hold a NaN.
- **The other four `screen` call sites** (`fetch_screen`, `inner_loop/summary`, `channel/ingest`,
  `recall_assembly/pg_builder`) keep catalogue-only behaviour, as does the core-initiated
  `gliner-relex` dispatch. Widening is a separate slice with its own blast radius.
- **[#585](https://github.com/hherb/kastellan/pull/585) `f90631da` — guard slice 1.** Two findings
  overturned the feasibility study and must not be re-derived from it: its `0.45–0.70` band holds
  exactly one reachable value, and `observation replay` is plan-level and cannot score a
  document-level tier. Best review catch, and it generalises: *a mock that does not return what it
  was sent tests only your own canned response*.
- **[#579](https://github.com/hherb/kastellan/pull/579) `bb937df7` — #564 slice 2.** D16's
  peer-scoped `EXISTS` inside the guarded UPDATE (**the nonce is a BEARER token — reading, not
  guessing, was the real threat**). Its five-agent review found eight things nine per-task reviews
  and 3522 tests had missed, all on the **argument-passing seams between layers**.
- **[#578](https://github.com/hherb/kastellan/pull/578) `af3e7e66` — #564 slice 1b.** **D11**
  (`asks.resume_state`, migration 0024), because a resumed task otherwise re-executed steps it had
  already run — approve a plan and an earlier step's email goes out twice.
- **[#572](https://github.com/hherb/kastellan/pull/572)/[#573](https://github.com/hherb/kastellan/pull/573)
  `fbe91c4d`+`e8ea4339`** — **a mutation score is only as good as the mutation set**: a reviewer's
  own 15 mutations left **11 surviving** with all 113 tests green.
- **[#569](https://github.com/hherb/kastellan/pull/569) `07b6451e`** — runtime and quantisation
  **PINNED**: llama.cpp + `Shieldstral-1.0-3B-Q8_0` on both hosts, so one fitted τ transfers.
- **The four faults (2026-08-02).** One real Matrix message, **four independent faults, only one a
  kastellan bug in the layer everyone suspected**, each masking the next. The durable lesson is the
  shape: a green stack with a silent output means look at every layer, and fix them one at a time so
  each fix's evidence is separable.

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
>
> ⚠️ **UPDATED 2026-09-02 — the `sample` signature above is NOT sufficient on its own, and treating it as conclusive cost this session a wrong diagnosis in five documents.** A thread that is merely never *scheduled* shows the same single `_dyld_start` frame, because `sample` reports where a stack is, not why it is not moving. On a box at **load average 22.68** with another project's `cargo` running 16 `rustc` processes, a `kastellan-supervisor --lib` run took **13m34s wall at 4% cpu** and then **passed**. Check `uptime` and `%cpu` first: a wedge burns no CPU *and never finishes*; contention burns little CPU and finishes. Only after ruling out load is the one-frame stack evidence of anything.

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

**FIRST: get [PR #645](https://github.com/hherb/kastellan/pull/645) reviewed and merged.** Both
hosts are green over the whole branch and **nothing is outstanding** — DGX 3940 / 0 / 55 plus cold
clippy, and the macOS leg (`kastellan-supervisor --lib` 115 / 0, `clippy --all-targets -D warnings`
exit 0) covering the `cfg(target_os = "macos")` `launchd_agents` code the DGX compiles out.

**THEN: the DGX redeploy, which has now been the "first unblocked action" for two sessions running
and is the oldest thing on this list.** The whole guard arc —
`4aee83ad` / `8040ca83` / `d3f8ed3f` / `44e0f38d` / `466ca7ff` — is on `main` and **not deployed**.
Until it is, no live `guard_tier.boot` row carries `slowest_tok_per_s` / `measured_samples` /
`attempted_samples`, and that is expected rather than a gap. When it happens: `install`
REGENERATES `kastellan.env` and silently reverts tuned values, so re-add the four keys and repair
the model tag afterwards [[dgx-deploy-env-clobber-and-missing-workers]]; then verify at the
**installed binary** with `strings`, never at the checkout
[[handover-claims-verify-before-carrying]]. The first cold-backend boot after it is also the first
chance to watch #626's retry on a real stalled `/v1/chat/completions`.

**Then the guard arc's remaining work is one item and it is the one that matters:**
[#612](https://github.com/hherb/kastellan/issues/612), a design call rather than a patch — **#616
unblocked its favoured option**, so it is now reachable rather than merely filed. Read the
measurement in the issue before proposing a fix; every cheap one is closed off there. Beside it,
both cheap: [#639](https://github.com/hherb/kastellan/issues/639) (split `guard_tier_e2e.rs`, 1558
lines, also [#622](https://github.com/hherb/kastellan/issues/622)'s cheapest option — the probe half
would then fit a CI gate with no Postgres service container) and
[#638](https://github.com/hherb/kastellan/issues/638) (214 rustdoc warnings, 67 of them broken
intra-doc links, in a tree that treats doc comments as the design record).

**Next up — operator's choice, each roughly one session.** Full issue text is authoritative; these
are the gotchas that are *not* in the issues.

- **[#560](https://github.com/hherb/kastellan/issues/560) — the planner fabricates a 16-hex
  `message_id`.** Do **not** close it by rewriting the parameter description: #536 already did
  exactly that ("not a placeholder"), deployed 2026-08-09, and both later runs still fabricated. The
  lead worth measuring: with keys stripped by `extract_scannable_text`, `"20973"` reaches the planner
  as a bare line among subjects and dates, with nothing marking it as *the id*
  [[tool-output-reaches-planner-key-stripped]] [[opaque-ids-are-unusable-tool-params]].
- **[#550](https://github.com/hherb/kastellan/issues/550) — the *generated* `kastellan.env` gets no
  end-to-end check.** #531 verifies the optional overlay most hosts do not have and skips the
  required file every host does. **The naive fix is wrong** — the overlay legitimately overrides
  `kastellan.env` keys, so per-file comparison false-positives; it must compare the *folded*
  environment, which `fold_env_files` already computes for launchd.
- **[#551](https://github.com/hherb/kastellan/issues/551) — no path directive escapes systemd's `%`
  specifier.** Pre-existing and workspace-wide (`ExecStart=`, `Environment=`, not just
  `EnvironmentFile=`). Measure first, then either escape `%%` or reject at install.
- **[#548](https://github.com/hherb/kastellan/issues/548) — PG e2e tests install units into the
  operator's *real* `~/.config/systemd/user/`.** Not a teardown bug — `PgCluster`'s `Drop` guards are
  correct and simply cannot run on SIGKILL — so the fix is about blast radius. **Confirmed still
  accruing 2026-09-01**, and it is a slow leak rather than one historical accident: the DGX carries
  units from two *different* tests, 2026-06-21 and a `failed`
  `kastellan-test-seccli-1-726614-…`. `systemctl --user list-units --type=service --all | grep -i
  kastellan` shows them. ⚠️ **#641 removed the shared suffix between a test daemon's unit and its
  sibling PG cluster**, so a sweep can no longer correlate the two; if that matters, restore it with
  a `.suffix()` setter rather than by reverting the constructor.
- **[#519](https://github.com/hherb/kastellan/issues/519), [#554](https://github.com/hherb/kastellan/issues/554),
  [#534](https://github.com/hherb/kastellan/issues/534)** — see
  [Open follow-up issues](#open-follow-up-issues-filed-but-not-picked); each is a design call, and
  #554 needs a live DGX gate because it narrows what a deployed worker may do.
- **[#564](https://github.com/hherb/kastellan/issues/564)** — slices 1a, 1b and 2 are all MERGED.
  What remains under that heading is non-blocking.
- **Email channel — slices 2 and 3.** Slice 1 (gated inbound) MERGED, #503 closed its MITM gap. Spec
  `docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`. **Slice 2** = SMTP outbound
  (`lettre`, MIT-verified) + full round trip; today `EmailChannel::send` refuses and every refusal is
  audited `channel.reply_undelivered`. **Slice 3** = DGX deploy + live tier; **restart
  `localmail-serve` (+ `localmail-daemon`) on the DGX first**. Its deploy blocker is gone (`41b21f36`
  installs `kastellan-worker-email-in`).
- **A Mac daemon deployment is a deliberate decision, not a task.** The tier boots fine there (91.4 s
  derived, `n_ctx` 66 048) but #612 means it fails open on large documents. Decide #612 first, or
  deploy with a pinned timeout and say so.
- **Live guard-host facts** (verified 2026-08-23): the DGX guard server is `llama-server …
  Shieldstral-1.0-3B-Q8_0.gguf --alias shieldstral --port 8081 -c 131072 -ngl 99`; `/props` reports
  the per-request context at `default_generation_settings.n_ctx` with **no top-level `n_ctx`**.
  Restart it with **at least `-c 66048`** or the daemon refuses to boot. The three guard keys live in
  `~/.config/kastellan/kastellan.env.local`, which `install` never rewrites.
- **Corpus growth is now cheap, and that is new.** D5's per-dispatch `p` is live and survives on
  large documents since the audit-cap fix, so production is finally a score source with no catalogue
  selection in it. Harvest it before designing another capture campaign.
- **Deferred with a reason, not forgotten:** macOS Seatbelt-loopback verification of mail tier 1a
  (needs a Mac run with working launchd-PG); **Telegram inbound** (still rejected as primary — no bot
  E2E, centralized, ban risk); **MITM-of-browser** via a proper NSS trust-store import, **not**
  `--ignore-certificate-errors-*`, since production must not be loosened to make a test pass.


- **File-split backlog (Item 9b)** — **re-`wc -l` before picking; the numbers drift and this list is
  a pointer, not a census.** The rule the tree follows, and the reason this list keeps growing rather
  than shrinking: **split BEFORE the change that grows a file**, in a movement-only commit whose
  `#[test]` name set is verifiable either side, so the movement diff is reviewable on its own.
  Folding a move in afterwards is the worst of both. `timeout.rs` (four files, 27 tests before and
  after), `tier/boot.rs` → `tier/probe.rs`, and `boot_supervisor/tests.rs` are the worked examples;
  `boot_report/tests.rs` (now **686**) is the counter-example.
  - **Newly over cap, from this session:** `tests-common/src/daemon/spec/tests.rs` **599**. Its
    production half *was* split (`spec.rs` 538 → 438, `LlmEndpoint` lifted to
    `spec/llm_endpoint.rs`); the test half was not, because the seam is a judgement rather than a
    movement — the five `LlmEndpoint` cases mostly assert *through* a built `DaemonSpec`, so moving
    them means first deciding whether they belong with the type (making `url()` `pub(crate)` and
    testing it directly) or with the spec that wires it. Decide that before splitting, not during.
  - **Best first picks, each a pure test-lift** (production code untouched, count verifiable either
    side): `core/src/channel/ask_message.rs` **956** (~330 production, ~620 test),
    `workers/mail/src/handler.rs` **670** (~305 production),
    `sandbox/src/linux_firecracker/plan.rs` ~**1160** (~485 production; `cfg(linux)`, so DGX-gated),
    and `core/tests/guard_tier_e2e.rs` **1558** (now [#639](https://github.com/hherb/kastellan/issues/639)), whose ~200-line multi-request HTTP mock lifts to
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
| **DGX** (native aarch64, real bwrap + KVM + live PG 18) | **`466ca7ff`** — merged `main`, #632 + #634 (PR #640) | **3928 / 0 / 55**, **176** suites, `TEST_EXIT=0`, `--no-fail-fast --nocapture`. **Reconciles exactly: 3921 + 1 + 6.** The `+6` are the review wave's `daemon::spec::tests::`; the `+1` is `boot_report::tests::the_durable_wire_key_did_not_follow_the_rust_field_rename`, which the previous handover's predicted 3927 had missed. ⚠️ **Reconciled by diffing PER-SUITE pass counts, not test names** — `--nocapture` interleaves output, so a `test … ok` name grep captured only 3909 of 3928 and invented six "removed" tests that were merely mangled. Pair each `Running <binary>` header with the `test result:` line after it. `kastellan_tests_common` **114** (Mac 116 = +2 `cfg(macos)` `serial::tests`), `kastellan_core` lib **1981** | exit 0 from a **cold** private dir under `$HOME` (`rm -rf`'d first): **345** `Checking`+`Compiling` lines, **330** distinct crates, **all 27** kastellan crates named, **zero** `warning`/`error`. 345 matches the `8d92c02b`/`c0255cd7`/`d3f8ed3f`/`553ec6ff` cold runs — that match is what says it was a real full-workspace lint rather than a cached pass. rustc **1.98.0** | **8**, all gliner-relex (4 venv-shim, 4 `ENABLE != "1"`) — **zero** non-gliner, *not* the bwrap-userns skip, so containment really ran |
| **DGX** (native aarch64, real bwrap + KVM + live PG 18) | **branch tip of `fix/641-642-643-daemonspec-and-service-name`** — #641 + #642 + #643 + the `LlmEndpoint` movement | **3940 / 0 / 55**, **176** suites, `TEST_EXIT=0`, `--no-fail-fast --nocapture`. **Reconciles exactly: 3928 + 12**, and all three deltas were *measured* by diffing per-suite counts rather than subtracted — `kastellan_core` 1981 → **1986** (the five `reported::tests`), `kastellan_supervisor` 115 → **117** (eight new `service_name::tests` minus the six duplicated systemd validator tests they replace), `kastellan_tests_common` 114 → **119** (five new `daemon::spec::tests`). All fourteen new tests observed running; the four `#[should_panic]` ones print `- should panic ... ok`, so a bare `… ok` grep reports them missing. **Ten mutants, ten killed**, each by the test written for it — including `validate_service_name(&spec.label)` instead of `&spec.service_name()`, which survives both `#[should_panic]` cap tests and was found by inventorying the new API rather than by reading the diff. `git diff --cached --stat` empty afterwards, the only proof index == tree [[mutation-testing-contaminates-the-index]] | exit 0 from a **cold** private dir under `$HOME`: **345** `Checking`+`Compiling` lines, **330** distinct crates, **all 27** kastellan crates, **zero** `warning`/`error`. rustc **1.98.0** | **8**, all gliner-relex — **zero** non-gliner |
| **Mac** (aarch64 darwin) | **branch tip** — the macOS half of #642 | `kastellan-supervisor --lib` **115 / 0**, `TEST_EXIT=0`. **All 8 `service_name::tests` observed running here as well as on the DGX**, which is the entire point of #642: the rule set is one set now and both hosts execute it. Platform split confirmed rather than assumed — **38 `launchd_agents` tests, 0 `systemd_user`** (the exact mirror of the DGX) [[mac-compiles-zero-systemd-tests]]. 113 → 115 is the same **+2** the DGX saw (8 new minus the 6 duplicated ones they replace). ⚠️ The first attempt was captured through `tail -12`, so a `service_name` grep over it found nothing and briefly looked like the tests had not run — [[truncated-gate-log-is-not-a-gate]], walked into again | `clippy -p kastellan-supervisor --all-targets -D warnings` **exit 0**, zero warnings — this is the leg that covers `cfg(target_os = "macos")` `launchd_agents`, invisible to the DGX | — |
Older rows (`553ec6ff` DGX 3921, `6764d272` DGX 3910, `8d92c02b` DGX 3910, `c0255cd7` DGX 3909, `d3f8ed3f` DGX 3908, `12809297` DGX 3901, `33029e32` DGX 3900, `020b0e53` Mac 3778, `b65e44ab` DGX 3890, the `fix/615-616-618-guard-diagnostics` Mac 3748, `8cb8cfb7` DGX 3854, `09c6231f` 3840/3718, `69834357` 3823, `0bae6b2c` 3759, `f46c67cf` 3749, `2ab6612c` 3686, `b58edc77` 3668, and 3047 back to 2950) are in the [`archive/`](archive/) snapshots — most recently [`handover_20260830_633_pre-prune.md`](archive/handover_20260830_633_pre-prune.md) § Test baseline.

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

- **`466ca7ff`** ([#640](https://github.com/hherb/kastellan/pull/640)) — #632 (the
  `fastest_tok_per_s` rename) + #634 (the daemon bring-up harness lifted out of three copies).
  See [Current state](#current-state).
- **`44e0f38d`** ([#637](https://github.com/hherb/kastellan/pull/637)) — #626, the
  saturating-first-sample defect.
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

Beyond those under [Next TODO](#next-todo). Only currently-open issues; closed-issue detail lives in
the [`archive/`](archive/) snapshots and git history. **The one-line summaries here are pointers —
read the issue before acting, since several carry measurements that close off the obvious fix.**

**From the #640 review (this session's branch fixes #641/#642/#643):**
[#644](https://github.com/hherb/kastellan/issues/644) — a duplicate `ServiceSpec.env` key renders as
a duplicate launchd plist dict key, whose resolution the format does not define. `tests-common` is
safe (it collapses last-wins); this is the general case for every *other* producer.

**Guard model / measurement:** [#605](https://github.com/hherb/kastellan/issues/605) (the
`PROVISIONAL` banner is unconditional — until it lands no report can say a τ is fitted);
[#602](https://github.com/hherb/kastellan/issues/602) (an empty body pinned as the page — fail-**open**
under `--record`); [#601](https://github.com/hherb/kastellan/issues/601) (`capture` admits under
`Relaxed`, `calibrate` excludes under `Strict`; quantified **inert** for this run but still wrong);
[#603](https://github.com/hherb/kastellan/issues/603) (the URL inside the hash);
[#599](https://github.com/hherb/kastellan/issues/599)/[#600](https://github.com/hherb/kastellan/issues/600);
[#608](https://github.com/hherb/kastellan/issues/608)–[#611](https://github.com/hherb/kastellan/issues/611);
[#597](https://github.com/hherb/kastellan/issues/597) (the two hosts hold different *projectors*;
inert while the tier runs `vision:false`).
**#604 is addressed, not closed** — D8 makes the 400 unreachable on a correctly sized host; it does
not make it unrepresentable.
[#617](https://github.com/hherb/kastellan/issues/617) is the big one of that family: `req` is lost
wholesale above the 4 KiB cap, and for `shell.exec` `req.argv` **is** the audited act. The allowlist
is the wrong tool (unbounded); a bounded **producer-side** summary is the right one, which makes it a
change in every tool's dispatch path rather than in `db::audit`.

**Audit / observability:** [#628](https://github.com/hherb/kastellan/issues/628) — causal structure
lives in payload prose, not columns: `task_id` is a payload key in 58 places across 19 files,
enforced nowhere, with no grouping key for one plan iteration and no link from a row to the row that
caused it. [#629](https://github.com/hherb/kastellan/issues/629) — `MemoryLayer::L4` is declared with
no writer.

**Channel / scheduler:** [#588](https://github.com/hherb/kastellan/issues/588) (the shared
`live_ask_for_claimant!` bind contract is doc-only; both binds are `text`, so a transposition
type-checks and returns zero rows — fail-*closed*, hence deferred);
[#515](https://github.com/hherb/kastellan/issues/515) (an unreachable PG can delay daemon shutdown by
sqlx's 30 s pool-acquire timeout; fix is a 5 s `tokio::time::timeout`);
[#501](https://github.com/hherb/kastellan/issues/501) (no long-lived channel sidecar gets
leak-scanner fingerprints, **and the proxy fails open**, so it looks scanned);
[#497](https://github.com/hherb/kastellan/issues/497) (unify the per-family `ChannelBus` instances);
[#334](https://github.com/hherb/kastellan/issues/334); [#332](https://github.com/hherb/kastellan/issues/332)/[#328](https://github.com/hherb/kastellan/issues/328);
[#330](https://github.com/hherb/kastellan/issues/330).

**Tools / planner:** [#537](https://github.com/hherb/kastellan/issues/537)/[#538](https://github.com/hherb/kastellan/issues/538)
(`mail.search`'s `filters.account_ids`/`folder_ids` bypass the `LocalmailId` widening entirely —
**measure live before deciding the fix**); [#534](https://github.com/hherb/kastellan/issues/534)
(give `ToolParam` a type — smaller than #527 assumed: 38 literals across 10 files, one pure renderer,
two design calls to settle first); [#438](https://github.com/hherb/kastellan/issues/438);
[#277](https://github.com/hherb/kastellan/issues/277); [#485](https://github.com/hherb/kastellan/issues/485)/[#484](https://github.com/hherb/kastellan/issues/484).

**Sandbox / VM / egress:** [#519](https://github.com/hherb/kastellan/issues/519) (`microvm-run` is
resolved from `$PATH`, not exe-relative, so it is **not deployable**);
[#554](https://github.com/hherb/kastellan/issues/554) (`tool_allowlists` enforcement is kind-blind);
[#396](https://github.com/hherb/kastellan/issues/396); [#378](https://github.com/hherb/kastellan/issues/378)/[#372](https://github.com/hherb/kastellan/issues/372)/[#356](https://github.com/hherb/kastellan/issues/356);
[#407](https://github.com/hherb/kastellan/issues/407); [#426](https://github.com/hherb/kastellan/issues/426);
[#286](https://github.com/hherb/kastellan/issues/286); [#243](https://github.com/hherb/kastellan/issues/243);
[#298](https://github.com/hherb/kastellan/issues/298); [#317](https://github.com/hherb/kastellan/issues/317).
**The jail has no NSS, and that is documented rather than fixed** (closed by
[#546](https://github.com/hherb/kastellan/pull/546)): any command resolving a user or group name
— `ls -l`, `id`, `whoami`, a bare `python3` — dies by SIGSYS on `socket(2)` inside a `WorkerStrict`
worker. The kill is now *loud*; the commands still cannot run.

**Test-infra / hygiene:** [#510](https://github.com/hherb/kastellan/issues/510) (CI never exercises
#508's regression guard — see [What CI does not cover](#what-ci-does-not-cover));
[#535](https://github.com/hherb/kastellan/issues/535); [#442](https://github.com/hherb/kastellan/issues/442);
[#134](https://github.com/hherb/kastellan/issues/134); [#104](https://github.com/hherb/kastellan/issues/104)
(**six** `pid+nanos` suffix copies, counted properly); [#353](https://github.com/hherb/kastellan/issues/353);
[#130](https://github.com/hherb/kastellan/issues/130); [#196](https://github.com/hherb/kastellan/issues/196).
Long-tail: [#3](https://github.com/hherb/kastellan/issues/3), [#4](https://github.com/hherb/kastellan/issues/4),
[#8](https://github.com/hherb/kastellan/issues/8), [#13](https://github.com/hherb/kastellan/issues/13),
[#14](https://github.com/hherb/kastellan/issues/14), [#20](https://github.com/hherb/kastellan/issues/20),
[#21](https://github.com/hherb/kastellan/issues/21), [#24](https://github.com/hherb/kastellan/issues/24).


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
