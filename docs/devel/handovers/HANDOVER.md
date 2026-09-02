# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260826_624_pre-prune.md`](archive/handover_20260826_624_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here,
> including the full #619, #615/#616/#618 and live-bring-up write-ups compressed below.

**Last updated:** 2026-09-02 (second session) · **DGX RUNNING `121f22a2` — the whole guard arc is
DEPLOYED** (see [Merged work, compressed](#merged-work-compressed--the-guard-arc-and-the-2026-09-02-deploy)). ·
**`main` HEAD:** `e5cb6bfc` —
[#648](https://github.com/hherb/kastellan/pull/648), **docs-only**, on top of
`121f22a2` ([#645](https://github.com/hherb/kastellan/pull/645): #641, #642, #643, plus a
movement-only `LlmEndpoint` split) and `466ca7ff`
([#640](https://github.com/hherb/kastellan/pull/640), #632 + #634). ·
**OPEN BRANCH: `fix/649-transformers-lock-bump`** — the #649 security bump; see
[#649](#649--the-transformers-advisory-and-the-two-faults-it-uncovered).

> ⚠️ **The DGX checkout is left on `fix/649-transformers-lock-bump`, and its gliner-relex `.venv`
> is rebuilt from that branch's lock** (transformers 5.13.1). Neither affects the running daemon,
> which executes the *installed* `121f22a2` binaries. `git checkout main` there after #651 merges.

> ⚠️ **THE DGX WORKSPACE IS NO LONGER GREEN, and that is an honest change, not a regression.**
> Rebuilding the gliner-relex venv (it had been a broken **macOS** copy, so four tests had been
> skipping-as-passing for months) made those tests actually run — and three of them fail, on a
> **production** defect now filed as [#650](https://github.com/hherb/kastellan/issues/650).
> Proved pre-existing by A/B: the same tests fail identically with the pre-bump lock. **The next
> session's first job is #650**; the operator has already chosen to take it as its own PR right
> after #649 merges.

**Last gate: DGX over the `fix/649-transformers-lock-bump` tip `5445dd68` — 3937 / 3 / 55, 176
suites, `TEST_EXIT=101`, and 4 `[SKIP]` (was 8).** The total is unchanged at **3940**: the three
failures are tests that had been *skipping-as-passing* and now honestly fail, all on
[#650](https://github.com/hherb/kastellan/issues/650). The four venv-shim skips are **gone**, which
is #649's acceptance criterion met literally. Plus both Python legs green — `51 / 0` on the Mac and
`51 / 0` on the DGX **with `KASTELLAN_GLINER_RELEX_REQUIRE_E2E=1`**, so the real 1.3 GB model load
provably ran under transformers 5.13.1. Both hosts on rustc **1.98.0** (CI parity). See
[Test baseline](#test-baseline-authoritative).

> **The last fully green gate is DGX `f12ed26d` — 3940 / 0 / 55**, tree-identical to `main`
> `121f22a2` (`git diff` empty; a squash commit's tree is how a branch gate carries over — check
> content, never `merge-base`).

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

### #649 — the transformers advisory, and the two faults it uncovered

Branch `fix/649-transformers-lock-bump`. GHSA-xrqw-3rrv-vx5w (high, `save_pretrained` path
traversal) covered every `transformers < 5.10.0` in `workers/gliner-relex/uv.lock`.

- **The remedy the issue stated does not work, and says nothing while failing.**
  `uv lock --upgrade-package transformers` reaches **5.6.2 — still inside the vulnerable range —
  and exits 0**, because `gliner 0.2.27` declares `transformers<5.7.0`. `gliner 0.2.28` raised that
  cap to `<5.14.0`; **bumping transformers alone cannot fix this advisory.** Dependabot's own
  [#647](https://github.com/hherb/kastellan/pull/647) had it right (it moved both) but only in the
  lock. Both floors now move in **`pyproject.toml`**, which makes the vulnerable range
  *unsatisfiable* rather than merely unlocked, and turns a `gliner` downgrade into a hard conflict.
  Resolves to transformers **5.13.1**, gliner 0.2.28, safetensors 0.8.0.
- **The cuda-toolkit marker rewrite in the lock is uv's own normalisation, not a widening** — `torch`
  pulls `cuda-toolkit[..]` behind `sys_platform == 'linux'`, so the parent edge already carries the
  guard the child markers drop. Our uv (0.11.29) emits exactly what dependabot's did; the old lock
  was written by uv 0.10.x. Don't re-litigate it.
- **Reachability re-checked, not taken on trust** (the issue asked): `save_pretrained` appears
  nowhere in the worker's `src/`, `tests/` or README — it only *loads* weights — and the worker is
  sandboxed like every other one. "Free to fix", not "we are exposed".
- **Both venvs were stale, and one was the wrong OS.** The Mac's held transformers 5.1.0 and a
  package still named `hhagent-worker-gliner-relex`; the **DGX's `bin/python` symlinked to a macOS
  path** (`/Users/hherb/.local/share/uv/python/cpython-3.12.6-macos-aarch64-none/…`). A venv is
  gitignored, so nothing in the repo could have told you this — check
  `.venv/bin/python` with `readlink` before believing a `[SKIP]`.
- **The suite could not have caught a broken model load, and its stated cover did not exist.**
  `conftest.py` pointed at "the manual smoke test (operator-runnable, not in CI)"; there was no such
  artifact, only a one-off from May 2026. `tests/test_model_live.py` is that smoke test made
  repeatable — a real 1.3 GB load plus one extraction — and
  **`KASTELLAN_GLINER_RELEX_REQUIRE_E2E=1` turns its skip into a failure** (the knob this file has
  wanted for a while). All three arms verified: runs with weights, skips with a reason naming the
  path, fails under the knob. **63/63 on the Mac** after the review round (51 before), and 51/51 on
  the DGX with the knob on, so the live load really ran (transformers 5.13.1 / torch 2.13.0+cu130).
  ⚠️ **The DGX has not re-run since the review round** — it is owed a fresh `pytest` and workspace
  gate.
- **`uv.lock` was the one committed lockfile with no drift gate.** Every cargo step passes
  `--locked`; nothing checked that `pyproject.toml` and `uv.lock` agree. New `python-lock-check` CI
  job runs `uv lock --check --offline` — hermetic (validates from the lock's own metadata, no PyPI
  round-trip), both arms verified. The mocked pytest suite stays out of CI (~2 GB of torch); the
  torch-free guard tests *are* in, deselecting the live model test explicitly.

#### The #651 review round (2026-09-02, third session)

A six-agent review of #651 found one thing the PR had missed outright and several it had oversold.
All fixed on the branch; three follow-ups filed. **What is worth carrying:**

- **A "fix all N call sites" commit fixed two of three.** `memory_entity_link_e2e.rs` was the third
  host-mode fixture with the identical hardcoded `interpreter_root: None`, and the commit message
  said "the e2e **fixtures**" (plural). Its own in-file comment — *"if a third caller appears, lift
  them into `kastellan-tests-common`"* — had already tripped its trigger condition and been ignored.
  **When a comment names the condition under which to refactor, check whether it has fired before
  adding the caller.**
- **Now lifted, and the lift found a second drift.** `kastellan-tests-common` gained
  [`gliner_weights`](../../../tests-common/src/gliner_weights.rs) (pure `weights_dir_candidate` + 10
  unit tests) and [`venv_interpreter`](../../../tests-common/src/venv_interpreter.rs). The three
  copied `resolve_weights_dir`s were **not** equivalent: `gliner_relex_e2e`'s ignored
  `KASTELLAN_GLINER_RELEX_WEIGHTS_DIR` entirely while the other two honoured it, so one override
  could point two suites at different snapshots in the same `cargo test` run.
- **`(None, vec![])` from `resolve_host_interpreter_binds` is ambiguous three ways** —
  self-contained venv (fine), interpreter that does not resolve (dead fixture), dep tool missing
  (under-bound jail). The fixtures accepted all three alike, so a `.venv` staged on another host
  produced *exactly* the `interpreter_root: None` the PR had just deleted as broken.
  `venv_interpreter_binds` now checks the `None`: it panics naming the path and the remedy.
- **A skip-as-pass knob that only reads itself from inside the test is not a gate.** Proven:
  `KASTELLAN_GLINER_RELEX_REQUIRE_E2E=1 pytest -k "not real_model"` exited **0**, and a rename off
  the `test_` prefix exited 0 with *no deselected counter to notice*. `conftest.py` now carries a
  `pytest_sessionfinish` guard; both holes exit 1. **`trylast=True` on
  `pytest_collection_modifyitems` is load-bearing** — `-k` deselection is itself such a hook, so
  without it you see the pre-filter list and misreport a deselected test as present.
- **Simulate a CI step in a venv-free copy before believing it.** The new torch-free pytest step
  passed locally for the wrong reason (it silently reused the on-disk `.venv`). In a faithful copy
  it **failed**: under `--no-project` the worker package is unimportable, so on any host that *does*
  have weights the live test errors instead of skipping. Now deselected explicitly.
- **The gate does not cover the advisory class, and the comment now says so.** `uv lock --check`
  passes on `origin/main`'s vulnerable 5.5.0 state with exit 0 — floor and lock agreed perfectly.
  Advisories are Dependabot's job (alerts are enabled; that is what filed #649 and opened #647).
  What the gate *does* catch is a silently **weakened** floor, verified exit 1.
- **Two review findings were wrong and were dropped**, both asserted by more than one agent: "no
  advisory scanning anywhere" (inferred from a missing `.github/dependabot.yml` — security updates
  need no config file) and `gliner_relex_e2e.rs:161` as a fourth unfixed site (it is container mode,
  where `None` is correct). [[verify-deployment-claims-before-carrying]] applies to agent findings
  too.

Follow-ups filed, none blocking: **[#653](https://github.com/hherb/kastellan/issues/653)** (the Rust
e2es still cannot be forced to run — five silent `[SKIP]` gates, and that is how the fixture bug
survived), **[#654](https://github.com/hherb/kastellan/issues/654)** (three fixtures gate on a strict
`Some("1")` while production takes the #459 `1|true|yes|on` dialect; `resolve.rs` comments still
teach the old rule), **[#655](https://github.com/hherb/kastellan/issues/655)** (`main` has **no**
required status checks — clippy, the matrix build and the new lock gate can all go red and merge).

### #650 — the fault the acceptance run exposed (OPEN, next session's first job)

[#650](https://github.com/hherb/kastellan/issues/650). **Production, two workers, pre-existing.**
`uv` lays managed interpreters out as `cpython-3.13.14-linux-aarch64-gnu/` with a minor-version
**symlink alias** `cpython-3.13-linux-aarch64-gnu` beside it, and the venv's `bin/python` — hence
every console-script shebang — points at the **alias**. `interpreter_deps::resolve_interpreter_root`
**canonicalizes**, so only the `.14` directory is bound into the jail; the alias lives one level up
in an unbound parent. Inside bwrap `.venv/bin/python` dangles and `execve` returns **ENOENT for a
file that is present and readable**.

- **It surfaces as `Protocol(EarlyExit)` and nothing else** — which is
  [#613](https://github.com/hherb/kastellan/issues/613)'s subject. Getting the real message took
  dumping `bwrap_argv` from `linux_bwrap::spawn_under_policy` and **replaying it verbatim** (parse
  the `{:?}` form with `ast.literal_eval`; a `join(" ")` + `eval` mangles
  `KASTELLAN_LANDLOCK_RW=["/tmp"]` and hands you a *different*, wrong error). Worth remembering as
  the technique — nothing drains the worker's piped stderr.
- **Not a seccomp kill** — `journalctl -k | grep type=1326` was empty, which rules that out in one
  command.
- **Hits `GlinerRelexManifest::resolve` AND `browser_driver`** (`core/src/workers/browser_driver.rs:152`),
  both via the shared `resolve_interpreter_root`. macOS is unaffected (Seatbelt is not a filesystem
  view), as are container mode and system-interpreter venvs.
- **Fix direction:** bind the prefix as the venv *names* it alongside the canonical one.
  Canonicalization is right for the dep-walk prefix (`interpreter_lib_dirs` compares `starts_with`
  against canonical `ldd` output) and wrong as the sole bind. Keep it in the shared pure function;
  unit-test the diverging case **and** the accepting arm
  [[unreachable-success-path-proves-nothing]].
- **Two faults, the outer masking the inner** — the same shape as the 2026-08-02 four. The DGX venv
  being a macOS copy made the tests skip; the fixtures hardcoding `interpreter_root: None` meant
  even a working venv would not have exercised the production binds. Fixture half fixed in
  `8dc0a9f6` (two of three e2e files) and completed in the #651 review round (the third, plus a
  shared `kastellan_tests_common::venv_interpreter_binds` that refuses an unexplained `None`) —
  **that fix is what let the real fault surface, and it is necessary but not sufficient.**

### Merged work, compressed — the guard arc and the 2026-09-02 deploy

Full prose in [`archive/handover_20260902_649_pre-prune.md`](archive/handover_20260902_649_pre-prune.md)
and the snapshots before it. What still binds:

- **The DGX redeploy (2026-09-02) proved #624's thesis on the host it was filed about.** First
  post-arc boot: `measured_samples: 3`, **4 765.7** tok/s fastest against **1 450.4** slowest — a
  **3.29x spread inside one boot** — where both pre-arc single-sample boots (1 398, 1 582) sat
  essentially *at this boot's floor*, making the derived timeout **3.4x too generous** every time.
  #643 verified live: the `info!` and the durable row carry the rates identical to the last digit,
  which no test in the tree can observe. `fastest_tok_per_s` is **absent** from the installed
  binary's `strings` and that is CORRECT — the durable wire key stayed `tok_per_s` on purpose; grep
  `slowest_tok_per_s` / `measured_samples` instead, and use a **substring** grep (`grep -qx` fails
  on all of them, Rust packs literals without NULs). **Per-dispatch guard records are a `guard`
  SUB-OBJECT**, so `WHERE action LIKE 'guard%'` finds only the five boot rows and reads as "never
  screened anything"; the honest query is `WHERE payload ? 'guard'`. The `kastellan.env` clobber
  ritual is **RETIRED** on this host — post-install `diff` IDENTICAL, every tuned key lives in
  `kastellan.env.local`. **Not yet observed:** #626's retry on a genuinely stalled backend (this
  boot measured 3 of 3), which needs a cold one.
- **#641/#642/#643 (`121f22a2`) were one failure mode at three layers — a same-typed neighbour that
  can be transposed in silence.** #642: one un-`cfg`'d `validate_service_name` + `MAX_NAME_LEN` at
  the supervisor crate root, because a character-identical copy behind each platform `cfg` meant
  **neither host ever ran the other's**; the cap is deliberately not re-exported (nothing would name
  it → unused import → `-D warnings`). ⚠️ It was the third, fourth **and fifth** copy —
  [#646](https://github.com/hherb/kastellan/issues/646) records the two still hand-rolled; the
  shared predicate is *stricter*, so tightening `bring_up_pg_cluster`'s ~200 call sites without an
  audit turns passing tests into panics. #641: `DaemonSpec::new(label, data_dir, llm)`, no two
  parameters sharing a type — `suffix`/`user` were the same expression at all six call sites, so
  **deleting beat newtyping**, and no setters were added (an unused setter re-opens what the
  signature closes). ⚠️ `new` now reads the environment, and the unit's suffix no longer matches its
  sibling PG cluster's — restore with a `.suffix()` setter, not by reverting
  [[issue-as-filed-can-carry-a-regression]]. #643: one `ReportedRates` shared by the `info!`, the
  `warn!` and the durable row; **a swap silences rather than inverts** (since `slowest <= fastest`,
  a transposed row asks `fastest < slowest / 2` — the empty set on every host, forever). Also open:
  [#644](https://github.com/hherb/kastellan/issues/644).
- **A mutant found by inventory, not by diff** [[mutation-proof-counts-only-mutants-you-tried]]:
  `validate_service_name(&spec.label)` survives **both** `#[should_panic]` tests. Killed with a
  163-char label — legal alone, illegal once prefix and suffix are added — plus the **accepting**
  arm, without which a mutant that panics unconditionally passes both
  [[unreachable-success-path-proves-nothing]].
- **#632/#634 (`466ca7ff`).** The REPORTING vocabulary is **frozen at `tok_per_s`** (live rows carry
  it; the operator query is written against it), and #643 is what makes that freeze cost nothing.
  **A blind `sed` would have broken production**: `\btok_per_s\b` does not match inside
  `slowest_tok_per_s` but **does** match `"tok_per_s"`. #634's two divergences were found by reading
  the copies, not the issue: a **three**-value readiness spread, and a verbatim
  `KASTELLAN_LLM_LOCAL_URL` where an unconditional append would have dialled `/v1/v1`. **The first
  fix for that was itself a regression** — a bare `Verbatim` narrowed a variable a
  `strip_suffix`+append pair had been *normalising*. **Making a distinction representable is not the
  same as making the wrong side of it unreachable.** `extra_env` later-wins is now a *property*:
  `service_spec` collapses duplicates, because systemd documents last-wins while launchd gets a
  plist dict with a **duplicate key** the format does not define — a containment control rested on a
  belief about `CFPropertyList` [[handover-claims-verify-before-carrying]]. **A deletion mutant is
  weaker than a transposition one.**
- **#626 (`44e0f38d`)** — `PROBE_TOTAL_BUDGET_MS` equalled `PROBE_BUDGET_MS`, so any saturating
  sample ended the probe at one. The budget relation is now a compile-time assertion **beside the
  constants** — it had been `>` inside `#[cfg(test)] mod tests`, both wrong
  [[cfg-test-const-assert-is-not-a-release-guard]]. **`TimeoutBasis::Saturated` does NOT mean every
  sample stalled**; `attempted_samples: 1` is pre-#626, or a bug. `upgrade_from_git.sh`'s
  `CHANNEL_WAIT` is **120**.
- **#633 (`d3f8ed3f`)** — **the premise that kept it open was FALSE**: `from_router_config` skips the
  probe under a pinned timeout, so the configured arm needs only a `/props` mock. The gap was real;
  documenting it as *unclosable* was the defect. **Literal assertions must sit beside a structural
  equality**, and **an UNDER-POWERED mutant is indistinguishable from a blind test.**
- **#627 (`8040ca83`)** — `boot_payload` takes `tau` + `n_ctx` as **scalars, not a `&GuardTier`, and
  that IS the fix**, since `GuardTier`'s only constructor has a fatal `/props` dependency.
- **#624 (`4aee83ad`)** — the probe measured the BOOT, not the host: 6 073 / 269.6 / 1 582 tok/s on
  three consecutive boots, a **26x** under-measurement whose slowest boot fired a **false** ceiling
  finding. Keep the **FASTEST** sample (prompt processing has a ceiling and no floor, so a mean is
  wrong for a one-sided error); **each sample carries its OWN cache-buster**. The rule it left:
  **when a fix's value lives in a fold, pin the fold's *inputs*, not just its output shape.**
- **#619 (`3bd45a36`) / #615/#616/#618 (`e258ad3c`)** — `classify_transport` folded a **connect
  timeout** into `Timeout`, sending an operator to #612's pin, which cannot help (connect is capped
  at `min(timeout, 5 s)`). **The honest whole-fail-open query is `state NOT IN ('clear','block')`,
  not `error_kind IS NULL`.** `guard.error_kind` is a **closed discriminant** beside `guard.state`;
  `TimeoutBasis::Operator` carries a `PinBand` (an in-band pin keeps the historic `"operator"`
  token — use `LIKE 'operator%'`). **#616 is what unblocked #612's favoured option.**

> ⚠️ **#624 and #626 do NOT close [#612](https://github.com/hherb/kastellan/issues/612), and merging
> them is the mistake to avoid.** #624 removed the *contention* error; #612 is that extrapolating
> from a ~1 KiB sample is non-linear **on Metal whatever the load** — a quiet Mac still reads 1 137
> tok/s at 1 KiB and 260 at 64 KiB [[metal-prompt-processing-is-nonlinear]]. Both point at the same
> remedy: measure from the `ms` / `body_byte_len` the guard rows carry since #616.

> ⚠️ **#614's merge wrongly CLOSED #612 and #615** via "Filed, **not fixed**: #N". See
> [Standing hazards](#standing-hazards-that-have-each-cost-a-session).

### The guard tier itself — what still binds

- **D10 — the tier is ADVISORY defence-in-depth, NOT a gate.** 65% recall (36/55) at FP-0; 6/6 on
  bare imperatives but **5/8 missed** on narrative framing. **Nothing downstream may relax on it.**
- **τ = 0.79552656 is a REQUIRED operator input with no default**, and **five misconfigurations STOP
  THE DAEMON** (D6): half-configured keys, τ outside `(0.0, 1.0]`, a pinned timeout of 0, an
  unreachable `/props`, and a context below `SCAN_BYTE_CAP + 512 = 66 048` (D8).
  `KASTELLAN_REQUIRE_GUARD=1` makes the *unconfigured* case fatal too.
- **Measurement 3 ([#606](https://github.com/hherb/kastellan/pull/606))** — 133 cases, FP-0 on both
  hosts. `best_tau` returns **NONE**: real captured content overlaps at every threshold. Its
  security-prose stratum was **catalogue-selected**, which is why **corpus growth from production is
  now the cheap path** — harvest it before designing another campaign. `RouterConfig` lost its `Eq`
  derive (`guard_tau: Option<f32>` can hold a NaN).
- **`AuditSink::insert` is a provided method applying `truncate_payload` before delegating to
  `insert_stored`**, so no sink double can record a payload Postgres never stored
  [[audit-sink-doubles-hide-storage-transforms]]. Round one kept half the defect by dropping an
  unaffordable preserved key *silently* — **absence and loss must not render identically**.
- **The stated mitigation for an issue can disarm the instrument built to check it** — the live
  probe passed having measured nothing under a *pinned* timeout, precisely what #612 tells a Metal
  operator to use. It now refuses a pin outright.
- **The other four `screen` call sites** (`fetch_screen`, `inner_loop/summary`, `channel/ingest`,
  `recall_assembly/pg_builder`) keep catalogue-only behaviour, as does the core-initiated
  `gliner-relex` dispatch. Widening is a separate slice with its own blast radius.
- **[#585](https://github.com/hherb/kastellan/pull/585) `f90631da`** — two findings overturned the
  feasibility study and must not be re-derived from it: its `0.45–0.70` band holds exactly one
  reachable value, and `observation replay` is plan-level and cannot score a document-level tier.
  *A mock that does not return what it was sent tests only your own canned response.*
- **[#579](https://github.com/hherb/kastellan/pull/579) `bb937df7`** — D16's peer-scoped `EXISTS`
  inside the guarded UPDATE (**the nonce is a BEARER token — reading, not guessing, was the real
  threat**). Its five-agent review found eight things nine per-task reviews and 3522 tests had
  missed, all on the **argument-passing seams between layers**.
  **[#578](https://github.com/hherb/kastellan/pull/578) `af3e7e66`** — **D11** (`asks.resume_state`,
  migration 0024), because a resumed task otherwise re-executed steps it had already run.
- **[#572](https://github.com/hherb/kastellan/pull/572)/[#573](https://github.com/hherb/kastellan/pull/573)** —
  **a mutation score is only as good as the mutation set**: a reviewer's own 15 mutations left **11
  surviving** with all 113 tests green.
  **[#569](https://github.com/hherb/kastellan/pull/569)** — runtime + quantisation **PINNED**:
  llama.cpp + `Shieldstral-1.0-3B-Q8_0` on both hosts, so one fitted τ transfers.

### Standing hazards that have each cost a session

> ⚠️ **Clippy parity is a `rustup update`, not a property of the hosts — check it, don't assume it.**
> CI pins nothing (`dtolnay/rust-toolchain@stable`) and both dev hosts float on the same `stable`
> channel, so they drift out of parity silently simply by not being updated. That is what bit #573:
> clippy-clean on both hosts, then a CI failure on a lint the older toolchain did not have.
> `rustc --version` on the host you are gating on, compare against `rustup check`, `rustup update
> stable` if behind. **2026-08-31: both hosts on 1.98.0 = CI parity** (from 1.96.0), and the bump
> surfaced **zero** new lints on either — but that is a fact about this tree at this pair of
> versions, not a reason to skip the check. `rust-version = "1.78"` is the MSRV and constrains none
> of this.

> ⚠️ **A cached `cargo clippy` reports a full-workspace pass it never ran.** Exit code alone does not distinguish it — **count the `Checking` lines**. Honest from a cold `CARGO_TARGET_DIR` is ~217–303; a warm dir can report exit 0 having linted 4. Count against the *reverse-dependency set*, not against 27, or a correct incremental lint reads as a failure.

> ⚠️ **`cargo check`/`clippy --all-targets` do NOT warm the target dir for `cargo test`** — they emit metadata-only artifacts, no linked binaries. A full sweep after a lint-only leg pays a cold link (11m on the Mac vs 29s on the DGX). **Run the sweep first, lint after.**

> ⚠️ **A private `CARGO_TARGET_DIR` does not build `examples/`,** so `email_channel_e2e`'s 6 tests fail with `fixture not built` at a perfectly green commit. Fix: `cargo build -p kastellan-core --example fake_email_worker`. Same family as the daemon-e2e breakage a custom target dir causes ([[custom-cargo-target-dir-breaks-daemon-e2e]]) — read the failure text before believing a regression.

> ⚠️ **"Filed, not fixed: #N" in a PR body or commit message CLOSES #N.** GitHub matches the `fixed: #N` substring and has no notion of negation. It has cost three issues: #539 (2026-08-11, noticed), then **#612 and #615 together** (2026-08-24, unnoticed until the next session reconciled this file against `gh issue list`). Write **"deferred to #N"** or **"#N — filed, unfixed"**, and before merging run `gh pr view <n> --json body --jq .body | grep -oiE '(close[sd]?|fix(e[sd])?|resolve[sd]?)[[:space:]:,]+#[0-9]+'` over the body *and* the commit message.

> ⚠️ **Squash-merge caveat:** every PR lands as one squash commit, so its *branch-tip* SHA (where the gate ran) is **not** an ancestor of `main`. Check content, not `merge-base`.

> ⚠️ **Freshly-linked executables can hang forever in `_dyld_start` on macOS**, so every daemon e2e fails with the daemon's stdout **and** stderr **completely empty** — which reads exactly like a code defect. **Newness, not size:** the 40 MB daemon first, 13 KB `build-script-build` binaries later (wedging a cold `cargo clippy`), while anything already assessed kept running. Not the target dir and not signing — hanging, old and fresh-test binaries are all identically `adhoc,linker-signed`. **A warm `CARGO_TARGET_DIR` still works**, so `check` and `clippy --all-targets` remain available; a cold one is what wedges. Distinct from [[custom-cargo-target-dir-breaks-daemon-e2e]], which a `cargo build --workspace` fixes and this does not. [[mac-fresh-large-binaries-hang-in-dyld]]
>
> ⚠️ **The `sample` signature alone does NOT prove it — that mistake cost a wrong diagnosis in five documents (2026-09-02).** A thread merely never *scheduled* shows the same single `_dyld_start` frame, because `sample` reports where a stack is, not why it is not moving. At **load average 22.68** with another project running 16 `rustc` processes, a `kastellan-supervisor --lib` run took **13m34s wall at 4% cpu** and then **passed**. **Check `uptime` and `%cpu` first:** a wedge burns no CPU *and never finishes*; contention burns little CPU and finishes.

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

**FIRST, and the operator has already agreed to it:
[#650](https://github.com/hherb/kastellan/issues/650)** — the interpreter-alias bind, as its own PR
straight after #649 merges. It is a **production** defect in a shared pure function
(`interpreter_deps::resolve_interpreter_root`) affecting **two** workers, and it is the only thing
standing between the DGX and a green workspace: three `gliner_relex_e2e` tests fail on it today.
Full mechanism, reproduction and fix direction in
[#650 above](#650--the-fault-the-acceptance-run-exposed-open-next-sessions-first-job) — read that
before the issue, it carries the debugging technique.

**The DGX redeploy is DONE** — see
[Merged work, compressed](#merged-work-compressed--the-guard-arc-and-the-2026-09-02-deploy). The whole arc
(`4aee83ad` / `8040ca83` / `d3f8ed3f` / `44e0f38d` / `466ca7ff` / `121f22a2`) is live.

**THEN: the guard arc's remaining work is one item and it is the one that matters:**
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
| **DGX** (native aarch64, real bwrap + KVM + live PG 18) | **`5445dd68`** — branch tip of `fix/649-transformers-lock-bump` | **3937 / 3 / 55**, **176** suites, `TEST_EXIT=101`, `--no-fail-fast --nocapture`. **NOT GREEN, and the delta reconciles exactly**: 3937 + 3 = **3940**, the same total as the `f12ed26d` row below. Nothing was added or lost — three tests moved from *skipped-as-passed* to *honestly failing*. All three are `gliner_relex_e2e` (`happy_path_extract_returns_entities_and_triples`, `warm_reuse_two_calls_keep_one_worker_warm`, `invalid_input_returns_rpc_error_and_worker_stays_alive`) and all three are **[#650](https://github.com/hherb/kastellan/issues/650)**, proved pre-existing by A/B against the pre-bump lock. `entity_extraction_e2e`'s two real-model tests hit the same fault but only under `KASTELLAN_GLINER_RELEX_ENABLE=1`, which a plain workspace run does not set | exit 0 from a **cold** private dir under `$HOME`: **345** `Checking`+`Compiling` lines (107 + 238), all **27** kastellan crates named, **zero** warnings. rustc **1.98.0** | **4** (was 8), all `KASTELLAN_GLINER_RELEX_ENABLE != "1"`. **The four venv-shim skips are GONE** — that is #649's acceptance criterion, met literally |
| **Mac** (aarch64 darwin) | **#651 review round** | `uv run --frozen pytest` **63 / 0** in `workers/gliner-relex` (was 51), including the real 1.3 GB model load + one extraction under transformers 5.13.1. Seven gate arms verified individually: plain, knob=`1`, knob=`true`, missing-weights skip, missing-weights+knob→fail (both dialects), and **deselect-under-knob → exit 1** and **rename-out-of-collection-under-knob → exit 1**, which both exited **0** before. `cargo test -p kastellan-tests-common --lib gliner_weights` **10 / 0** | see the row's note | 0 |
| **Mac** (aarch64 darwin) | **`5445dd68`** | `uv run --frozen pytest` **51 / 0**, real model load under transformers 5.13.1. `cargo check -p kastellan-core --test gliner_relex_e2e --test entity_extraction_e2e` clean. Superseded by the row above | — | 0 |
| **DGX** (Python leg) | **`5445dd68`** | `KASTELLAN_GLINER_RELEX_REQUIRE_E2E=1 uv run --frozen pytest` **51 / 0** — the knob makes a missing-weights skip a failure, so the live load provably ran (transformers 5.13.1 / torch 2.13.0+cu130). ⚠️ **Predates the review round; owed a re-run at 63** | — | 0 |

**The previous green row, which the three failures above are measured against:** DGX `f12ed26d`
(tree-identical to `main` `121f22a2`) — **3940 / 0 / 55**, 176 suites, `TEST_EXIT=0`; cold clippy
exit 0 with **345** `Checking`+`Compiling` lines over 330 distinct crates, all **27** kastellan
crates, zero warnings, rustc **1.98.0**; **8** `[SKIP]`, all gliner-relex; ten mutants, ten killed.
Plus the Mac leg `kastellan-supervisor --lib` **115 / 0** and `clippy -p kastellan-supervisor
--all-targets -D warnings` exit 0 — the only host compiling the `cfg(target_os = "macos")`
`launchd_agents` half of #642.

Older rows (`466ca7ff` DGX **3928**, `553ec6ff` 3921, `6764d272` 3910, `8d92c02b` 3910, `c0255cd7`
3909, `d3f8ed3f` 3908, `12809297` 3901, `33029e32` 3900, `020b0e53` Mac 3778, `b65e44ab` 3890,
`8cb8cfb7` 3854, `09c6231f` 3840/3718, and 3047 back to 2950) are in the [`archive/`](archive/)
snapshots.

**Both hosts are load-bearing, in opposite directions — always check both.** The two supervisor backends compile on one host each: a `launchd_agents.rs` change is invisible to the DGX and a `systemd_user.rs` change is invisible to the Mac, so the two hosts legitimately report different counts. `cargo test` on the Mac compiles **zero** `systemd_user` tests, so a Mac-green run can be missing the test that pins a Linux fix entirely (it was, in #530). The mirror direction is just as real: Mac clippy compiles `cfg(target_os = "linux")` items out, so an unused cfg-linux helper fails only the DGX `-D dead-code` gate. [[cfg-linux-e2e-deadcode-dgx-clippy]]

**This is why shared, `cfg`-free modules keep winning.** #458's gate predicted 3067 and landed 3069 — investigated rather than accepted, and the +2 was exactly two `env_file` tests **running on Linux for the first time**, having lived inside the macOS-only launchd backend. Same argument as #511's `atomic_write` fold.

**Predict the count, then reconcile the delta exactly.** Every gate above was predicted from the diff's new `#[test]` count and investigated when it missed — the cheapest available detector for "a test I think I added is not being compiled". **Reconcile by diffing PER-SUITE counts, not test names:** `--nocapture` interleaves output, so a `test … ok` name grep loses lines and invents "removed" tests, and `#[should_panic]` tests print `- should panic ... ok`, which a bare `… ok` grep reports missing.

⚠️ **A `[SKIP]` can hide a dead fixture for months.** The four gliner-relex venv-shim skips were not "this host is unstaged" — the DGX's `.venv` was a **copy of the Mac's**, its `bin/python` pointing at a path that cannot exist on Linux. A venv is gitignored, so nothing in the repo could tell you. `readlink .venv/bin/python` before believing a skip, and prefer a `REQUIRE_*=1` knob that turns the skip into a failure wherever one can be added.

**Mac verification runs under a private `CARGO_TARGET_DIR`** (the IDE's rust-analyzer holds `target/debug/.cargo-lock` — [[mac-cargo-buildlock-prefer-dgx]]), and **it must live under `$HOME`, not `/tmp`**: macOS scrubbed a scratchpad target dir *mid-run* once, so a test binary vanished between build and exec (`TEST_EXIT=101`) while every `test result:` line still said `ok` — [[dgx-run-logs-tmp-scrubbed]].

**Two standing reading rules.** A green run with `[SKIP]` lines means tests *skipped*, not that the sandbox contained anything — always re-check with `-- --nocapture`. And skip-as-pass counts as passed, so counts stay comparable with or without `--nocapture`.


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

Newest first. Older entries live in the [`archive/`](archive/) snapshots and in git history; the
substance of each is compressed under [Current state](#current-state) rather than repeated here.

- **`fix/649-transformers-lock-bump`** (OPEN) — #649, the transformers advisory, plus the
  interpreter-bind fixture fix, a repeatable real-model load test, and a `uv lock --check` CI job.
  Exposed [#650](https://github.com/hherb/kastellan/issues/650).
- **`e5cb6bfc`** ([#648](https://github.com/hherb/kastellan/pull/648)) — docs-only: the DGX redeploy.
- **`121f22a2`** ([#645](https://github.com/hherb/kastellan/pull/645)) — #641 + #642 + #643 + the
  `LlmEndpoint` split.
- **`466ca7ff`** ([#640](https://github.com/hherb/kastellan/pull/640)) — #632 + #634.
- **`44e0f38d`** ([#637](https://github.com/hherb/kastellan/pull/637)) — #626, the saturating first
  sample. **`d3f8ed3f`** ([#635](https://github.com/hherb/kastellan/pull/635)) — #633, the configured
  boot-row seam. **`8040ca83`** ([#631](https://github.com/hherb/kastellan/pull/631)) — #627,
  `boot_report` as a pure module. **`4aee83ad`** ([#625](https://github.com/hherb/kastellan/pull/625))
  — #624, three probe samples, keep the fastest. **`3bd45a36`**
  ([#623](https://github.com/hherb/kastellan/pull/623)) — the connect-timeout fold. **`e258ad3c`**
  ([#619](https://github.com/hherb/kastellan/pull/619)) — `guard.error_kind` as a closed
  discriminant.
- **`8736f559`** ([#607](https://github.com/hherb/kastellan/pull/607)) — the guard tier WIRED and
  running live on the DGX. **`bb937df7`** ([#579](https://github.com/hherb/kastellan/pull/579)) /
  **`af3e7e66`** ([#578](https://github.com/hherb/kastellan/pull/578)) — #564 slices 2 and 1b.


### Earlier history

One bullet per session, newest first, in [`archive/handover_20260803_pre-prune.md`](archive/handover_20260803_pre-prune.md) § "Earlier history" — covering the Firecracker micro-VM slices 1–5c, the python-exec warm/idle arc, the Matrix worker hardening + live-channel arc, the planner-feedback arc (#337–#340), the entity/L1-embedding arc, the L3 skill arc, the egress-proxy slices #1–#4, the comms/channel-bus slices, the crates.io 0.1.0 release and the hhagent→kastellan rename. Older snapshots: [`20260727`](archive/handover_20260727_pre-prune.md), [`20260719`](archive/handover_20260719_pre-prune.md), [`20260629`](archive/handover_20260629_pre-prune.md), [`20260615`](archive/handover_20260615_pre-prune.md), [`20260611`](archive/handover_20260611_pre-prune.md), [`20260605`](archive/handover_20260605_pre-prune.md), [`20260529`](archive/handover_20260529_pre-prune.md), [`20260510`](archive/handover_20260510_pre-prune.md).

---

## Open follow-up issues (filed but not picked)

Beyond those under [Next TODO](#next-todo). Only currently-open issues; closed-issue detail lives in
the [`archive/`](archive/) snapshots and git history. **The one-line summaries here are pointers —
read the issue before acting, since several carry measurements that close off the obvious fix.**

**Sandbox (blocking a green DGX):** [#650](https://github.com/hherb/kastellan/issues/650) — see
[Next TODO](#next-todo); it is the first item.

**From the #640 review (fixed in #645 as #641/#642/#643):**
[#644](https://github.com/hherb/kastellan/issues/644) — a duplicate `ServiceSpec.env` key renders as
a duplicate launchd plist dict key, whose resolution the format does not define. `tests-common` is
safe (it collapses last-wins); this is the general case for every *other* producer.
[#646](https://github.com/hherb/kastellan/issues/646) — the two name-cap copies #642 undercounted.

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

**Re-surveyed 2026-09-02** at `fb1bfc62`; full write-up in [`docs/devel/notes/2026-09-02-openworker-resurvey.md`](../notes/2026-09-02-openworker-resurvey.md). The containment verdict is unchanged (still no OS sandbox; the `Executor` ABC is documented as *"the hedge for a future `ContainerExecutor`/`VMExecutor`"*, and `permissions.py` itself says its shell matching stops *"not a determined adversary (that needs the OS sandbox)"*), and all five earlier entries still stand. What is new is the August oversight work (OPE-111/114/117/130), which lands squarely in CASSANDRA's seam. Six findings, three of them worth filing: **(1)** their `tests/corpora/` splits the gate question from the reviewer question from the multi-action question — 120 + 121 + 60 rows — and carries **two** answer keys per row (`expected_current` vs `expected_secure`, with `known_gap`/`failure_point` when they differ) so a test *"cannot bless an identified vulnerability just because it matches today's behaviour"*; `scripts/eval_reviewer.py` turns that into a published ship gate with dated reports in `reports/`, and its `Verdict.error` flag separates a provider 5xx from a real judgment because a gate that passes on error-unsures is *"caution by outage, not judgment"*. Our guard corpus is single-key and output-side only; `cassandra::review` has no corpus at all. **(2)** the reason the *agent* gets on a denial is deliberately non-diagnostic — a specific reason *"turns the reviewer into an oracle: retry, read the reason, adjust, retry"* — which `WITHHELD_NOTE` already half-does and every CASSANDRA verdict should. **(3)** their `protected_paths()` self-protection floor read against our grants exposes one: `kastellan_runtime` holds INSERT/DELETE on `tool_allowlists` (migration 0009, deliberate — the CLI writes under it), so the daemon's own role can widen its own argv allowlist. Not exploitable today (no SQL tool reaches the model) but it is precisely the escalation that floor exists to block; a `kastellan_policy` role owning the policy tables, `SELECT`-only for runtime, is 0002's split one level in. Also noted, not filed: `PERSISTENT_AUTHORITY_TOOLS` (*"the effect lands after the conversation that authorised it has ended, so the person who bears it is not in the room"*) as a floor class no standing grant may cover; `provenance.py`'s *"the agent wrote this file 2 steps ago"* line for Phase 4; and the deliberate asymmetry that their reviewer fails **closed** while our guard tier fails **open** — both right, for opposite inputs, and worth one paragraph in the CASSANDRA docs before someone "fixes" it. One licence correction to carry forward: openworker is **MIT**, so unlike the openhuman borrowings nothing here needs clean-room reimplementation.

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
