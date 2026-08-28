# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260826_624_pre-prune.md`](archive/handover_20260826_624_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here,
> including the full #619, #615/#616/#618 and live-bring-up write-ups compressed below.

**Last updated:** 2026-08-29 · **`main` HEAD:** `4aee83ad` — [#625](https://github.com/hherb/kastellan/pull/625)
squash-merged, so **`fix/624-boot-probe-samples` is closed and its content is on `main`**. ·
**OPEN BRANCH: `fix/627-guard-tier-boot-payload`** (5 commits: two docs, the #627 fix, a prune, and
the five-agent review's fixes). ·
**Last gate: DGX at `12809297` — 3901 / 0 / 55, 175 suites, `TEST_EXIT=0`, cold clippy exit 0; see
[Test baseline](#test-baseline-authoritative).** Reconciles exactly: 3900 at `33029e32` **+ 1**, the
one net new `#[test]` the review round adds, and all **11** `boot_report::tests::` names observed
running in the log. **The Mac has NOT been re-gated since `020b0e53`** (3778 / 0 / 25) — the review
round touches no `cfg`-gated code, but say that rather than round it up.

> ⚠️ **Draft PR [#630](https://github.com/hherb/kastellan/pull/630) (the Headlong cross-project
> study, docs-only) is based on the pre-merge `a30e11fb` and edits both this file and the ROADMAP.**
> It needs a rebase onto `4aee83ad` before it will merge cleanly. Its two filed issues,
> [#628](https://github.com/hherb/kastellan/issues/628) (the audit log's causal structure lives in
> payload prose, not columns) and [#629](https://github.com/hherb/kastellan/issues/629)
> (`MemoryLayer::L4` is declared with no writer), are open and unpicked.

---

## Current state

### #627 — the boot row's key set and rate assignment were untested (branch `fix/627-guard-tier-boot-payload`)

`report_guard_tier` was a private `async fn` in `core/src/main.rs`, a **binary with no
`#[cfg(test)]` module**, so the construction of the durable `policy / guard_tier.boot` payload was
reachable only by starting a daemon against a live guard endpoint. The two tests naming that row
(`cli_ask_e2e.rs`) asserted its **count**.

- **Why it mattered: half of #624's fix lives in the REPORT, not the probe.** Taking the fastest of
  three samples is worth nothing if the row cannot then tell a quiet host from a busy one — and
  swapping `tok_per_s` with `slowest_tok_per_s` **silences** the documented operator query
  `slowest_tok_per_s < tok_per_s / 2`. Every key, every type and every non-null survives that
  mutation.
  > ⚠️ **This said "inverts … a contended host stops reporting as contended and a quiet one starts"
  > until `12809297`, and the second half is impossible.** Since `slowest <= fastest` always holds,
  > a swapped row asks `fastest < slowest / 2`, which **no row can satisfy** — the query returns the
  > empty set on every host, forever. Worse and simpler than an inversion. The same wrong sentence
  > was in the module doc, a test doc, the PR body and here; all four are fixed.
- **New pure module `cassandra::guard_model::boot_report`**: `BootRates::from_basis` (the four
  probe-derived numbers, read out of the basis by one pure function and so shared by the `info!`
  line, the `warn!` finding and the row), `boot_payload`, `not_configured_payload`, and `timeout_ms`
  (the log sites and the payload each had their own copy of `as_millis() as u64`).
  - **`main.rs` does NOT decide "none of them any more"**, which is what its comment claimed until
    `12809297`. `boot_report` owns exactly two things that were decided there: the ms derivation and
    the rate assignment. `kind()`, `tau()` and `n_ctx()` are still read twice and agree by
    coincidence — tolerable (one accessor, no arithmetic, no second *spelling* that can drift) but
    not the same guarantee, and calling it one is how a later session stops checking.
- **`boot_payload` takes `tau` + `n_ctx` as scalars and the budget for provenance, NOT a
  `&GuardTier` — and that IS the fix.** A `GuardTier` has no constructor but `from_router_config`,
  whose `/props` verification is fatal; that dependency is exactly why the payload was untestable.
  Same move as [[unreachable-success-path-proves-nothing]]: extract the decision until a unit test
  can reach it.
- **Mutation-proven THIRTEEN for thirteen**, each mutant executed. The original nine (rate swap —
  killed by 2 tests, deleting any of the three #624 keys, a fabricated `attempted_samples` on an
  operator pin, fabricated zero rates, a drifted unconfigured `state` token, an always-`null`
  `coverage_finding`, an inverted `configured` flag), all re-proved after `from_basis` was
  restructured, **plus four the review found surviving**:
  - `coverage_finding` routed **only for `Probed`** — the worst of the four, because it is
    *selective* and so invisible on a healthy host. Five basis states carry a finding; only the
    clamped one was ever read back. The mutant silenced the three loudest (probe never returned,
    probe FAILED, both out-of-band operator pins) while `timeout_basis` still read `"probe-failed"`,
    so the documented query returned empty for exactly the hosts it exists to find. Now table-driven
    over all ten states with the expectation spelled as a literal `bool`.
  - `timeout_ms` returning the basis's `derived_ms` — no fixture had `timeout != derived_ms`. A
    ceiling-clamped host would have reported a 489 s budget against a 120 s enforced one.
  - `n_ctx` frozen to `66 048` — all ten calls passed that one value. (`guard_tier_e2e` runs 131 072.)
  - the durable `not_configured` token renamed — both assertions compared against the enum, so they
    moved together. Now pinned as a literal; `tier.rs` holds the only other occurrence in the tree.
- **The SEAM is pinned separately, and that is #625's lesson applied one layer out.** Unit tests on
  a shared function stay green if the caller stops calling it. `cli_ask_e2e` now reads the row a
  **real daemon boot stored in real Postgres** and asserts equality with `not_configured_payload()`
  — confirmed to fail when `main.rs` is mutated back to composing that payload inline
  (`left: "unconfigured", right: "not_configured"`). Read from the table rather than a sink double,
  so it asserts what Postgres **stored** [[audit-sink-doubles-hide-storage-transforms]]. The helper
  uses `fetch_all` + an explicit length assertion, because `fetch_one` returns the FIRST row and
  errors only on zero — its old `expect("exactly one …")` claimed a check it did not make.
- **`main.rs` 824 → 771**, which is the lift the file-split backlog already named as the obvious
  first one for that file. Still over the cap; the next lift is the bring-up block.
- **The configured arm's seam is still unpinned by any gate — but NOT for the reason this file gave.**
  > ⚠️ Until `12809297` this said "it needs a live guard endpoint, so only `guard_tier_e2e` can
  > reach it", and [#622](https://github.com/hherb/kastellan/issues/622) says that suite is in no
  > gate and self-skips to a silent PASS. **The premise is wrong.** `from_router_config` skips the
  > probe *entirely* when `KASTELLAN_LLM_GUARD_TIMEOUT_MS` is pinned, so a configured boot needs
  > only a mock answering `/props` — no live endpoint, no timing flake, a fully deterministic row.
  > The gap was real; documenting it as *unclosable* was the defect, and that is the version a later
  > session would have believed. [[handover-claims-verify-before-carrying]] again, on this file's
  > own text. Recipe filed as [#633](https://github.com/hherb/kastellan/issues/633).
- **Deferred with recipes filed, not lost:** [#632](https://github.com/hherb/kastellan/issues/632)
  (rename `tok_per_s` → `fastest_tok_per_s` in `BootRates` **and** `TimeoutBasis::Probed` together —
  the durable wire key must not move, and there is no `Serialize` derive so the two are already
  decoupled) and #633 above.

### #624 — the boot probe measured the BOOT, not the host — MERGED `4aee83ad` ([#625](https://github.com/hherb/kastellan/pull/625))

Full prose in [`archive/handover_20260827_625_merged_pre-prune.md`](archive/handover_20260827_625_merged_pre-prune.md).
Kept here only for what still binds:

- **D9's probe took ONE sample ~3 s into daemon startup**, so it measured startup contention.
  Three consecutive boots on one unchanged DGX backend: 6 073 / 269.6 / 1 582 tok/s, against a
  reproducible ~7 000 measured directly minutes later. A **26x** under-measurement whose slowest
  boot fired a **false** ceiling finding — the tier's loudest signal, spent on a host that
  adjudicates a worst-case document in ~19 s.
- **Fix (spec D11, amending D9): `PROBE_SAMPLES` (3) samples, keep the FASTEST.** Prompt
  processing has a hardware ceiling and no floor, so contention can only make an observation
  *slower*; a mean is wrong for a one-sided error. It moves the budget **down**, toward the
  fail-open edge, deliberately — `PROBE_SAFETY_FACTOR`'s 2x is already the margin for runtime
  contention, and folding startup contention into the rate spends it twice.
- **Each sample carries its OWN cache-buster, and that is load-bearing.** N samples sharing one
  send N byte-identical prompts, which on a backend that does not report `cached_tokens` read as
  enormous throughputs — and fastest-wins would then *prefer* them. A fail-open manufactured by
  the fix; only an e2e can see it, and one does.
- **The durable row carries the spread AND its denominator.** `TimeoutBasis::Probed` gained
  `slowest_tok_per_s` + `measured_samples` + `attempted_samples`; `Saturated`/`Unprobed` gained
  `attempted_samples`. Queries: `slowest_tok_per_s < tok_per_s / 2` = busy boot;
  `attempted_samples > measured_samples` with no finding = read that boot's `warn!` lines. The
  spread is deliberately **not** a finding.
- **The review's CRITICAL, and the rule it left:** `summarise(&samples)` → `summarise(&samples[..1])`
  silently reverted the whole fix and passed every guard test in the tree. The e2e asserted three
  completions and three distinct bodies — which proves the LOOP ran three times and says nothing
  about whether the FOLD saw more than one. **When a fix's value lives in a fold, pin the fold's
  *inputs*, not just its output shape.** #627 below is the same rule applied one layer out.
- **Deferred, still open:** [#626](https://github.com/hherb/kastellan/issues/626) — because
  `PROBE_TOTAL_BUDGET_MS == PROBE_BUDGET_MS`, a **saturating FIRST sample still ends the probe at
  one sample** and fires the false ceiling finding (the cold-`llama-server` case). #624 fixed the
  *contention* half only.

> ⚠️ **#624 does NOT close [#612](https://github.com/hherb/kastellan/issues/612), and merging the
> two is the mistake to avoid.** #624 is that the *sample* was taken under load on any host; #612
> is that extrapolating from a ~1 KiB sample is non-linear on Metal *whatever* the load — a quiet
> Mac still reads 1 137 tok/s at 1 KiB and 260 at 64 KiB. Both point at the same eventual remedy:
> measure from the `ms` / `body_byte_len` the guard rows carry since #616.

### #619, #615/#616/#618 — merged, compressed

Full prose in [`archive/handover_20260826_624_pre-prune.md`](archive/handover_20260826_624_pre-prune.md).
Kept here only for what still binds:

- **#619 (`3bd45a36`)** — the retrospective review of the diagnostics slice. One real defect:
  `classify_transport` folded the both-reqwest-flags-set case (a **connect timeout**) into
  `Timeout`, copying `transport_kind_tag` — which makes a *display suffix*, not a *count* —
  and contradicted `boot::is_timeout` ~300 lines away. A black-holed SYN then read as 100%
  timeouts and sent an operator to #612's ~350 s pin, which cannot help: connect is capped at
  `min(timeout, 5 s)` independently. Fixed with a fourth arm, `GuardErrorKind::ConnectTimeout`;
  `boot::is_timeout` is now `matches!(classify(e), Timeout)`, so the two cannot diverge again.
  Also: three wirings pinned by nothing CI runs, now hermetic; a `warn!` on the `Unmeasured`
  fail-open; `coverage_finding`'s `_ => None` made exhaustive. **The honest whole-fail-open
  query is `state NOT IN ('clear','block')`, not `error_kind IS NULL`.**
  Deferred: [#620](https://github.com/hherb/kastellan/issues/620) (the `error_kind`
  biconditional is documented, not structural), [#621](https://github.com/hherb/kastellan/issues/621)
  (a withheld `fetch_handoff` slice is audited `outcome: "ok"` with no injection trace),
  [#622](https://github.com/hherb/kastellan/issues/622) (`guard_tier_e2e` is in no gate and
  self-skips to a silent PASS).
- **#615/#616/#618 (`e258ad3c`)** — the tier's durable record could not answer the questions
  its own open design call turns on. `guard.error_kind` is a **closed discriminant** beside
  `guard.state` (never the backend's error text), so a timeout is countable by equality
  rather than inferred from `ms` and `body_byte_len`; `TimeoutBasis::Operator` carries a
  `PinBand` so an out-of-band pin reaches the `warn!` and the boot row while still being
  honoured verbatim (an **in-band** pin keeps the historic `"operator"` token — use
  `LIKE 'operator%'` to count all pins); and `fetch_screen`'s Block arm withholds through a
  **total** function. **#616 is what unblocked #612's favoured option.**
  [#617](https://github.com/hherb/kastellan/issues/617) stays out of scope — `req` lost
  wholesale above the audit cap needs a bounded **producer-side** summary in every tool.

> ⚠️ **#614's merge wrongly CLOSED #612 and #615** via "Filed, **not fixed**: #N" — GitHub
> matches the `fixed: #N` substring and ignores the negation. Now in
> [Standing hazards](#standing-hazards-that-have-each-cost-a-session).

### #614's two review rounds, and the wiring slice — compressed

Eight agents over two rounds on #614; full prose in
[`archive/handover_20260824_diagnostics_pre-prune.md`](archive/handover_20260824_diagnostics_pre-prune.md)
and, for the wiring slice, [`archive/handover_20260823_live-bringup_pre-prune.md`](archive/handover_20260823_live-bringup_pre-prune.md).
What still binds:

- **Round one: the audit-cap fix kept half the defect.** An unaffordable preserved key was
  dropped *silently*, giving a row with the same key set as one whose dispatch never ran a
  tier. Keys are now admitted individually against the budget less `DROP_MARKER_RESERVE`,
  refusals are named under `DROPPED_PRESERVED_KEY`, and a `const` block makes a member
  shadowing `_truncated`/`sha256`/`len` a **compile** error.
- **Round one's other defect composed with #612:** the live instrument passed having measured
  nothing under a *pinned* timeout — precisely the configuration #612 tells a Metal operator
  to use. It now refuses a pin outright. **The stated mitigation for an issue can be the exact
  thing that disarms the instrument built to check it.**
- **Round two closed the CLASS, not the key.** `AuditSink::insert` is a **provided method**
  applying `truncate_payload` before delegating to `insert_stored`, so no sink double can
  record a payload Postgres never stored [[audit-sink-doubles-hide-storage-transforms]];
  `GUARD_KEY` is shared across crates so a rename is a compile error; `probe::run` was routed
  through `audit::insert` (round one's own module invariant had claimed every write already
  went through it, while the site it named first did not). Mutation-proven six for six.
- **Smaller, each its own lesson:** a live instrument that **printed** `COVERAGE FINDING` and
  passed green (libtest captures `println!` on a pass) now **fails**; four `{value:.N}` format
  specs were silent no-ops, because `serde_json`'s `Display` never consults `precision`.
- **D10 — the tier is ADVISORY defence-in-depth, NOT a gate.** 65% recall (36/55) at FP-0; 6/6
  on bare imperatives at median 0.9955 but **5/8 missed** on narrative framing at median
  0.0797; τ pinned by ~4 documents. **Nothing downstream may relax on it** — no catalogue
  weight lowered, no allowlist widened, no sandbox constraint loosened.
- **τ = 0.79552656 is a REQUIRED operator input with no default**, and **five
  misconfigurations STOP THE DAEMON** (D6): half-configured keys, τ outside `(0.0, 1.0]`, a
  pinned timeout of 0, an unreachable `/props`, and a context below
  `SCAN_BYTE_CAP + 512 = 66 048` (D8, which turns #604's attacker-reachable HTTP 400 into a
  boot refusal). Unconfigured is silent by design; `KASTELLAN_REQUIRE_GUARD=1` makes *that*
  fatal too, because `install` regenerating `kastellan.env` drops all three keys at once and
  lands on the one non-fatal arm.
- **`RouterConfig` lost its `Eq` derive** — `guard_tau: Option<f32>` can hold a NaN.
- **Deliberately not done:** a `cfg!(target_os = "macos")` boot warning for #612 — filing it
  with the evidence was the call, and the runtime path already `warn!`s on every fail-open.

### The guard-model arc so far, compressed

Full prose in [`archive/handover_20260823_wiring-slice_pre-prune.md`](archive/handover_20260823_wiring-slice_pre-prune.md)
and the linked PRs. Kept here only for what still binds.

- **[#606](https://github.com/hherb/kastellan/pull/606) `d51c9b20` — measurement 3.** 133 cases,
  109 captured through the real `web.fetch` path: **τ = 0.79552656**, FP-0 on both hosts.
  `best_tau` returns **NONE** — real captured content overlaps at every threshold, which is what
  makes D7's budgeted operating point necessary rather than nice. Three findings still govern how
  the tier may be described. **(A)** τ is pinned by **security prose** with ~1 point of headroom and
  by roughly **four** documents, but the pairs make it legible (OWASP's index 0.0009 vs its LLM01
  page 0.5446; Wikipedia *prompt injection* 0.0274 vs *XSS* 0.7843), so the guard reacts to **quoted
  attack text**, not to subject matter. **(B)** 19 of 55 attacks missed, concentrated in
  **narrative** indirect injection: bare imperatives 6/6 at median 0.9955, the same intent wrapped
  in a plausible document 5/8 missed at median 0.0797. **(C)** truncation can cost the whole signal
  — a 1.8 MB payload truncated to 64 KiB scored **0.0102** against its family's median 0.9937.
  Two caveats the artefacts did not state about themselves: the Mac report is **not recomputable
  from its own printed scores** (4 dp against an 8-sig-fig τ), and **τ is fitted and evaluated on
  the same 133 cases**, so `FP 0` is guaranteed by the criterion that chose it and is not a rate.
  Five issues filed: [#601](https://github.com/hherb/kastellan/issues/601)–[#605](https://github.com/hherb/kastellan/issues/605).
- **[#598](https://github.com/hherb/kastellan/pull/598) `abb3d3a7` — the weights are pinned by
  BYTES, checked at use.** Both hosts said they ran `Shieldstral-1.0-3B-Q8_0`; they ran **different
  files at the identical byte length**, both valid GGUF. **Pinning a quantisation LABEL is not
  pinning the bytes.** Consequence that still binds: **calibration must run on the host serving the
  model.** Its review's best catch is now this repo's rule: **when no fixture can exercise the
  success path end to end, extract the decision until one can — otherwise the suite proves only
  that the tool says no** [[unreachable-success-path-proves-nothing]].
- **[#596](https://github.com/hherb/kastellan/pull/596) `2ab6612c` — four fail-opens**, each able to
  produce a corpus or threshold that *looked verified and was not*: `--record` disabled every hash
  check; an empty budget scope made D7's criterion vacuous; τ printed at `{:.6}`, which does not
  round-trip an f32; and the HTTP status was never checked, so a vanished snapshot's 404 page was
  pinned wearing the label of the page it replaced.
- **[#593](https://github.com/hherb/kastellan/pull/593) `b58edc77` — M1 discharged.** At
  `SCAN_BYTE_CAP` the tier costs **p50 3 215 / 3 558 ms** on the DGX against the study's 30–43 ms
  (taken on ~26-token strings) — ~85×. Cost is **entirely prompt processing and linear in tokens**,
  which is what makes D9's boot probe possible at all. Its own lesson: **a test that exercises only
  the degenerate configuration of a parameter proves nothing about the parameter.**
- **[#585](https://github.com/hherb/kastellan/pull/585) `f90631da` — slice 1.** Guard endpoint seam,
  adjudicator, offline calibration harness, **no production wiring**. Two findings overturned the
  feasibility study and must not be re-derived from it: its `0.45–0.70` band holds exactly one
  reachable value, and `observation replay` is plan-level so cannot score a document-level tier.
  Best review catch: *a mock that does not return what it was sent tests only your own canned
  response.*
- **[#587](https://github.com/hherb/kastellan/pull/587) `47ba5b4f` — exact ask containment.**
  Containment became an exact, peer-scoped **live-nonce lookup**. **The slice diverges from #582 as
  filed and the reason must not be re-derived away** — done literally, the issue reintroduces
  #579's defect, because the predicate it says to replace had a second job
  [[issue-as-filed-can-carry-a-regression]]. **Open risk 3, accepted not fixed:** a body carrying a
  live token and **no verb at all** never reaches the exact check and is enqueued. **Still unfiled,
  from slice 2's reviews:** `via: "cli"` on the CLI's `ask.resolved` row is untested, and nothing
  drives `/deny` end to end.

### Merged arcs, compressed

Full prose in [`archive/handover_20260821_pre-prune.md`](archive/handover_20260821_pre-prune.md) and the linked PRs. Kept here only for the lessons that still bind.

- **[#585](https://github.com/hherb/kastellan/pull/585) `f90631da` — Shieldstral adjudicator, guard-model slice 1.** Guard endpoint seam, adjudicator, offline calibration harness. **No production wiring** — five chokepoint files byte-identical to `main`, verified as a merge gate. **Two findings overturned the feasibility study** and must not be re-derived from it: (F1) its `0.45–0.70` band holds exactly one reachable value, so the tier is re-aimed at the catalogue **miss** at 0.0; (F2) `observation replay` is plan-level and cannot score a document-level tier. **The seeded 24-case corpus is a PROOF OF CONCEPT and does not discharge measurement 3** — any τ from it is provisional and must never become a default. Best review catch, and it generalises: *a mock that does not return what it was sent tests only your own canned response* — `guard_model_e2e`'s mock read only far enough to find `Content-Length`, leaving two tier-killing mutations green.
- **[#579](https://github.com/hherb/kastellan/pull/579) `bb937df7` — #564 slice 2, the ask channel.** `ChannelOutbox`, D16's peer-scoped `EXISTS` inside the guarded UPDATE (**the nonce is a BEARER token — reading, not guessing, was the real threat**), D17's `NONCE_BYTES` 32 → 5. Its five-agent review found eight things nine per-task reviews and 3522 tests had missed, all on the **argument-passing seams between layers** rather than in logic.
- **[#578](https://github.com/hherb/kastellan/pull/578) `af3e7e66` — #564 slice 1b, the ask path.** `Verdict::Escalate` stops degrading to `Block`; `Outcome::{AwaitingOperator, Denied}`, `final_state() -> Option<&'static str>`, the 60 s expiry sweep, `kastellan-cli inbox`, and **D11** (`asks.resume_state`, migration 0024) because a resumed task otherwise re-executed steps it had already run — approve a plan and an earlier step's email goes out twice.
- **[#572](https://github.com/hherb/kastellan/pull/572)/[#573](https://github.com/hherb/kastellan/pull/573) `fbe91c4d`+`e8ea4339` — mail attachments by `{message_id, filename}`.** Plus #574, the `/tmp`-wipe fix. Durable lesson: **a mutation score is only as good as the mutation set** — a reviewer's own 15 mutations left **11 surviving** with all 113 tests green, clustered exactly where per-module rounds had not looked.
- **[#569](https://github.com/hherb/kastellan/pull/569) `07b6451e` — guard measurement 2 + Q8.** Runtime and quantisation **PINNED**: llama.cpp + `Shieldstral-1.0-3B-Q8_0` on both hosts, so one fitted τ transfers.
- **Older arcs** (#555/#556/#558/#562, the channel-supervision arc, #549/#546/#540/#536/#528, email slice 1, the egress and micro-VM slices, the 0.2.0 release) — see the archive snapshots and [Recently merged](#recently-merged).

### Standing hazards that have each cost a session

> ⚠️ **Local clippy is NOT CI parity: both hosts run rust 1.96.0, GitHub CI runs 1.97.0.** #573 was clippy-clean on the Mac *and* the DGX and still failed CI on a lint that does not exist in 1.96. **Treat CI as the authority on lints** and expect a possible one-line follow-up after any push that adds code.

> ⚠️ **A cached `cargo clippy` reports a full-workspace pass it never ran.** Exit code alone does not distinguish it — **count the `Checking` lines**. Honest from a cold `CARGO_TARGET_DIR` is ~217–303; a warm dir can report exit 0 having linted 4. Count against the *reverse-dependency set*, not against 27, or a correct incremental lint reads as a failure.

> ⚠️ **`cargo check`/`clippy --all-targets` do NOT warm the target dir for `cargo test`** — they emit metadata-only artifacts, no linked binaries. A full sweep after a lint-only leg pays a cold link (11m on the Mac vs 29s on the DGX). **Run the sweep first, lint after.**

> ⚠️ **A private `CARGO_TARGET_DIR` does not build `examples/`,** so `email_channel_e2e`'s 6 tests fail with `fixture not built` at a perfectly green commit. Fix: `cargo build -p kastellan-core --example fake_email_worker`. Same family as the daemon-e2e breakage a custom target dir causes ([[custom-cargo-target-dir-breaks-daemon-e2e]]) — read the failure text before believing a regression.

> ⚠️ **"Filed, not fixed: #N" in a PR body or commit message CLOSES #N.** GitHub matches the `fixed: #N` substring and has no notion of negation. It has cost three issues: #539 (2026-08-11, noticed), then **#612 and #615 together** (2026-08-24, unnoticed until the next session reconciled this file against `gh issue list`). Write **"deferred to #N"** or **"#N — filed, unfixed"**, and before merging run `gh pr view <n> --json body --jq .body | grep -oiE '(close[sd]?|fix(e[sd])?|resolve[sd]?)[[:space:]:,]+#[0-9]+'` over the body *and* the commit message.

> ⚠️ **Squash-merge caveat:** every PR lands as one squash commit, so its *branch-tip* SHA (where the gate ran) is **not** an ancestor of `main`. Check content, not `merge-base`.

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

**Next up — operator's choice, each roughly one session:**

- **Three closed, two facts survive them.** ~~#561~~ (fixed upstream in localmail), ~~#506~~ (`cb33005c`), ~~#552~~ (`76ac51f5`); detail in [`archive/handover_20260824_diagnostics_pre-prune.md`](archive/handover_20260824_diagnostics_pre-prune.md). **#506's `floor_resolved` branch could not be exercised by the live gate** (the planner never omits the field on this host), so its PG e2e is that branch's only evidence. And **#561 leaves a latent, unfiled hazard: paging a `mail.search` with a *different* `query`** continues the date walk with the new filter and returns `200`, silently skipping anything newer than the cursor — keyset semantics working as designed, but it means don't change the query while paging.
- **[#560](https://github.com/hherb/kastellan/issues/560) — the planner fabricates a 16-hex `message_id`.** Do **not** close this by rewriting the parameter description: #536 already did exactly that ("not a placeholder"), deployed 2026-08-09, and both later runs still fabricated. The lead worth measuring is in the issue — with keys stripped by `extract_scannable_text`, `"20973"` reaches the planner as a bare line among subjects and dates, with nothing marking it as *the id* [[tool-output-reaches-planner-key-stripped]] [[opaque-ids-are-unusable-tool-params]].
- **[#550](https://github.com/hherb/kastellan/issues/550) — the *generated* `kastellan.env` gets no end-to-end check.** #531 verifies the optional overlay most hosts do not have and skips the required file every host does have; on a no-overlay host a dropped directive for it renders as the reassuring `none at …` line at `info!`. **The naive fix is wrong** — the overlay legitimately overrides `kastellan.env` keys, so per-file comparison false-positives; it has to compare the *folded* environment (later file wins), which `fold_env_files` already computes for launchd.
- **[#551](https://github.com/hherb/kastellan/issues/551) — no path directive escapes systemd's `%` specifier.** Pre-existing and workspace-wide (`ExecStart=`, `Environment=`, not just `EnvironmentFile=`): a literal `%` in `$HOME` renders a directive systemd mis-expands, dropping it with the same fail-open shape #530 fixed. Measure first, then either escape `%%` or reject at install.
- **[#548](https://github.com/hherb/kastellan/issues/548) — PG e2e tests install units into the operator's *real* `~/.config/systemd/user/`.** Filed 2026-08-13 while verifying #529 on the DGX: a unit from a hard-killed `channel_bus_pg_e2e` run on **2026-06-21** was still sitting beside the three production units. Not a teardown bug — `PgCluster`'s `Drop` guards are correct and simply cannot run on SIGKILL — so the fix is about blast radius, not cleanup. Cheapest option is a sweep of stale `kastellan-supervisor-test-*` units at bring-up; a scratch units dir is cleaner isolation but breaks anything that needs the manager to actually start the unit. Low priority, no correctness or containment impact.
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

- **Shieldstral guard-model — WIRED (`8736f559`, [#607](https://github.com/hherb/kastellan/pull/607)) and RUNNING LIVE on the DGX** (see [Current state](#current-state)). **Deployed 2026-08-25 and verified at the binary** — `strings` on the *installed* binary carries all five era markers (`guard_tier.boot`, `_dropped_preserved`, `error_kind`, `connect_timeout`, `operator-below-floor`), and task 178's `web.fetch` row read `{"p": 0.0081, "state": "clear", "error_kind": null}`. The lesson generalises: a DGX checkout can look current while the running daemon predates it by hours, because the tree was pulled and never rebuilt — `strings` on the installed binary beats every timestamp argument [[handover-claims-verify-before-carrying]]. **The branch below is NOT yet deployed.** What remains:
  - ~~[#624](https://github.com/hherb/kastellan/issues/624)~~ **MERGED as `4aee83ad`** ([#625](https://github.com/hherb/kastellan/pull/625)) — the probe now takes up to 3 samples and keeps the fastest; see [Current state](#current-state). **The DGX has NOT been redeployed onto it** — that is the one outstanding operator action from this arc; expect `slowest_tok_per_s`, `measured_samples` and `attempted_samples` in the next `guard_tier.boot` row. Two follow-ups filed from the review and **not** folded in: **[#626](https://github.com/hherb/kastellan/issues/626)** (a saturating FIRST sample still fires the false ceiling finding — #624 fixed the *contention* half only; the fix costs up to 60 s of startup on the sickest host, or weaken the finding when `measured_samples == 0 && attempted_samples == 1`) and ~~[#627](https://github.com/hherb/kastellan/issues/627)~~ — **DONE on `fix/627-guard-tier-boot-payload`**, see [Current state](#current-state).
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
- **File-split backlog (Item 9b)** — re-`wc -l` before picking, the numbers drift. **Six files are over the cap and the ask arc keeps growing them, deliberately** (the repo's own rule is to split *before* the change that grows a file, and folding a move in afterwards is the worst of both). Re-measured 2026-08-21 at the tip of `fix/582-584-ask-recognition`: `db/src/asks.rs` **1127** (was 905), `core/src/channel/ask_message.rs` **956**, `core/src/scheduler/asks.rs` **801** (~510 non-test), `core/src/scheduler/inner_loop.rs` **778**, `core/src/channel/bus.rs` **742** (was 658), `core/src/scheduler/runner/task_exec.rs` **561**. **`core/src/channel/ask_message.rs` is now the best first pick** — it is ~330 production lines and ~620 test lines, so a pure test-lift to `ask_message/tests.rs` fixes it in one movement-only commit with the count verifiable either side. `db/src/asks.rs` is the biggest and the least mechanical (its `mod tests` is small, so any split there is a production reorganisation). `scheduler/asks.rs` remains a clean pick — its pure half (`resolution_choice`/`decide`/`ask_deadline_seconds`/the resume-state codec) is cleanly separable from its async half. Also over-cap: **`db/src/tasks.rs` (533, added 2026-08-16 by #564 slice 1a — 427 before it)**, whose `mod tests` is a single 12-line round-trip, so the cheap test-lift saves nothing and any split here is a *production* reorganisation; deliberately deferred to its own movement-only PR rather than folded into the slice, per this list's own rule below. Then `workers/mail/src/handler.rs` (**670**, of which ~305 is production ⇒ a clean test-lift to `handler/tests.rs`) and `core/src/scheduler/inner_loop/summary.rs` (**533**), both grown by #562 and both already at or past the cap before it — deliberately *not* split there, because this repo's own pattern is to split **before** the change that grows a file so the movement diff is reviewable alone, and folding a move in afterwards is the worst of both; `sandbox/src/linux_firecracker/plan.rs` (~1160, prod only ~485 ⇒ a clean test-lift; `cfg(linux)` so DGX-gated), `workers/matrix/src/sdk_live.rs` (722, live-matrix-gated → DGX), `db/graph.rs` (926, design-gated Item 23b — deferred until a 2nd `WalkedEdge` consumer). ≤27-over deferrals (a lift saves little): `db/src/lib.rs`, `supervisor/src/launchd_agents.rs`, `core/src/scheduler/tool_dispatch.rs`, `db/src/memories/search.rs`, `entity_extraction/batch_upsert.rs`. Over-cap *test* files: `core/src/workers/gliner_relex/tests.rs` (1083), `core/src/workers/python_exec/tests.rs` (844), `core/src/scheduler/inner_loop/tests.rs` (767), `core/src/scheduler/audit/tests.rs` (713), `core/src/cassandra/types/tests.rs` (654). **Added by the guard wiring slice (2026-08-23), stated rather than left to be discovered:**
`core/tests/guard_tier_e2e.rs` (**774**, new) — ~200 of it is the multi-request HTTP mock, which
would lift cleanly into `tests/guard_tier_e2e/{main,mock}.rs`; deliberately *not* done in the slice
that created it, per this list's own split-before-you-grow rule, and consistent with the tree's other
large e2e binaries (`secret_vault_e2e` 813, `cli_ask_e2e` 800). `core/src/main.rs` grew **~645 → 727**
for the boot block and `report_guard_tier`; that function is self-contained and is the obvious first
lift if `main.rs` is ever split. `llm-router/src/config.rs` **750 → 843** and
`llm-router/src/messages.rs` **536 → 586**, both already over-cap before the slice and both grown only
by additive fields plus their tests. **`core/src/cassandra/guard_model/tier.rs` was split at 507
during the slice** — pure decisions (257) from the boot sequence (`tier/boot.rs`, 279) — which is the
rule being followed rather than deferred, and the counterexample to the rest of this paragraph.

**The `timeout` family is DONE (2026-08-26, `fix/624-boot-probe-samples`)** — and it is the worked
example of the rule this list keeps stating. `timeout.rs` (479) and `timeout/tests.rs` (687) were
split **before** #624 grew them, in a movement-only commit with 27 `#[test]`s before and 27 after
and an identical name set. Three ways, along the seam the production side already had:
`timeout/sample.rs` (what ONE measurement is) / `timeout.rs` (how it becomes a budget) /
`timeout/basis.rs` (how the budget describes itself), each with its own `tests.rs`. Post-fix:
`timeout.rs` **336**, `sample.rs` **457**, `basis.rs` **319**, `tests.rs` **362**,
`sample/tests.rs` **464**, `basis/tests.rs` **282** — all under. Two things worth copying: the
probe-document tests moved beside the private `PROBE_BODY` they assert on, so that constant stayed
private instead of being widened for a test's convenience; and everything moved is re-exported from
`timeout.rs`, so the split moved code and not names.
**`tier/boot.rs` went 497 → 533 in the same slice and was split in the same PR** rather than
deferred: `tier/probe.rs` (**133**) now holds the probe's IO half (`run_probe`, `run_one_sample`,
`elapsed_ms`, `is_timeout`) and `boot.rs` drops to **426**, with three lines changed besides the
deletion. Better cohesion as well as line count — `boot.rs` is about whether a *configured* tier is
usable, `probe.rs` about *measuring* one.
**Still open from the diagnostics slice:** `core/tests/guard_tier_e2e.rs` (**1309 → 1351**), whose
~200-line multi-request HTTP mock is the clean lift (`tests/guard_tier_e2e/{main,mock}.rs`) —
deliberately not folded into #624, per this list's own rule. `tier/error_kind.rs` (**449**) and
`tier/tests.rs` (**432**) are both approaching the cap and neither was touched by #624.

**`boot_supervisor/tests.rs` is off this list** — the #518/#522 PR split it into `tests/{mod,bringup,liveness,reporting}.rs` (137/257/281/233) as pure movement in its own commit, which is the pattern to copy for the rest: split *before* the change that grows the file, so the movement diff is reviewable on its own and the test count is verifiable before and after.

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

## Load-bearing findings from the last two sessions

### The four faults (2026-08-02) — compressed, full prose in [`archive/handover_20260823_pre-prune.md`](archive/handover_20260823_pre-prune.md)

Driven end to end from one real Matrix message. **Four independent faults, only one a
kastellan bug in the layer everyone suspected**, each masking the next:

1. **The NVIDIA driver was gone** after an `apt upgrade` — Ollama ran a 26B model 100% on
   CPU. Diagnose first, in one line:
   `ssh dgx 'lsmod | grep -c nvidia; curl -s 127.0.0.1:11434/api/ps | grep -o "size_vram[^,]*"'`.
   [[dgx-apt-upgrade-drops-nvidia-module]]
2. **kastellan did not survive a reboot on Linux** — missing `systemctl --user enable`,
   fixed in [#509](https://github.com/hherb/kastellan/pull/509).
3. **The local model thought until the request timed out.** `chat_template_kwargs` +
   `disable_thinking` (default ON) measured **222 s → 51 s**. Raising
   `KASTELLAN_LLM_TIMEOUT_MS` is NOT a fix — tried, failed.
4. **`plan_parser` hid all three.** `parse_plan_lenient` re-emitted the *strict* error, so
   every reader was pointed at a markdown fence while the real error was
   `missing field 'steps'`. **Cost the entire session** until the raw output was logged.
   `steps` is deliberately NOT defaulted — an empty `steps` marks a terminal plan.

**Do not benchmark the agent against a loaded DGX**: the same question took 77 s on a quiet
box and timed out at 283 s under a concurrent `cargo test --workspace`.

### The fail-open `data_ceiling` correction — CLOSED, kept for the shape

`data_ceiling` is a **ceiling**, so the most *sensitive* `DataClass` is the most *permissive*
value it can hold. #505 defaulted an absent field to `Secret` — rank 3, the maximum — and
shipped it documented as fail-**closed**, which left a plan omitting the field not
ceiling-constrained at all. Behavioural fix in #506 (`cb33005c`). Severity was bounded:
the invariants it disabled only ever catch a model contradicting *its own* declarations, a
competence signal rather than an attack barrier. Full analysis, including why the second
review round widened it from two invariants to three, in [`archive/handover_20260823_pre-prune.md`](archive/handover_20260823_pre-prune.md).


### Egress / MITM traps (from #491–#503) — read before touching the proxy

1. **A `CA:TRUE` self-signed *leaf* is rejected at handshake** with rustls' `CaUsedAsEndEntity`, even though `openssl verify` accepts it — and `openssl req -x509` commonly produces exactly that shape. It fails **late and opaquely** as a `mitm_failed: …` egress decision, not at startup. The live DGX localmail cert WAS this shape; regenerated `CA:FALSE` on 2026-07-26 (backups `~/.config/localmail/tls/*.cabak-20260726-173651`) and the tier then passed. Working shapes: a non-CA leaf (`CA:FALSE`) or a real CA that signed a separate leaf. Verify with `openssl x509 -in <cert.pem> -noout -text | grep -A1 'Basic Constraints'`.
2. **The upstream anchor is trusted for EVERY host that sidecar can reach**, not just the keyed origin. #492 therefore *enforces* single-private-origin rather than trusting operator discipline: an anchor is handed out only when the worker's allowlist resolves to a single private origin written as an **IP literal** (privateness via `kastellan-net-classify::is_denied_range`, the proxy's own SSRF predicate, so the two can't drift), and **a refusal fails the spawn**. **Known limitation, documented and test-pinned, not closed:** keying is per-**host**, not per-service, so two private services sharing one address (the DGX's actual shape: localmail `:8443` + SearxNG `:8888` on `10.0.0.3`) are one origin to the rule and the second worker's sidecar also receives the first's anchor. Closing it needs `host:port` keying or a per-host rustls verifier (#492's explicit non-goal); mitigation is operational — give co-located private services distinct addresses.
3. **`tls_intercepted: true` is weaker than it reads** — emitted when the proxy takes the MITM branch, *before* the upstream handshake. **Round-tripped bytes are the load-bearing assertion.**
4. **The decision-ingest thread is deliberately DETACHED**, so reading captured rows right after `worker.close()` races its drain — worst for a connection's LAST decision. Any test asserting on a *terminal* egress decision must poll to quiescence (the shared helper does).
5. **UDS path length:** a merely descriptive scratch-dir prefix under macOS `$TMPDIR` pushes `<scratch>/egress.sock` past `sun_path` (`SUN_LEN` error) and the sidecar dies before reading the CA. Use `tests-common::short_scratch_root`.
6. **The proxy's upstream leg trusts webpki roots ONLY** unless the operator sets an anchor — so **no hermetic self-signed origin is possible for a MITM'd worker's e2e**; real-origin tiers are structural, not lazy ([[egress-proxy-upstream-trusts-webpki-only]]).
7. **Egress-decision assertions must match `host:port`, not a bare host** — a bare-host check passes on any decision mentioning it, and on the DGX loopback SearxNG and the embed endpoint share an address (cost a #448 round-trip; the convention is `is_allowed_row_for` / `is_for_origin`).

### Deployment facts (DGX)
- **Deploy history** (the `fix/531` branch build, `ddda13dc`, `6e22a470`) is in [`archive/handover_20260823_pre-prune.md`](archive/handover_20260823_pre-prune.md) § Deployment facts. The host has not been redeployed this session; measurement 3 ran from the dev tree, not the installed build.

- **#492 live confirmation DONE 2026-08-01** (audit_log, task 114): the force-routed mail tool reached the **self-signed** localmail through the MITM sidecar using the operator anchor — `egress.allowed {worker:"mail", host:"10.0.0.3", port:8443, tls_intercepted:true}` then `mail.search` returning **25 533 bytes**, which is the load-bearing evidence. Contrast in the same table: `worker:"matrix"` rows carry `tls_intercepted:false` (transparent tunnel), exactly as designed.
- **Deploying a BRANCH is a hand-roll**, because `upgrade_from_git.sh` hardcodes `git switch main`. The working sequence, mirroring the script's steps 2–5: `bash scripts/build-release.sh` (NOT a bare `cargo build --release --workspace` — the Matrix worker needs its `live-matrix` feature or the channel dies at spawn), then **`./target/release/kastellan-cli install --matrix-homeserver-url <url> --matrix-user <user>`**. Two traps, both live-confirmed on 2026-08-11: the CLI **must** be the `./target/release/` one (`~/.local/bin/kastellan-cli` is a symlink into the install dir, so it copies the installed binaries onto themselves and still prints `installed 15 binaries`), and the Matrix flags **must** be re-passed because those three keys live in the *generated* `kastellan.env`, not the `.local` overlay. The overlay's five keys (extra-CA, `LLM_LOCAL_MODEL`, `LLM_TIMEOUT_MS`, mail endpoint + token file) survive untouched — verified in `/proc/<MainPID>/environ`, including the `-ctx64k` model tag and the 180 s timeout that older deploys used to clobber. Force-routing is baked into the generated unit and came back `=1` without intervention.
- **A hand-rolled branch deploy must install from `./target/release/kastellan-cli`, NOT `~/.local/bin/kastellan-cli`.** That symlink resolves into `~/.local/lib/kastellan/`, and `install` defaults `--from` to `current_exe()`'s directory — so it copies the installed binaries **onto themselves**, prints `installed 15 binaries`, and changes nothing. Cost a false negative on 2026-08-10 (the live test appeared to show the fix not working; it was testing the old build). It must also re-pass `--matrix-homeserver-url` / `--matrix-user`, which `upgrade_from_git.sh` reads back from the env files first — omitting them regenerates `kastellan.env` without the three `KASTELLAN_MATRIX_*` keys and takes the channel down.
- **A branch deploy must build with `scripts/build-release.sh`, NOT a bare `cargo build --release`.** That script builds the matrix worker with `--features live-matrix`; without it the worker exits immediately and the channel never comes up — `matrix.init: worker exited before responding`, retried forever, `CHANNEL STILL DOWN` after 5 min. Cost one deploy cycle on 2026-08-09. `upgrade_from_git.sh` calls the script (it is hardcoded to `main`, which is why a branch deploy is hand-rolled at all). Silver lining: the failure was *loud and correctly attributed* within 5 minutes — #516/#517/#525's supervision working exactly as designed on a defect it had never seen.
- **Deploy timestamps are AEST (UTC+10) in `systemctl` output and UTC in the daemon log** — the same install reads `08:29 AEST 09-08` and `22:29Z 08-08`. Comparing the two without converting makes a current start look like a stale one; it is the cheapest way to misread this host.
- **The env clobber is FIXED and the manual re-add procedure is GONE** ([#458](https://github.com/hherb/kastellan/issues/458), merged `6e22a470`, live on the DGX since 2026-08-08 22:29 UTC). Tuned settings live in the operator-owned overlay `~/.config/kastellan/kastellan.env.local`, listed *after* the generated file, so `install` may freely regenerate `kastellan.env` and the overlay still wins. **The five settings on this host** — `KASTELLAN_LLM_LOCAL_MODEL` (`gemma4:26b-a4b-it-q8_0-ctx64k`; the bare 4096-ctx tag is what made tasks 111–113 die on `plan decode failed`), `KASTELLAN_LLM_TIMEOUT_MS`, `KASTELLAN_MAIL_ENDPOINT`, `KASTELLAN_MAIL_TOKEN_FILE`, `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA` — are already in it, and nothing needs re-adding after an install. **What to check instead of what to re-do:** `install` prints `dropped:`/`changed:` key *names* (never values) and writes `kastellan.env.bak`, `.bak.1`, … the first time each destructive install runs; on a steady-state reinstall it correctly prints and writes **nothing**, because the generated file is already the stripped one. The one verification still worth doing is the old one — **`tr '\0' '\n' < /proc/$(systemctl --user show -p MainPID --value kastellan-core.service)/environ`** — since a file being right is still not the same as the process having it. Editing the overlay needs a **restart** on systemd and a full **`install`** on launchd (which must carry the original flags); the warning says which.
- **Force-routing needs no re-add** — the generated `kastellan-core.service` carries `Environment=KASTELLAN_EGRESS_FORCE_ROUTING=1` from `core_service_spec` (verified live 2026-08-01). [[dgx-force-routing-deploy-facts]]
- **localmail:** bound to **`10.0.0.3:8443` ONLY** (not loopback); cert SANs `IP 10.0.0.3 / IP 127.0.0.1 / DNS spark-0d2d / DNS localhost`, verified `CA:FALSE`. api-user `kastellan-mail`, granted `horst-gmail`; bearer token `~/.config/kastellan/mail-token` (0600), password `~/.config/kastellan/mail-apipw`. **The token expires 2026-08-30** — re-mint via `POST /v1/auth/login`. The running `localmail-serve.service` started **2026-07-27**, three days *before* the server-side-cursor merge ([hherb/localmail#223](https://github.com/hherb/localmail/pull/223), `0b6c5e05`) it depends on — **restart it before any live email-channel deployment.**
- `scripts/upgrade_from_git.sh` does the whole build+install+restart+verify and is hardcoded to `main`. Daemon logs live in `~/.local/state/kastellan/*.out`, **not** the journal.
- The live DGX bot is **eval-only** (just the two of us, no external users), so transient downtime is fine and restarts need no confirmation — but containment controls still get re-added after an install. [[dgx-eval-only-experiment-freely]]

### Process lessons that have each cost a re-run

- **Write long-run logs to `$HOME`, never `/tmp` — on BOTH hosts.** `/tmp` is scrubbed mid-run on the DGX *and* on the Mac; macOS deleted a completed `cargo test --workspace` log plus the harness task-output files, forcing a second 45-minute gate. Include an explicit exit-code line and a DONE sentinel. [[dgx-run-logs-tmp-scrubbed]]
- **Each host is structurally blind to the other's `cfg` arms.** Mac clippy compiles `#[cfg(target_os="linux")]` items out, so an unused cfg-linux helper passes on the Mac and fails the DGX `-D dead-code` gate; the mirror direction is real too (the DGX is blind to DE-gated items in dual-platform files). Gate both hosts after scripted edits. [[cfg-linux-e2e-deadcode-dgx-clippy]]
- **Don't race sidecar tests against a build.** The 5 s sidecar-readiness budget is load-sensitive: `email_mitm_e2e` took 28.6 s loaded vs 8.2 s quiet, and `egress_force_routing_e2e::forced_coupling_…` fails under full-workspace load but passes 3/3 standalone in ~0.11 s. **Don't "fix" it by inflating a production timeout.** Adjacent: [#328](https://github.com/hherb/kastellan/issues/328). (The *leaked* sidecar children were a different thing — #502, fixed in PR #516.)
- **Subagent-driven sessions: tell the implementer to pass `timeout: 600000` on the Bash call, not just "run cargo in the foreground".** Six agents stalled identically across one 9-task plan (#564 slice 1b): the Bash tool's default timeout is **120 s**, this repo's builds and PG e2e runs exceed it, the call auto-backgrounds, and the agent then reaches for `Monitor` and parks waiting for a notification that its own instructions forbade it to wait for. Repeating the rule did not work; naming the mechanism did. Each stall cost 20+ minutes. The controller should run long gates itself via a backgrounded Bash call, which notifies on exit — that is the mechanism the subagents kept mis-reaching for.
- **Mac CLI cargo blocks on the IDE's rust-analyzer** holding `target/debug/.cargo-lock` — use a scratch `CARGO_TARGET_DIR` (e.g. `$HOME/.cache/kastellan-sdd-target`) or iterate on the DGX. Never pipe background cargo through `| tail` (masks the exit code, buffers output). [[mac-cargo-buildlock-prefer-dgx]]
- **Verify deployment claims before carrying them forward** — issue comments and handovers go stale; check the live host before repeating a claim in a spec, PR or ROADMAP. [[handover-claims-verify-before-carrying]]

---

## Working state

### Test baseline (authoritative)

| Host | Commit | Result | clippy `-D warnings` | `[SKIP]` |
| --- | --- | --- | --- | --- |
| **DGX** (native aarch64, real bwrap + KVM + live PG 18) | **`12809297`** — the tip of `fix/627-guard-tier-boot-payload`, after the five-agent review round | **3901 / 0 / 55**, 175 suites, `TEST_EXIT=0`, `--no-fail-fast --nocapture`. **Reconciles exactly: 3900 at `33029e32` + 1** — the one *net* new `#[test]` the review round adds (`a_clamped_row_reports_the_enforced_budget_not_the_derived_one`; the coverage-finding test was renamed and table-driven rather than added, so the diff's raw `#[test]` count is misleading and the name set is the honest instrument). All **11** `boot_report::tests::` names observed running. Ignored unchanged at 55 | exit 0 over **236** `Checking` lines from a cold private dir (`CARGO_TARGET_DIR=~/clippy-cold-631fix`), zero `warning`/`error` lines. **All 27 workspace crates named**, counted `sort -u` | **8**, all gliner-relex (4 venv-shim, 4 `ENABLE != "1"`) — *not* the bwrap-userns skip, so containment really ran |
| **DGX** (native aarch64, real bwrap + KVM + live PG 18) | **`33029e32`** — the tip of `fix/627-guard-tier-boot-payload` before the review round | **3900 / 0 / 55**, 175 suites, `TEST_EXIT=0`, `--no-fail-fast --nocapture`. **Reconciles exactly, by counting on both sides:** 3890 at `b65e44ab` **+ 10**, and `b65e44ab` is the tip that merged as `4aee83ad`, so it is content-identical to this branch's base — the rare case where the base is *measured* rather than inferred. All ten `boot_report::tests::` names observed in the log. Ignored unchanged at 55 — no new `#[ignore]` | exit 0 over **236** `Checking` lines from a cold private dir (`CARGO_TARGET_DIR=~/clippy-cold-627`), zero warnings. **All 27 workspace crates named in the log**, counted `sort -u` — the check that separates an honest full lint from a warm-cache exit 0 | **8**, all gliner-relex (4 venv-shim, 4 `ENABLE != "1"`) — *not* the bwrap-userns skip, so containment really ran |
| **Mac** (aarch64 darwin, Seatbelt + Postgres.app 18 via `KASTELLAN_PG_BIN_DIR`) | **`020b0e53`** — the tip of `fix/627-guard-tier-boot-payload` | **3778 / 0 / 25**, 175 suites, `TEST_EXIT=0`, `--no-fail-fast --nocapture`. **+10 over this branch's base, which is exactly the ten `#[test]`s the diff adds** — all ten observed running by name in the log, so the count is measured on both sides rather than subtracted. `main` itself has **not** been gated on the Mac (the last Mac row is `fix/615-616-618-guard-diagnostics` at 3748, two merges back), so the base is inferred and only the delta is measured; say that rather than round it up. Ignored unchanged at 25 — no new `#[ignore]`. **The first sweep of this tree failed one test** — `scheduler_ask_expiry_e2e::an_unanswered_ask_expires_and_fails_its_task_without_a_restart`, with the PG socket gone (`No such file or directory (os error 2)`) mid-test. That suite names nothing this branch touches; it passed on the re-run. Same family as the standing macOS full-sweep PG bring-up flake below | exit 0 over **214** `Checking` lines from a cold private dir (`CARGO_TARGET_DIR=~/clippy-cold-627`), zero warnings — an honest full-workspace lint, matching the historical Mac 213 range. `cargo doc -p kastellan-core --no-deps` reports **136** warnings on the Mac, none naming `boot_report`. Deliberately **not** compared to the 138/142/146 figures in the rows below: those are DGX runs of `cargo doc -p kastellan-core` *without* `--no-deps` (which counts 182 here), so the numbers are not commensurable and treating them as a trend would be the mistake | **26**, all Apple-`container` (service not started) + gliner-relex — *not* the sandbox skip |
| **DGX** (native aarch64, real bwrap + KVM + live PG 18) | **`b65e44ab`** — the TIP of `fix/624-boot-probe-samples`, after #625's review round | **3890 / 0 / 55**, 175 suites, `TEST_EXIT=0`, `--no-fail-fast --nocapture`. **Reconciles exactly, by counting:** 3889 at `37961f43` (the pre-review gate, same branch) **+ 1** — the single `#[test]` the review round adds (`the_summary_counts_the_samples_it_took_not_only_the_usable_ones`). The `sample.rs` -> `summary.rs` split in the same commit adds none: the whole-tree name set across both test files is 22 against 21 before, with that one named and nothing removed, which is the honest instrument when `git diff`'s rename detection makes the raw `+`/`-` count misleading. Ignored unchanged at 55 — no new `#[ignore]` | exit 0 over **241** `Checking` lines from a cold private dir (`CARGO_TARGET_DIR=~/clippy-cold-625r`). Also `cargo doc -p kastellan-core` exit 0 at **138** warnings, below `main`'s 142 and the pre-review tip's 146 | **8**, all gliner-relex — *not* the bwrap-userns skip, so containment really ran |
| **Mac** (aarch64 darwin, Seatbelt + Postgres.app 18 via `KASTELLAN_PG_BIN_DIR`) | tip of **`fix/615-616-618-guard-diagnostics`** | **3748 / 0 / 25**, 175 suites, `TEST_EXIT=0`, `--no-fail-fast --nocapture`. **Measured one line before the tip**: the clippy fix in the next column landed after this sweep and changes a runtime `assert!` in ONE `guard_tier_e2e` test into a `const` assertion, so it cannot reach another suite; `guard_tier_e2e` was re-run at the real tip (**20 / 0**, zero `[SKIP]`) and the cold clippy below IS at the tip. Said exactly rather than rounded up to "gated at the tip". **Reconciles exactly, and the arithmetic closes on both hosts:** the Mac was last measured at `09c6231f` (3718), **+4** for `a1c10da3` (that row reads 3721 passed *plus 1 load-flake*, so 3722 run) and **+10** for round two puts `main` at a predicted **3732**; this branch adds **16** `#[test]`s counted from the diff (2 `fetch_screen`, 5 `timeout/tests`, 4 `error_kind`, 3 `tier/tests`, 2 `guard_tier_e2e`) → 3748, measured. Ignored unchanged at 25 — no new `#[ignore]`. **The Mac has NOT been re-gated at `main` itself**, so 3732 is predicted, not measured; the 3748 is measured | exit 0 over **213** `Checking` lines from a cold private dir (`CARGO_TARGET_DIR=~/clippy-cold-diag3`), zero warnings — an honest full-workspace lint, matching the historical Mac range. **The first cold run FAILED** on `clippy::assertions_on_constants`: a fixture guard `assert!(5_000 < TIMEOUT_FLOOR_MS)` compares two constants. Now a `const _: () = assert!(..)`, which is strictly better — raising the floor past `pinned_cfg`'s 5 s would silently turn a below-floor test leg into a second in-band one, and this stops the build instead | **26**, all Apple-`container` (service not started) + gliner-relex. `guard_tier_e2e` ran all **20** with zero `[SKIP]`, including both new real-socket cases |
| **DGX** (native aarch64, real bwrap + KVM + live PG 18) | **`8cb8cfb7`** — tip of `feat/guard-live-bringup` after review round two, **squash-merged 2026-08-24 as `main` `45d5f6c2`** | **3854 / 0 / 55**, 175 suites, `TEST_EXIT=0`, `--no-fail-fast --nocapture`. **Reconciles exactly, and by measurement rather than subtraction:** 3840 at `09c6231f` **+ 4** (`a1c10da3`'s four `db::audit` tests — that commit was never DGX-gated, which was the open gap) **+ 10** (round two, all `db::audit`; `audit::tests` 15 → 25, confirmed by counting `^test audit::tests::` in the log). Ignored unchanged at 55 — round two added no `#[ignore]` | exit 0 over **245** `Checking` lines from a cold private dir (`CARGO_TARGET_DIR=~/clippy-cold-614`) | **8**, all gliner-relex — *not* the bwrap-userns skip |
Older rows (`09c6231f` 3840/3718, `69834357` 3823, `0bae6b2c` 3759, `f46c67cf` 3749, `2ab6612c` 3686, `b58edc77` 3668, and 3047 back to 2950) are in the [`archive/`](archive/) snapshots — most recently [`handover_20260823_wiring-slice_pre-prune.md`](archive/handover_20260823_wiring-slice_pre-prune.md) § Test baseline.

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

> One line per crate: what it owns and the invariants a change must not break. Exhaustive per-module detail (function names, wire shapes, every flag) lives in [`archive/handover_20260803_pre-prune.md`](archive/handover_20260803_pre-prune.md) and in the code. The root `README.md` Layout section is the user-facing version.

**Core & shared libraries**

- **`core`** (`kastellan-core`) — lib + 2 bins (`kastellan` daemon, `kastellan-cli`). Owns: the `tool_host` dispatch chokepoint (spawn-under-sandbox, lockdown-env derivation, wall-clock watchdog, secret-ref substitution in, output secret-scrub + injection screen out, three audit-emission arms), the scheduler + inner loop, CASSANDRA review stages, three-lane memory + recall + the L0/L1/L3 arcs, `worker_lifecycle` (SingleUse / IdleTimeout / `PersistentWorker`) and `force_route`, the `egress/` host side (sidecar spawn, policy rewrite, decision→audit, leak-scanner provisioning, upstream extra-CA selection), `channel/` (Matrix + gated email inbound, pairing, bus, and since #514 the `boot_supervisor` retry loop both channels bring up through), the secrets vault, the audit mirror, `workers/*` host-side manifests, `registry_build`, the handoff cache, and the installer. **Startup is fail-closed:** `db::probe::run` → `connect_runtime_pool` → `spawn_mirror` before `wait_for_shutdown`.
- **`db`** (`kastellan-db`) — Postgres layer + embedded migrations 0001–0021. Pure builders (initdb/auto_conf/bin-dir), `probe::run`, runtime-role separation, `audit`, `memories` (three lanes + `truncate_to_embedding_dim`, **EMBEDDING_DIM = 256**, Matryoshka), `graph`, `tool_allowlists` (argv0 vs domain kinds), `pairings`, `secrets` (AES-256-GCM + OS keyring). **`sqlx::migrate!` embeds at COMPILE time** — `touch db/src/lib.rs` after adding a migration ([[sqlx-migrate-embeds-at-compile-time]]).
- **`sandbox`** (`kastellan-sandbox`) — `SandboxPolicy` (+ `proxy_uds`, `persistent_store`) + `Net {Deny | Allowlist | ProxyEgress}` + `Profile {WorkerStrict | WorkerNetClient | WorkerBrowserClient | WorkerMlClient}` + the `dyn`-safe `SandboxBackend` trait. Backends: `LinuxBwrap` (in a `systemd-run --scope` cgroup), `MacosSeatbelt`, `MacosContainer` (opt-in), `LinuxFirecracker` (opt-in; guest kernel sha256-verified at every boot). **Invariants:** `build_argv()` stays a pure `SandboxPolicy → Vec<String>`; always `--unshare-all --die-with-parent --new-session --as-pid-1 --clearenv`; env only via `policy.env`; `fs_read` paths must be absolute; **`Net::Allowlist` + `proxy_uds` ⇒ private netns + UDS bind (no `--share-net`)**, legacy `Allowlist` without `proxy_uds` ⇒ `--share-net`.
- **`protocol`** — JSON-RPC 2.0 over stdio, MCP-stdio compatible. The sole IPC between core and workers.
- **`supervisor`** — `systemd --user` + launchd drivers and unit/plist generation; `core_service_spec` bakes `KASTELLAN_EGRESS_FORCE_ROUTING=1` into the generated unit. **Contract: install implies auto-start** — systemd via `systemctl --user enable` (added in #508), launchd via the unconditional `RunAtLoad=true`; `uninstall` must undo it. Honouring it on one OS only is a parity break. The contract stops at *arming* the unit: getting a per-user manager up on a host nobody logs into is the host's job on **both** platforms (linger on Linux, a GUI session on macOS), so "survives a reboot" is this contract plus a host-level arrangement. Both backends publish through **one** `cfg`-free `atomic_write` module (`.tmp.<pid>.<n>` staging name, `create_new`, cleanup after a failed publish) — deliberately shared, not per-backend, so its tests run on both hosts; a destination-derived tmp name races between concurrent writers.
- **`llm-router`** — the sole core-side LLM egress. `Router::send`/`embed`, `Backend {Local, Frontier}`, `PolicyGate` (Frontier denied until Phase 5), `RouterConfig::from_env`, `disable_thinking` default-ON.
- **`leak-scan`** — pure credential-leak scanner: `fingerprint` (Rabin + SHA-256), `RollingMatcher` (streaming, used by the proxy to BLOCK), `redact` (all-hits, used by core to SCRUB).
- **`net-classify`** — the pure SSRF/denied-range predicate `is_denied_range` + its 12 tests. Sole consumer today: egress-proxy.
- **`tests-common`** — shared dev-dep harness (`publish = false`, never linked into a runtime binary): `PgCluster`, RAII guards, skip helpers, sandbox factory, binary discovery, `daemon.rs`, `scripted_llm`, `mock_localmail`, `egress_forcing`, `microvm`, `short_scratch_root`.

**Workers (18 Rust + 2 Python)** — one process, one sandbox each; no shared process or jail.

- **`prelude`** — Linux Landlock + seccomp `lock_down` (no-op on macOS) + cross-platform `setrlimit`. Profiles: Strict / NetClient / BrowserClient / MlClient / MatrixClient. **The filter installs with `SECCOMP_FILTER_FLAG_TSYNC`** so it covers every thread — without it the filter was a no-op on the Matrix worker's tokio threads. Also ships `kastellan-worker-lockdown-exec`, the exec-shim giving pure-Python venv workers worker-side seccomp + Landlock.
- **`egress-proxy`** — the per-worker egress boundary, all four slices done. Per CONNECT: allowlist → self-resolved DNS → `is_denied_range` → pin+dial → 200 → peek first byte → MITM or transparent tunnel. Owns the ephemeral per-instance CA, the leaf cache, the leak-scanning relay, TLS pinning, and the upstream extra-CA seam. **Builds + validates the upstream TLS config BEFORE binding the UDS or exporting `ca.pem`** (the readiness signal) — reversing that order returns a healthy handle for a dying proxy.
- **`shell-exec`** — argv-allowlisted execve wrapper. Entries must be **absolute paths**; the daemon loads the allowlist **once at startup**; the jail has no PATH, so the agent must emit an absolute `argv[0]` ([[shell-exec-allowlist-operational]]).
- **`mail`** — six read-only `mail.*` tools over localmail `/v1`; `get_attachment` delivers to a durable per-task out dir. Live-verified against the real 37 k-message archive.
- **`email-in`** — the Phase-2 email fallback channel's inbound poller (`email.init`/`poll`/`ack`) over localmail's server-side subscription cursors; no IMAP crate in-jail, no mail credentials in a kastellan sandbox; `POLL_INTERVAL` 5 s. Gate + channel are core-side (`core/src/channel/email/`); unset `KASTELLAN_EMAIL_*` ⇒ byte-identical daemon, misconfig ⇒ loud `EMAIL CHANNEL DISABLED` with the daemon still running.
- **`web-common`** — shared lib for net workers: `HostAllowlist` (host-only + port-scoped), the `HttpGet` seam + `ReqwestGet` + `ProxyConnectGet` (CONNECT-over-UDS, end-to-end TLS), and the feature-gated `search` / `fetch` / `extract` logic the three web workers share.
- **`web-fetch`** / **`web-search`** / **`web-research`** — HTTPS-only `web.fetch`; `web.search` against an operator-set SearxNG endpoint; and the composite `web.research` (search → filter → fetch top-N → chunk → BM25-rank passages). **`web-research`'s `from_env` fails closed if the endpoint host is off-allowlist** — a force-routed e2e that omits it times out looking like a transport bug ([[web-research-e2e-endpoint-must-be-allowlisted]]).
- **`python-exec`** — the Phase-4 executor for agent-authored Python (opt-in). Strictest policy of any worker: `Net::Deny` + `WorkerStrict` + `fs_write=[]` + curated stdlib (`-I -S -B`), cpu 10 s / mem 512 MiB / wall 30 s, SingleUse. Params ≤64 KiB ride an env var, larger go to a 0600 scratch file, over-ceiling fails closed.
- **`matrix`** + **`matrix-wire`** — the live Matrix inbound worker (`LiveSdk` behind the `live-matrix` feature, restore-or-login persisted session, `ProxyBridge` for egress) and its shared wire types.
- **`microvm-run`** + **`microvm-init`** — the Firecracker launcher (pure-std, is the Child) and the guest PID1 vsock-stdio adapter.
- **`embed-broker`** / **`search-broker`** — the two trusted worker-side exceptions to "no worker talks to the model layer".
- **`kv-demo`** / **`net-demo`** — long-lived-worker fixtures for the 5b/5c VM arcs (persistent store; net-in-a-VM through a transparent-tunnel sidecar).
- **Python, outside Cargo:** `browser-driver` (Playwright read-only render; in-jail `ProxyShim` bridges Chromium's CONNECT to the sidecar UDS) and `gliner-relex` (relation extraction; `WorkerMlClient`).

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

Newest first, one line each. Full narrative in git, the linked PRs, and [`archive/handover_20260803_pre-prune.md`](archive/handover_20260803_pre-prune.md).

- **2026-08-27 `4aee83ad` — [#625](https://github.com/hherb/kastellan/pull/625): the boot probe measured the boot, not the host** (closes [#624](https://github.com/hherb/kastellan/issues/624)). D9's probe took one sample ~3 s into daemon startup and measured **startup contention**: three consecutive boots on one unchanged DGX backend read 6 073 / 269.6 / 1 582 tok/s where the same backend measured a reproducible ~7 000 uncontended — a 26x under-measurement whose slowest boot fired a **false** ceiling finding. Fixed by D11: `PROBE_SAMPLES` (3) samples, keep the **fastest**, each with its own cache-buster. `TimeoutBasis::Probed` gained `slowest_tok_per_s` + `measured_samples` + `attempted_samples` so one row distinguishes a quiet host from a busy one. Three movement-only splits (`timeout/` is now four files, `tier/probe.rs` lifted out of `tier/boot.rs`). Five-agent review found one CRITICAL — `summarise(&samples[..1])` silently reverted the whole fix and passed every guard test in the tree. Gated at the branch tip `b65e44ab`: **DGX 3890 / 0 / 55**, 175 suites, cold clippy exit 0 over 241 `Checking` lines. Deferred: [#626](https://github.com/hherb/kastellan/issues/626), [#627](https://github.com/hherb/kastellan/issues/627). **Not yet deployed to the DGX.**
- **2026-08-24 `45d5f6c2` — [#614](https://github.com/hherb/kastellan/pull/614): the guard tier's first live bring-up, and the audit cap that was eating its scores.** The tier ran in production for the first time (DGX): probed 21 752 ms budget at 6 073 tok/s, a cleared document at `p = 0.0074`, and **a real attack document blocked at `p = 0.9199` where the deterministic catalogue scored `0.0`**. The bring-up found in an hour what seventeen e2e cases and a five-agent review had not: `truncate_payload` replaced any over-cap payload *in its entirety*, so every tool result past ~4 KiB took D5's guard score with it — biased the wrong way, since blocks keep their score and *clears* lose theirs. Fixed by `db::audit::PRESERVED_KEYS` + `preserve_onto`, with `AuditSink::insert` now a **provided method** applying the transform before delegating to `insert_stored`, so no sink double can record a payload Postgres never stored. Two four-agent review rounds, mutation-proven six for six in round two. Gated at the branch tip `8cb8cfb7`: **DGX 3854 / 0 / 55**, 175 suites, cold clippy exit 0 over 245 `Checking` lines. Filed out of it: [#612](https://github.com/hherb/kastellan/issues/612) and [#615](https://github.com/hherb/kastellan/issues/615)–[#618](https://github.com/hherb/kastellan/issues/618) — **and the merge itself wrongly auto-closed #612 and #615**, since reopened; see the header and [Standing hazards](#standing-hazards-that-have-each-cost-a-session).
- **2026-08-23 `8736f559` — [#607](https://github.com/hherb/kastellan/pull/607): the guard tier wired to the `tool_host` chokepoint** (closes [#586](https://github.com/hherb/kastellan/issues/586)). `ToolHostStepDispatcher` carries `guard: Option<Arc<GuardTier>>`; catalogue first and a catalogue Block short-circuits; escalate-up only. D8 turns #604's attacker-reachable HTTP 400 into a **boot refusal** (`/props` must report a per-request context ≥ 66 048), D9 **probes** the timeout at boot instead of assuming D2's 15 s, D10 pins the tier as **advisory** at 65% recall. Eleven post-review fixes, all mutation-proven; deferred [#608](https://github.com/hherb/kastellan/issues/608)–[#611](https://github.com/hherb/kastellan/issues/611). Gated at `31a05e00`: **DGX 3834 / 0 / 54**, **Mac 3712 / 0 / 24**. **Never yet run live** — see [Current state](#current-state).
- **2026-08-22 `abb3d3a7` — [#598](https://github.com/hherb/kastellan/pull/598): the guard weights pinned by BYTES, checked at use** (closes [#592](https://github.com/hherb/kastellan/issues/592)). `guard calibrate` GETs `/props`, takes `model_path`, hashes that file itself, and refuses **before scoring anything** on any of five distinct failures; `--weights-unpinned` keeps a candidate model calibratable and stamps the report `UNPINNED`. Gated at the branch tip `f46c67cf`: **3749 / 0 / 54**, whose tree differs from `abb3d3a7` only in HANDOVER + ROADMAP. Deferred to [#599](https://github.com/hherb/kastellan/issues/599) + [#600](https://github.com/hherb/kastellan/issues/600); [#597](https://github.com/hherb/kastellan/issues/597) filed for the projector. Full detail in [Current state](#current-state).
- **2026-08-22 `2ab6612c` — [#596](https://github.com/hherb/kastellan/pull/596): the four fail-opens #593's review found after it merged.** `--record` disabled every hash check; an empty budget scope made D7's criterion vacuous; τ printed at `{:.6}`, which does not round-trip an f32; the HTTP status was never checked, so a 404 page could be pinned wearing the label of the page it replaced. +18 tests. **Merged on a Mac gate only — the DGX gate was run afterwards, at `2ab6612c`: 3686 / 0 / 54.**
- **2026-08-22 `b58edc77` — [#593](https://github.com/hherb/kastellan/pull/593): measurement-3 Tasks 1–4 + the live pilot.** D7's `operating_point` + `BudgetScope`, the operating point rendered once corpus-wide, the metadata-only `manifest` module, and `guard capture` through the real chokepoint. M1 discharged; both slices specced.
- **2026-08-22 `47ba5b4f` — [#587](https://github.com/hherb/kastellan/pull/587): exact ask containment, honest UX, diagnosable rejections** (closes [#582](https://github.com/hherb/kastellan/issues/582) + [#583](https://github.com/hherb/kastellan/issues/583) + [#584](https://github.com/hherb/kastellan/issues/584)). `handle_inbound`'s two ask arms became four; containment is an exact peer-scoped live-nonce lookup sharing `resolve_with_nonce`'s own `WHERE` fragment (`live_ask_for_claimant!`), `looks_like_ask_command` survives demoted to the cheap DB-free gate, and every `channel.ask_answer_rejected` row names its arm. Full narrative — including the two fail-open Criticals the five-agent review found, and why one of them was a regression against `main` that no agent found — in [Current state](#current-state). Deferred: [#588](https://github.com/hherb/kastellan/issues/588)–[#591](https://github.com/hherb/kastellan/issues/591).
- **2026-08-21 `f90631da` — [#585](https://github.com/hherb/kastellan/pull/585): the Shieldstral adjudicator, guard-model slice 1.** Guard endpoint seam (`RouterConfig::{guard_url,guard_model}` + the pure `for_guard`, which never falls back to `local_url`), the adjudicator (`cassandra::guard_model`: digest-pinned prompt artefact, pure three-valued `decide`, thin async shell), and an offline calibration harness (`core::guard_calibration` + `kastellan-cli guard calibrate` + 24 seeded corpus cases). **No production wiring** — five chokepoint files byte-identical to `main`, verified as a merge gate. DGX **3599 / 0 / 54**. Full detail in [Current state](#current-state); the wiring slice's three preconditions are in [Next TODO](#next-todo).
- **2026-08-20 `bb937df7` — [#579](https://github.com/hherb/kastellan/pull/579): [#564](https://github.com/hherb/kastellan/issues/564) slice 2, the ask channel.** An escalated plan's question reaches the operator's Matrix room; they answer `/approve <token>` / `/deny <token>`. `channel::outbox::ChannelOutbox` (shared `Arc` registry created in `main` before both scheduler and channel supervisors), D16's peer-scoped `EXISTS` predicate *inside* `resolve_with_nonce`'s guarded UPDATE (the nonce is a **bearer** token, so reading — not guessing — was the real threat), D17's `NONCE_BYTES` 32 → 5. Gated **3527 / 0 / 53** DGX. **Dormant in production: nothing constructs `Verdict::Escalate`** — see the warning at the top of this file.
- **2026-08-19 `af3e7e66` — [#578](https://github.com/hherb/kastellan/pull/578): [#564](https://github.com/hherb/kastellan/issues/564) slice 1b, the ask path.** `Verdict::Escalate` stops degrading to `Block`; `Outcome::{AwaitingOperator, Denied}`, `final_state() -> Option<&'static str>`, the 60 s expiry sweep, `kastellan-cli inbox`, and D11's `asks.resume_state` (migration 0024) so a resumed task does not re-execute steps it already ran. Closes [#571](https://github.com/hherb/kastellan/issues/571). Gated **3462 / 0 / 53** DGX.
- **2026-08-18 `fbe91c4d` + `e8ea4339` — [#572](https://github.com/hherb/kastellan/pull/572) / [#573](https://github.com/hherb/kastellan/pull/573): mail attachments addressed by `{message_id, filename}`,** plus the nine defects the post-gate review found. `method_qualify` completes an omitted namespace at dispatch; `qualify_plan_methods` normalises `plan.steps` **once** so the output cap, the digest and both audit payloads agree. Carries #574, the `/tmp`-wipe fix. Gated **3416 / 0 / 53** DGX; live-verified on both hosts.
- **2026-08-16 `07b6451e` — [#569](https://github.com/hherb/kastellan/pull/569): the guard tier's measurement 2 + the Q8 re-measurement, and `logprobs` plumbing.** Pinned runtime + quantisation: llama.cpp + `Shieldstral-1.0-3B-Q8_0` on **both** hosts, so one fitted τ transfers. Additive only — no existing caller passes the new fields.
- **2026-08-13 `d8f6acfd` — [#555](https://github.com/hherb/kastellan/pull/555), closes [#545](https://github.com/hherb/kastellan/issues/545) + [#541](https://github.com/hherb/kastellan/issues/541) + [#544](https://github.com/hherb/kastellan/issues/544) + [#542](https://github.com/hherb/kastellan/issues/542).** The `<tools>` advertisement made bounded (4 KiB over the *escaped* text, whole entries only), kind-correct (a row whose `kind` disagrees with its tool's is withheld from the *advertisement* only — enforcement stays kind-blind, [#554](https://github.com/hherb/kastellan/issues/554)), unforgeable (`char::is_control()` + separators + bidi, replacing `< 0x20`) and unrepresentable-when-half-declared (one `AllowlistDecl`). `/fixall` added a third advertisement state (`with_opaque_allowlist`) so an all-mismatched-kind allowlist stops reading as "nothing is permitted". Gated on both hosts — and the DGX gate caught that `cargo test --workspace` **did not compile on Linux**, because #541's type change reached two `cfg(target_os = "linux")` `ResolveCtx` fakes the Mac never compiles. Eleven mutations, all killed. Details in [Current state](#current-state).
**July 2026 and earlier, one line each** (full entries in [`archive/handover_20260824_diagnostics_pre-prune.md`](archive/handover_20260824_diagnostics_pre-prune.md) § Recently merged, and in the linked PRs):

- `bf8e850b` [#496](https://github.com/hherb/kastellan/pull/496) — Phase-2 email fallback channel, slice 1 (gated inbound). Nine pre-merge review fixes, two High: a live DMARC-gate bypass via a legal RFC 8601 `method-version`, and 401/403 treated as permanent, which **destroyed mail**.
- `0be03b30` [#495](https://github.com/hherb/kastellan/pull/495) + `c0ac4e62` [#493](https://github.com/hherb/kastellan/pull/493) — the egress-proxy **upstream extra-CA** seam and the force-routed mail round trip; also a real ordering bug (the proxy validated upstream TLS *after* publishing readiness, so a fail-closed abort returned a healthy handle for a dying proxy). See [Egress / MITM traps](#egress--mitm-traps-from-491503).
- `efc1001b` [#490](https://github.com/hherb/kastellan/pull/490) — mail-worker live-test coverage; `ce144513`/`87afd8b2` [#483](https://github.com/hherb/kastellan/pull/483)/[#487](https://github.com/hherb/kastellan/pull/487) — `kastellan-worker-mail`, first LIVE run against the real archive [[mail-worker-localmail-verification]].
- `e1d37633` [#486](https://github.com/hherb/kastellan/pull/486) — install-dir trust probe, keyring read-back-verify, merged secret-scrub spans. **The audit-remediation family is FULLY closed.**
- `06700212` [#482](https://github.com/hherb/kastellan/pull/482) — bwrap binds canonical-src → original-dest (TOCTOU-safe). Host-source paths only; guest-side paths stay lexical.
- `dd10bd68` → `c1fdb07c` → `4c03929f` — the **provisioning-integrity family**: guest kernel sha256-pinned in one shared `lib/guest-kernel.sh` and verified **at every VM boot**, image dir `root:<worker-group>` 1775. Sums are TOFU — documented honestly.
- `61890c48` / `1f353dd8` / `02ef016c` — the **VM-entry arc COMPLETE**: first real page rendered inside a micro-VM through a real sidecar, both slice-2 budgets measured. **Correction that keeps being needed:** this does NOT fix macOS [#286](https://github.com/hherb/kastellan/issues/286) — Firecracker is Linux-only; #286's named fix is the `MacosContainer` VM-netns backend ([#55](https://github.com/hherb/kastellan/issues/55)).

### Earlier history

One bullet per session, newest first, in [`archive/handover_20260803_pre-prune.md`](archive/handover_20260803_pre-prune.md) § "Earlier history" — covering the Firecracker micro-VM slices 1–5c, the python-exec warm/idle arc, the Matrix worker hardening + live-channel arc, the planner-feedback arc (#337–#340), the entity/L1-embedding arc, the L3 skill arc, the egress-proxy slices #1–#4, the comms/channel-bus slices, the crates.io 0.1.0 release and the hhagent→kastellan rename. Older snapshots: [`20260727`](archive/handover_20260727_pre-prune.md), [`20260719`](archive/handover_20260719_pre-prune.md), [`20260629`](archive/handover_20260629_pre-prune.md), [`20260615`](archive/handover_20260615_pre-prune.md), [`20260611`](archive/handover_20260611_pre-prune.md), [`20260605`](archive/handover_20260605_pre-prune.md), [`20260529`](archive/handover_20260529_pre-prune.md), [`20260510`](archive/handover_20260510_pre-prune.md).

---

## Open follow-up issues (filed but not picked)

Beyond those already listed under [Next TODO](#next-todo). Only currently-open issues; closed-issue detail lives in the archive snapshots and git history.

- **From [#587](https://github.com/hherb/kastellan/pull/587)'s review, all four deferred with reasoning rather than folded in:** [#588](https://github.com/hherb/kastellan/issues/588) — the shared `live_ask_for_claimant!` fragment's `$2`/`$3` bind contract is doc-only, and because both binds are `text` a transposition **type-checks and returns zero rows**, which is fail-*closed* in `resolve_with_nonce` and fail-**open** in `any_live_nonce_for_claimant`; the agreement test that would catch it is `harness()`-gated skip-as-pass. [#589](https://github.com/hherb/kastellan/issues/589) — `AskRejectReason` enum + a `Containment` newtype, so `?` inside the inverted-polarity `containment_refusal` becomes a **type error** instead of a paragraph-prohibited idiom that reads as ordinary Rust. [#590](https://github.com/hherb/kastellan/issues/590) — seal `AskResolver`: a public trait in a published crate whose external impl decides a containment outcome, and could be fail-open. [#591](https://github.com/hherb/kastellan/issues/591) — the char-boundary truncation walk is hand-written in five places and its *correct* test exists in only one.
- ~~[#592](https://github.com/hherb/kastellan/issues/592)~~ **CLOSED by `abb3d3a7`** ([#598](https://github.com/hherb/kastellan/pull/598)) — the pin is checked at use, so the measurement-3 spec's D6 is unblocked. See [Current state](#current-state).
- **From [#598](https://github.com/hherb/kastellan/pull/598)'s review, both deferred because each changes another tool's contract:** [#599](https://github.com/hherb/kastellan/issues/599) — `--weights-unpinned` still exits **0**, so nothing machine-readable separates a τ fitted on the pinned bytes from one fitted against a server we could not identify at all; the artefact says `UNPINNED` loudly but the exit status does not, and `guard_calibrate_cli_e2e` now passes the flag on every leg, which is how a flag becomes habitual. [#600](https://github.com/hherb/kastellan/issues/600) — `scripts/eval/run-shieldstral-llamacpp.sh`, **the one script in the tree that launches a Shieldstral server**, still checks only that `$MODEL` exists; `require_guard_weights` has no automated caller at all, which is weaker than the `require_guest_kernel` precedent it cites.
- **From measurement 3 (2026-08-23), five filed with the evidence that found each:** [#601](https://github.com/hherb/kastellan/issues/601) — `guard capture` admits a document under `Relaxed` (production's profile for `web-fetch`, via `for_tool`) and `guard calibrate` then excludes on `Strict` (`screen()`), so the corpus is filtered by one gate and scored for exclusion by a stricter one; **quantified as inert for this run** (0 captured cases excluded), still wrong. [#602](https://github.com/hherb/kastellan/issues/602) — a rate-limited **200 with an empty or truncated body** is hashed and, under `--record`, pinned *as the case*; measured `e3b0c442…` (the empty-string sha256) from a real fetch with curl exiting 0. #596 closed this for 404s and never checked the body. Fail-**open**. [#603](https://github.com/hherb/kastellan/issues/603) — the pin covers the **final URL**, so a Wayback redirect to an equivalent snapshot reads as `The source has drifted` when the document is byte-identical; fail-noisy, and it trains an operator to look past the campaign's loudest signal. [#604](https://github.com/hherb/kastellan/issues/604) — **`SCAN_BYTE_CAP` bounds bytes, not tokens**: 65,536 bytes tokenised to **44,437** and the adjudication died on HTTP 400, because the byte→token ratio is **attacker-controlled** (M1's prose 6.5 B/token, dense jailbreak text 1.47). [#605](https://github.com/hherb/kastellan/issues/605) — the `PROVISIONAL` banner is an unconditional `push_str` stating a criterion it does not check, so the one line separating a proof-of-concept τ from a fitted one can never change.
- **From [#625](https://github.com/hherb/kastellan/pull/625)'s five-agent review, two filed rather than folded in because each is a decision rather than a fix:** [#626](https://github.com/hherb/kastellan/issues/626) — because `PROBE_TOTAL_BUDGET_MS == PROBE_BUDGET_MS`, a **saturating FIRST sample still ends the probe at one unrepresentative sample** and fires the false ceiling finding (the cold-`llama-server` case). #624 fixed the *contention* half of that defect only. The fix is a total budget larger than one sample's, which costs up to 60 s of daemon startup on the host that is already sickest — or, cheaper and arguably more honest, weakening the *finding* when `measured_samples == 0 && attempted_samples == 1`. [#627](https://github.com/hherb/kastellan/issues/627) — `report_guard_tier` is private to the binary with **no `cfg(test)` module**, so swapping `tok_per_s` and `slowest_tok_per_s` in the payload (which inverts the documented `slowest < tok_per_s / 2` operator query) is silent, as is deleting any of the three new keys. Extract a pure `guard_tier_boot_payload(...) -> Value` into the lib.
- **From [#614](https://github.com/hherb/kastellan/pull/614)'s round-two review, four filed rather than folded in because each changes behaviour beyond the branch:** [#615](https://github.com/hherb/kastellan/issues/615) — an operator-pinned `KASTELLAN_LLM_GUARD_TIMEOUT_MS` **below `TIMEOUT_FLOOR_MS` or above `TIMEOUT_CEILING_MS` is accepted in silence** (`validate_operator_timeout` refuses only `0`, and `TimeoutBasis::Operator` yields no `coverage_finding`). Not clamping is deliberate and should stay; saying *nothing* is the defect — sharpened by #612 telling Metal operators to pin ~3× the ceiling. [#616](https://github.com/hherb/kastellan/issues/616) — `guard.state` collapses timeout / connect / HTTP-status / decode into `"router_error"`, so the durable record **cannot count the fail-open** that #612 is entirely about; a closed enum discriminant (`error_kind`) carries no attacker-controlled bytes and would fix it without weakening the no-backend-text rule. [#617](https://github.com/hherb/kastellan/issues/617) — `req` is still lost wholesale above the cap, and for `shell.exec` **`req.argv` *is* the audited act**; the allowlist is the wrong tool (unbounded), a bounded producer-side summary is the right one. [#618](https://github.com/hherb/kastellan/issues/618) — `fetch_screen`'s Block arm has an else-less `as_object_mut`, a silent fail-open *shape* on a screening path (unreachable today via the `get("data")` guard three lines up).
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
