# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260803_pre-prune.md`](archive/handover_20260803_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.

**Last updated:** 2026-08-04 · **`main` HEAD:** `fba4102c` · **Active branch:** `fix/514-supervise-channel-boot` · **Last gate:** DGX **2965 / 0 / 53**, `TEST_EXIT=0`, clippy `-D warnings` `CLIPPY_EXIT=0`, 4 `[SKIP]` (all the env-gated GLiNER tier)

**🚀 RELEASED (2026-07-19/20): v0.2.0** — [release + tag](https://github.com/hherb/kastellan/releases/tag/v0.2.0) at `b3757cac`; all 20 publishable crates on crates.io. **Operator TODO for FUTURE releases (nothing pending for this one):** add crates.io Trusted-Publishing config (owner `hherb`, repo `hherb/kastellan`, workflow `release.yml`) to the 8 crates created since 0.1.0 — leak-scan, net-classify, matrix-wire, worker-matrix, embed-broker, search-broker, python-exec, web-research — so the next tag publishes hands-free. Gotchas: [[crates-io-release-procedure]].

---

## Current state

`main` is at **`fba4102c`**; the only open kastellan PR is the unrelated dependabot [#453](https://github.com/hherb/kastellan/pull/453). Everything below the last merges is settled: the VM-entry arc is COMPLETE, v0.2.0 is RELEASED, the audit-remediation family is FULLY closed, the mail-worker test/e2e arc is done, and the extra-CA capability, its operator config and its live confirmation all shipped.

**Kastellan survives a reboot on Linux** (`4e1d30ca`, PR [#509](https://github.com/hherb/kastellan/pull/509), closes [#508](https://github.com/hherb/kastellan/issues/508)). `supervisor/src/systemd_user.rs` wrote correct `[Install] WantedBy=` stanzas and then never ran `systemctl --user enable`, so nothing was linked into `default.target` and a rebooted Linux host came up running nothing — while macOS came back, because `RunAtLoad=true` is unconditional in every plist. That parity break is what left a real Matrix message unanswered on 2026-08-02. `install` now enables `<name>.service`; `install_target` enables the **target only** (it is the boot entry point and its own `Wants=` pulls the members in — pinned by a test so a later change is deliberate); `uninstall_target` disables symmetrically; `enable` failures propagate rather than being swallowed. The `Supervisor` trait now carries **"install implies auto-start"** as an explicit cross-platform contract — which stops at *arming* the unit: getting a per-user manager up on a host nobody logs into is the host's job on **both** platforms (linger on Linux, a GUI session on macOS).

**Its retrospective review shipped as [#511](https://github.com/hherb/kastellan/pull/511) (`fba4102c`)** — no bug in #509's fix (the systemd semantics, enable-the-target-only, and disable-before-remove all check out); six findings, five fixed. The load-bearing one: **`write_atomic` staged through a *destination*-derived tmp path** (`path.with_extension("service.tmp")`), so any two concurrent writers of one unit race on a single file — and because `with_extension` *replaces* the final component, a `.target` was staged through `<name>.service.tmp`, the path a like-named `.service` would use. Now one `cfg`-free `supervisor/src/atomic_write.rs`: per-writer `.tmp.<pid>.<n>` name, `create_new(true)` so uniqueness is *enforced* rather than assumed (a pre-planted symlink is refused, not followed), cleanup on every failed-publish path. Shared by both backends **deliberately** — with no macOS runner in CI a per-backend copy is not even *compiled* on a PR, and reproducing #508's "fixed on one platform only" shape was the wrong trade; its seven tests now run on both hosts. The same destination-derived pattern was then found one layer up in `core/src/install/run.rs` (`copy_exec`, `symlink_replace` — the latter `remove_file`d the shared staging path first, deleting the *other* writer's staging link) and fixed the same way.

**The agent can answer questions about the user's mail end-to-end** — first achieved 2026-08-02, live, from a real Matrix message against the real 37 k-message archive.

Last five merges (newest first):

- **`fba4102c` — [#511](https://github.com/hherb/kastellan/pull/511), MERGED 2026-08-03.** The #509 review, two rounds — above. Filed rather than fixed: [#510](https://github.com/hherb/kastellan/issues/510) (CI never exercises #508's regression guard — see [What CI does not cover](#what-ci-does-not-cover)), [#512](https://github.com/hherb/kastellan/issues/512) (`kastellan-cli install` prints systemd-only guidance and health-checks via `systemctl` **on macOS**), [#513](https://github.com/hherb/kastellan/issues/513) (per-file atomicity is not per-*install* atomicity — two overlapping installs can publish a mix of both runs' binaries; an advisory lock is the right-sized fix). Declined deliberately: the `write`/`fsync` cleanup seam stays untested (reachable only from ENOSPC/EIO, which no hermetic test can force portably — said so in the test).
- **`4e1d30ca` — [#509](https://github.com/hherb/kastellan/pull/509), MERGED 2026-08-03, closes [#508](https://github.com/hherb/kastellan/issues/508).** Above.
- **`d37df573` — [#507](https://github.com/hherb/kastellan/pull/507), MERGED 2026-08-03.** The review round for #505. Docs/tests/prompt only, plus one log-level change. Headline: **`Plan::data_ceiling`'s `Secret` default is fail-OPEN, and #505 shipped it documented as fail-closed** — see [Fail-open `data_ceiling`](#the-fail-open-data_ceiling-correction) below. Behavioural fix is [#506](https://github.com/hherb/kastellan/issues/506).
- **`6b083553` — [#505](https://github.com/hherb/kastellan/pull/505), MERGED 2026-08-02.** Planner reliability: four independent faults, one live debugging session — see [The four faults](#the-four-faults-2026-08-02) below.
- **`e19d066a` — [#503](https://github.com/hherb/kastellan/pull/503), closes [#494](https://github.com/hherb/kastellan/issues/494).** One `Mitm` posture type (`Transparent | Intercept { upstream_extra_ca }`); the email channel's sidecar now intercepts TLS, so `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA` reaches it. Closed slice 1's known MITM gap. **Two limits survive:** the anchor map is GLOBAL and host-keyed (one entry reaches every worker on that address — the DGX shares `10.0.0.3` between localmail `:8443` and SearxNG `:8888`), and interception is the *precondition* for the #3b leak scanner, **not** coverage — nothing on this path is scanned ([#501](https://github.com/hherb/kastellan/issues/501)).

Earlier merges, one line each, in [Recently merged](#recently-merged).

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

**IN PROGRESS this session — [#514](https://github.com/hherb/kastellan/issues/514): a transient failure at startup permanently disables the Matrix channel.** `core/src/main/matrix_boot.rs` is one-shot: every failure arm logs `channel not started` and returns `None`, so a blip in the first seconds of daemon startup deafens the bot for the life of the process. Observed live on the DGX 2026-08-03 19:07 → 08-04 07:22 (**12 h deaf**, three queued messages ingested within one second of a manual restart); the same line appears on 2026-06-21 (×3), 07-02 (×2), 07-12. Trigger was a restart-window race: the *user manager itself* was shutting down (`exit.target has 'start' job queued`), so `systemd-run --scope` could not create the sidecar's cgroup, and the replacement manager then started core before the proxy path worked. Fix: a reusable `ChannelSupervisor` (retry with the existing pure `RestartBackoff`, unbounded attempts) used by **both** `matrix_boot` and `email_boot`, a `BootOutcome` taxonomy that keeps genuinely-static misconfigurations (`forced_localhost_homeserver`, a partial `EmailConfig`) fatal rather than looping, an escalating downtime log line, and `channel.started` / `channel.boot_failed` audit rows. Spec: `docs/superpowers/specs/2026-08-04-channel-boot-supervision-design.md`. **Deliberately NOT doing** issue item 3 (`After=network-online.target`): verified on the DGX that the **user** manager has no such unit (`systemctl --user list-unit-files | grep network` → nothing), so it would order against something nothing activates; and item 2's `kastellan-cli status` surface, which needs DB-backed state and is its own slice.

**Next up — operator's choice, each roughly one session:**

- **[#506](https://github.com/hherb/kastellan/issues/506) — make the `data_ceiling` default actually restrictive.** Resolve an absent ceiling to the task's `classification_floor` instead of the constant `Secret` (the *maximum* rank and therefore the loosest ceiling). Needs `Plan::data_ceiling: Option<DataClass>` (~67 literal sites, all in tests — production only ever deserializes a `Plan`), resolution once in the inner loop **before both review and L3 expansion**, and `data_ceiling_source` provenance in the `plan.formulate` audit row (today's serialized plan records `"Secret"` indistinguishably from a model decision). **`Public` is not the fix** — `classification_inference.rs` lists `"my email"` in `PERSONAL_PATTERNS`, so a `Public` ceiling would trip I1 and re-block the exact terminal plan #505 unblocked. Changes when plans get blocked ⇒ needs its own two-host gate plus a live re-run of tasks 122/124. **Also on its live gate** ([comment](https://github.com/hherb/kastellan/issues/506#issuecomment-5160758531)): confirm the planner still *emits* `data_ceiling` under #507's reworded prompt — i.e. `default_data_ceiling`'s `warn!` does not appear in `~/.local/state/kastellan/*.out`. While the constant default stands, emission rate *is* the mitigating control, and it is not unit-testable. Baseline caveat: #505's live runs used the *old* wording.
- **[#504](https://github.com/hherb/kastellan/issues/504) — the installer omits `kastellan-worker-mail` and `kastellan-worker-email-in`.** `install/plan.rs::optional_binaries()` lists neither, so **neither the mail tool nor the email channel has ever been deployable**; the #492 live run needed a hand-copy (`cp target/release/kastellan-worker-mail ~/.local/lib/kastellan/`). Small, and it unblocks any live email-channel work.
- **Mail-path planner defects (each burns a live run).** (a) The planner passes `mail.get_message`'s `message_id` as a **string** where the worker requires `i64` — observed as `"17817"`, `"{{message_id}}"` (an un-substituted template) and `"3db5c6e23812425c"` (a hex id). It costs a plan iteration in nearly every run, including the successful ones, which is why task 122 could not report flight numbers. Check whether `mail.search` advertises an id shape `mail.get_message` won't accept. **Not yet filed.** (b) [#500](https://github.com/hherb/kastellan/issues/500) — `get_message(full_headers)` sends `?full_headers=<bool>` but localmail reads `?headers=full`, so the flag has never worked.
- **Email channel — slices 2 and 3.** Slice 1 (gated inbound) MERGED, and #503 closed its MITM gap. Spec `docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`. **Slice 2** = SMTP outbound (`lettre`, MIT-verified) + full round trip; today `EmailChannel::send` refuses and every refusal is audited `channel.reply_undelivered`. **Slice 3** = DGX deploy + live tier: re-verify both negative controls (plan Task 4 Step 5 and Task 5 Step 5) on the deployed host, and **restart `localmail-serve` (+ `localmail-daemon`) on the DGX first** — see [Deployment facts](#deployment-facts-dgx).
- **[#497](https://github.com/hherb/kastellan/issues/497) — unify the per-family `ChannelBus` instances into one bus.** `main.rs` spawns a Matrix bus and an email bus, each with its own `PgCompletedTasks` LISTEN, so every bus sees every completed channel task and `handle_completed`'s `senders` miss is the normal case (the misleading `warn!("…dropping")` was demoted to `debug!` in `658924ae`). `ChannelBus::spawn` already takes a `Vec<Box<dyn Channel>>`, so this is mostly rewiring the two boot modules to return a *channel* rather than a *bus*; it also drops a redundant LISTEN connection and lets that log line be a real `warn!` again. Worth doing before a third channel family lands.
- **[#502](https://github.com/hherb/kastellan/issues/502) — `SidecarHandle` has no `Drop`**, so a failed respawn (Matrix, now also email) leaks the sidecar process; 22 were found alive on the Mac from prior sessions.
- **macOS Seatbelt-loopback verification of mail tier 1a** (carried from #490, non-blocking) — needs a Mac run with working launchd-PG; the Linux/bwrap leg is validated and tier 1b carries the macOS sandbox leg.
- **Telegram inbound** — still rejected as primary (no bot E2E, centralized, ban risk); revisit only as an additional `Channel` impl if a concrete need appears.
- **MITM-of-browser** (deferred egress slice-#2 follow-up): in-Chromium trust of the per-instance proxy CA via a proper NSS trust-store import — **not** `--ignore-certificate-errors-*`, since production must not be loosened to make a test pass. Only once a concrete inspection benefit justifies enlarging the sidecar blast radius.
- **File-split backlog (Item 9b)** — re-`wc -l` before picking, the numbers drift. Over-cap production: `sandbox/src/linux_firecracker/plan.rs` (~1160, prod only ~485 ⇒ a clean test-lift; `cfg(linux)` so DGX-gated), `workers/matrix/src/sdk_live.rs` (722, live-matrix-gated → DGX), `db/graph.rs` (926, design-gated Item 23b — deferred until a 2nd `WalkedEdge` consumer). ≤27-over deferrals (a lift saves little): `db/src/lib.rs`, `supervisor/src/launchd_agents.rs`, `core/src/scheduler/tool_dispatch.rs`, `db/src/memories/search.rs`, `entity_extraction/batch_upsert.rs`. Over-cap *test* files: `core/src/workers/gliner_relex/tests.rs` (1083), `core/src/workers/python_exec/tests.rs` (844), `core/src/scheduler/inner_loop/tests.rs` (767), `core/src/scheduler/audit/tests.rs` (713), `core/src/cassandra/types/tests.rs` (654).

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

### The four faults (2026-08-02)

Driven end-to-end from a real Matrix message ("what was my most recent qantas flight booking (in my email)?"). **Four independent faults, only one of which was a kastellan bug in the layer everyone suspected.** Order matters — each masked the next.

1. **Host: the NVIDIA driver was gone.** An `apt upgrade` installed kernel `6.17.0-1029-nvidia` but left `linux-modules-nvidia-580-open-nvidia-hwe-24.04` pinned at `-1021`, so no `nvidia.ko` existed and **Ollama silently ran the 26B model 100 % on CPU** (`/api/ps` → `"size_vram": 0`). The decoy: `linux-modules-nvidia-**fs**-…-1029` (GPUDirect Storage, *not* the driver) *did* upgrade, so the package tree looked current. **Diagnose in one line before blaming kastellan:** `ssh dgx 'lsmod | grep -c nvidia; curl -s 127.0.0.1:11434/api/ps | grep -o "size_vram[^,]*"'`. Memory: [[dgx-apt-upgrade-drops-nvidia-module]].
2. **Deployment: kastellan did not survive a reboot on Linux.** The missing `systemctl --user enable` — **fixed 2026-08-03 in PR [#509](https://github.com/hherb/kastellan/pull/509)**, see Current state. The DGX had been hand-`enable`d as a workaround, so its units already read `enabled`; a fresh install elsewhere had the bug until this landed.
3. **`llm-router`: the local reasoning model thought until the request timed out.** On the real ~16 k-token planner prompt the 26B model produced **15 094 chars of `reasoning` for 1 519 chars of plan**, 222 s wall clock; inside the daemon the same call consumed a full **600 s** budget and still timed out, so **raising `KASTELLAN_LLM_TIMEOUT_MS` is NOT a fix** (tried, failed, task 119). Fix: `ChatRequest.chat_template_kwargs` + `RouterConfig::disable_thinking` (default ON, `KASTELLAN_LLM_DISABLE_THINKING=0` opts out) emitting `{"enable_thinking": false}`, the OpenAI-compat extension both Ollama and vLLM honour, applied in `dispatch_local` as a property of the local leg; an explicit caller value wins; unset ⇒ field not serialised ⇒ byte-identical payload. Measured **222 s → 51 s**. There is no `chat_template_kwargs` PARAMETER in Ollama Modelfiles (`PARAMETER think false` is rejected — thinking lives in `RENDERER`/`PARSER`), so no zero-code operator workaround exists. Suppression *reduces* the reasoning block rather than eliminating it (15 094 → 1 933 chars), so headroom under the 180 s cap is real but not unlimited.
4. **`plan_parser` — the one that hid everything else.** `parse_plan_lenient` re-emitted the **strict**-path error whenever the lenient path failed, "so callers see a stable error type". The strict path parses the whole raw response, so its error always describes the markdown fence: `expected value at line 1 column 1` — i.e. *"your output wasn't JSON."* The model was in fact returning **complete, correct, well-formed fenced plans** that merely omitted a required field; the real error was `missing field 'steps'`. Every reader, human and agent, was pointed at the fence, at empty completions, at context overflow — none of which were happening. **Cost the entire session** until the raw output was finally logged. The lenient error is now returned whenever the input contained a `{`; the strict error survives only for input with no `{` at all, where "not JSON" is honest.

**Also fixed:** `AgentError::Decode` captured the raw completion but its `#[error("plan decode failed: {detail}")]` Display renders only `detail`, so the one piece of evidence needed reached nobody. The structural facts (`detail`, `raw_len`, `has_brace`, `finish_reason`, token counts) are now logged — that is what broke the case open. The 600-char raw head sits at `debug!`: a planner completion restates recalled memories and prior step output verbatim, so on a mail task it *is* the user's correspondence, and the daemon log (`~/.local/state/kastellan/*.out`) is a plaintext file with none of `audit_log`'s role gating — `docs/threat-model.md` gained a **"User data in the daemon log"** subsection stating the rule. **`steps` is deliberately NOT defaulted** — an empty `steps` marks a terminal plan, so defaulting it would turn a *truncated* plan into a silently-terminal one.

**Live result:** task 122 `completed` in 386 s / 6 plan iterations (*"DOCTOR HORST P HERB 19DEC DPSSYD"*, booking email 2024-08-20); task 124 `completed` in **77 s / 2 iterations** on a different question ("which airline did I fly to Bali with?"), correctly distinguishing outbound Qantas QF2103 from return QF1503.

**Caveat, and why a third run failed.** Task 123 (same Bali question) timed out at 283 s — dispatched **while `cargo test --workspace` was running on the same box** (load average 6.6). The identical question on a quiet box took 77 s. **Do not benchmark or acceptance-test the agent against a loaded DGX**; a full-workspace run and a live LLM task contend for the same CPU and the failure looks exactly like the runaway-thinking bug.

### The fail-open `data_ceiling` correction

`data_ceiling` is a **ceiling**, so the most *sensitive* `DataClass` is the most *permissive* value it can hold. #505 defaulted an absent field to `Secret` and shipped it documented as fail-**closed**; `Secret` is rank 3, the maximum. The field has exactly two enforcement points, both in `cassandra/deterministic.rs`: **I1** (`ceiling >= floor`) can never fire at rank 3 whatever the floor, and **I3** (`step.classification <= ceiling`) can never fire because no step class outranks it. A plan omitting the field is therefore **not ceiling-constrained at all** — where it was previously rejected outright.

Severity is bounded and worth stating precisely: I1/I3 only ever catch a model contradicting *its own* declarations (a competence signal, not an attack barrier — a hostile planner would declare a matching high ceiling). **The second review round narrowed the "I2 is still enforced" claim**, which was too broad: an `invoke_skill` plan's steps are *not* model-written — L3 expansion stamps each expanded step's `classification` with `plan.data_ceiling` (`memory::l3_invoke::agent` / `l3py_invoke::agent`) and runs at `inner_loop` step 1b, **before** the reviewer at step 2 — so on a plan that both omits the field and invokes a pinned skill, the expanded steps carry rank 3 and I2 passes vacuously for them too: **all three invariants, not two**. Nothing outside `deterministic.rs` reads `step.classification`, so the effect stays inside that review stage. A defect in a defence-in-depth layer, not a hole in the threat-model boundary. Behavioural fix: [#506](https://github.com/hherb/kastellan/issues/506).

### Egress / MITM traps (from #491–#503) — read before touching the proxy

1. **A `CA:TRUE` self-signed *leaf* is rejected at handshake** with rustls' `CaUsedAsEndEntity`, even though `openssl verify` accepts it — and `openssl req -x509` commonly produces exactly that shape. It fails **late and opaquely** as a `mitm_failed: …` egress decision, not at startup. The live DGX localmail cert WAS this shape; regenerated `CA:FALSE` on 2026-07-26 (backups `~/.config/localmail/tls/*.cabak-20260726-173651`) and the tier then passed. Working shapes: a non-CA leaf (`CA:FALSE`) or a real CA that signed a separate leaf. Verify with `openssl x509 -in <cert.pem> -noout -text | grep -A1 'Basic Constraints'`.
2. **The upstream anchor is trusted for EVERY host that sidecar can reach**, not just the keyed origin. #492 therefore *enforces* single-private-origin rather than trusting operator discipline: an anchor is handed out only when the worker's allowlist resolves to a single private origin written as an **IP literal** (privateness via `kastellan-net-classify::is_denied_range`, the proxy's own SSRF predicate, so the two can't drift), and **a refusal fails the spawn**. **Known limitation, documented and test-pinned, not closed:** keying is per-**host**, not per-service, so two private services sharing one address (the DGX's actual shape: localmail `:8443` + SearxNG `:8888` on `10.0.0.3`) are one origin to the rule and the second worker's sidecar also receives the first's anchor. Closing it needs `host:port` keying or a per-host rustls verifier (#492's explicit non-goal); mitigation is operational — give co-located private services distinct addresses.
3. **`tls_intercepted: true` is weaker than it reads** — emitted when the proxy takes the MITM branch, *before* the upstream handshake. **Round-tripped bytes are the load-bearing assertion.**
4. **The decision-ingest thread is deliberately DETACHED**, so reading captured rows right after `worker.close()` races its drain — worst for a connection's LAST decision. Any test asserting on a *terminal* egress decision must poll to quiescence (the shared helper does).
5. **UDS path length:** a merely descriptive scratch-dir prefix under macOS `$TMPDIR` pushes `<scratch>/egress.sock` past `sun_path` (`SUN_LEN` error) and the sidecar dies before reading the CA. Use `tests-common::short_scratch_root`.
6. **The proxy's upstream leg trusts webpki roots ONLY** unless the operator sets an anchor — so **no hermetic self-signed origin is possible for a MITM'd worker's e2e**; real-origin tiers are structural, not lazy ([[egress-proxy-upstream-trusts-webpki-only]]).
7. **Egress-decision assertions must match `host:port`, not a bare host** — a bare-host check passes on any decision mentioning it, and on the DGX loopback SearxNG and the embed endpoint share an address (cost a #448 round-trip; the convention is `is_allowed_row_for` / `is_for_origin`).

### Deployment facts (DGX)

- **#492 live confirmation DONE 2026-08-01** (audit_log, task 114): the force-routed mail tool reached the **self-signed** localmail through the MITM sidecar using the operator anchor — `egress.allowed {worker:"mail", host:"10.0.0.3", port:8443, tls_intercepted:true}` then `mail.search` returning **25 533 bytes**, which is the load-bearing evidence. Contrast in the same table: `worker:"matrix"` rows carry `tls_intercepted:false` (transparent tunnel), exactly as designed.
- **`install` REGENERATES `kastellan.env`** and silently reverts hand-added keys *and* tuned values — it reset `KASTELLAN_LLM_LOCAL_MODEL` from `gemma4:26b-a4b-it-q8_0-ctx64k` to the bare 4096-context tag, which made tasks 111–113 fail with `plan decode failed`. Re-check that value after every install. Keys that must be re-added: `KASTELLAN_MAIL_ENDPOINT`, `KASTELLAN_MAIL_TOKEN_FILE`, `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA`. Tracked by [#458](https://github.com/hherb/kastellan/issues/458).
- **Force-routing needs no re-add** — the generated `kastellan-core.service` carries `Environment=KASTELLAN_EGRESS_FORCE_ROUTING=1` from `core_service_spec` (verified live 2026-08-01). [[dgx-force-routing-deploy-facts]]
- **localmail:** bound to **`10.0.0.3:8443` ONLY** (not loopback); cert SANs `IP 10.0.0.3 / IP 127.0.0.1 / DNS spark-0d2d / DNS localhost`, verified `CA:FALSE`. api-user `kastellan-mail`, granted `horst-gmail`; bearer token `~/.config/kastellan/mail-token` (0600), password `~/.config/kastellan/mail-apipw`. **The token expires 2026-08-30** — re-mint via `POST /v1/auth/login`. The running `localmail-serve.service` started **2026-07-27**, three days *before* the server-side-cursor merge ([hherb/localmail#223](https://github.com/hherb/localmail/pull/223), `0b6c5e05`) it depends on — **restart it before any live email-channel deployment.**
- `scripts/upgrade_from_git.sh` does the whole build+install+restart+verify and is hardcoded to `main`. Daemon logs live in `~/.local/state/kastellan/*.out`, **not** the journal.
- The live DGX bot is **eval-only** (just the two of us, no external users), so transient downtime is fine and restarts need no confirmation — but containment controls still get re-added after an install. [[dgx-eval-only-experiment-freely]]

### Process lessons that have each cost a re-run

- **Write long-run logs to `$HOME`, never `/tmp` — on BOTH hosts.** `/tmp` is scrubbed mid-run on the DGX *and* on the Mac; macOS deleted a completed `cargo test --workspace` log plus the harness task-output files, forcing a second 45-minute gate. Include an explicit exit-code line and a DONE sentinel. [[dgx-run-logs-tmp-scrubbed]]
- **Each host is structurally blind to the other's `cfg` arms.** Mac clippy compiles `#[cfg(target_os="linux")]` items out, so an unused cfg-linux helper passes on the Mac and fails the DGX `-D dead-code` gate; the mirror direction is real too (the DGX is blind to DE-gated items in dual-platform files). Gate both hosts after scripted edits. [[cfg-linux-e2e-deadcode-dgx-clippy]]
- **Don't race sidecar tests against a build.** The 5 s sidecar-readiness budget is load-sensitive: `email_mitm_e2e` took 28.6 s loaded vs 8.2 s quiet, and `egress_force_routing_e2e::forced_coupling_…` fails under full-workspace load but passes 3/3 standalone in ~0.11 s. **Don't "fix" it by inflating a production timeout.** Adjacent: [#502](https://github.com/hherb/kastellan/issues/502) (leaked sidecar children), [#328](https://github.com/hherb/kastellan/issues/328).
- **Mac CLI cargo blocks on the IDE's rust-analyzer** holding `target/debug/.cargo-lock` — use a scratch `CARGO_TARGET_DIR` (e.g. `$HOME/.cache/kastellan-sdd-target`) or iterate on the DGX. Never pipe background cargo through `| tail` (masks the exit code, buffers output). [[mac-cargo-buildlock-prefer-dgx]]
- **Verify deployment claims before carrying them forward** — issue comments and handovers go stale; check the live host before repeating a claim in a spec, PR or ROADMAP. [[handover-claims-verify-before-carrying]]

---

## Working state

### Test baseline (authoritative)

| Host | Commit | Result | clippy `-D warnings` | `[SKIP]` |
| --- | --- | --- | --- | --- |
| DGX (native aarch64, real bwrap + KVM + live PG 18) | `f57db609` (#511, now ≡ `main` `fba4102c`) | **2965 / 0 / 53** across 162 binaries, `TEST_EXIT=0` | `CLIPPY_EXIT=0` (whole workspace) | exactly 4, all `KASTELLAN_GLINER_RELEX_ENABLE` |
| DGX | `c1942f23` (first review round) | **2956 / 0 / 53** | exit 0 | exactly 4, same tier |
| DGX | `4e1d30ca` ≡ `main` (#509) | **2952 / 0 / 53** across 162 binaries | exit 0 | exactly 4, same tier |
| DGX | `c5ba7637` ≡ pre-#508 `main` | **2950 / 0 / 53** (the baseline #508 was +2 over) | exit 0 | exactly 4, same tier |
| Mac (Apple Silicon, Seatbelt, PG-gated suites separately) | `#507` fix commit | **2830 / 1 / 23** | exit 0 | not verified (run omitted `--nocapture`) |

**Both hosts are load-bearing for this arc, in opposite directions — check both.** #508's two tests are `cfg(target_os = "linux")`, so the DGX is authoritative there and a Mac full-workspace run shows no count change at all. The first review round inverted that for half its diff (a macOS-only staging test set, **invisible to the DGX** — [[cfg-linux-e2e-deadcode-dgx-clippy]], mirror direction). The second round largely *dissolves* the problem: consolidating the two backend copies into one `cfg`-free `atomic_write` means its seven tests are counted and run on **both** hosts, and only the two backend wiring tests remain platform-split.

**+13 = exactly the tests added**, which is the useful cross-check: 7 shared `atomic_write` + 2 systemd wiring + 4 `install::run` staging (three portable, one `cfg(unix)`). Nothing platform-gated slipped through.

Mac verification (the IDE's rust-analyzer holds `target/debug/.cargo-lock`, so these run under a private `CARGO_TARGET_DIR` rather than full-workspace — [[mac-cargo-buildlock-prefer-dgx]]): `cargo test -p kastellan-supervisor` **85 / 0**, no `[SKIP]`, including the real `launchctl` round-trip; `cargo test -p kastellan-core --lib install::` **25 / 0** (+4); `clippy -p kastellan-supervisor --all-targets -D warnings` exit 0 **and the same crate cross-linted for `aarch64-unknown-linux-gnu`** exit 0 (pure-Rust crate, so this type-checks the cfg-linux half — including the systemd test bodies — from the Mac, [[cross-clippy-pure-rust-crates]]); `clippy -p kastellan-core --all-targets -D warnings` exit 0.

**One gate caveat, stated exactly:** the 2965 full-workspace run is at `f57db609`. The tip (`384c446b`) changes only two test bodies inside `kastellan-supervisor` (assert style; same test count), so rather than spend another full run on it the delta was gated directly on the DGX at the tip — `cargo test -p kastellan-supervisor -- --nocapture` **86 / 0, no `[SKIP]`** (the smoke suites really ran, so #508's regression guard executed rather than skipping) and `clippy -p kastellan-supervisor --all-targets -D warnings` exit 0.

The single Mac failure in the `#507` row is `egress_force_routing_e2e::forced_coupling_enforces_allowlist_and_ingests_decisions` — the load-sensitive sidecar-budget flake described above; it **passed on the DGX**, alongside its PG sibling, confirming host/load specificity rather than a code defect.

**Two standing reading rules.** A green run with `[SKIP]` lines means tests *skipped*, not that the sandbox contained anything — always re-check with `-- --nocapture` (CLAUDE.md's "when tests pass but feel suspicious"). And skip-as-pass counts as passed, so counts stay comparable with or without `--nocapture`.

#### What CI does not cover

`linux-check.yml` is **compile-only** — `cargo check --workspace --all-targets`, `clippy -D warnings`, and `cargo test -p kastellan-tests-common` (hermetic structural guards only). The workflow header argues that scope deliberately, and it is the right default. But it means a green PR check says *nothing* about behaviour, and two consequences are worth holding in mind rather than rediscovering:

- **`cargo test -p kastellan-supervisor` never runs on a PR.** So #508's regression guard — the tests pinning that `install`/`install_target` actually call `systemctl --user enable` — is enforced only by an operator running the DGX suite. Both tests are `cfg(target_os = "linux")` *and* `[SKIP]` without a live user manager, which `ubuntu-latest` does not have, so simply adding the job would not fix it. [#510](https://github.com/hherb/kastellan/issues/510) tracks the options (a `REQUIRE_USER_MANAGER=1` knob first, then a runner that can satisfy it).
- **There is no macOS job at all** — `.github/workflows/` is `linux-check.yml`, `docs.yml`, `release.yml`. So for `cfg(target_os = "macos")` code the gap is not "never run", it is **never compiled**: the launchd backend, its tests, and every macOS arm of a dual-platform file are invisible to CI, and the Mac is the only host that sees them (the mirror of [[cfg-linux-e2e-deadcode-dgx-clippy]]). That is the concrete reason #511 folded the two backends' staging helpers into one `cfg`-free module: shared code gets compiled and tested on a PR, per-backend code does not.
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

- **`core`** (`kastellan-core`) — lib + 2 bins (`kastellan` daemon, `kastellan-cli`). Owns: the `tool_host` dispatch chokepoint (spawn-under-sandbox, lockdown-env derivation, wall-clock watchdog, secret-ref substitution in, output secret-scrub + injection screen out, three audit-emission arms), the scheduler + inner loop, CASSANDRA review stages, three-lane memory + recall + the L0/L1/L3 arcs, `worker_lifecycle` (SingleUse / IdleTimeout / `PersistentWorker`) and `force_route`, the `egress/` host side (sidecar spawn, policy rewrite, decision→audit, leak-scanner provisioning, upstream extra-CA selection), `channel/` (Matrix + gated email inbound, pairing, bus), the secrets vault, the audit mirror, `workers/*` host-side manifests, `registry_build`, the handoff cache, and the installer. **Startup is fail-closed:** `db::probe::run` → `connect_runtime_pool` → `spawn_mirror` before `wait_for_shutdown`.
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

- **2026-07-31 `bf8e850b` — [#496](https://github.com/hherb/kastellan/pull/496) Phase-2 email fallback channel, slice 1 (gated inbound).** All 10 plan tasks: sandboxed `email-in` worker, pure DMARC+token gate (`channel/email/gate.rs` + `authres_parse.rs`), evidence enforcement at the ChannelBus authorization chokepoint, core-side `EmailChannel`, hermetic e2e, config-gated daemon wiring. Nine post-PR review findings fixed pre-merge — two High: a live DMARC-gate bypass via a legal RFC 8601 `method-version` (`dmarc/1=fail` parsed as a different method, so the more-than-one-dmarc rule never fired and a forged unversioned `dmarc=pass` decided), and 401/403 treated as permanent, which **destroyed mail**. Outbound send deliberately not in slice 1.
- **2026-07-27 `0be03b30` — [#495](https://github.com/hherb/kastellan/pull/495) / [#492](https://github.com/hherb/kastellan/issues/492): upstream extra-CA operator config.** `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA` parsed + every PEM read at daemon startup (fail-closed), per-worker selection at spawn, single-private-origin **enforced** rather than documented. See trap 2 above for the surviving per-host-keying limitation.
- **2026-07-27 `c0ac4e62` — [#493](https://github.com/hherb/kastellan/pull/493) / [#491](https://github.com/hherb/kastellan/issues/491): force-routed mail round-trip e2e + the egress-proxy upstream extra-CA seam.** Also fixed a real ordering bug: the proxy validated its upstream TLS config *after* publishing the readiness signal, so a fail-closed abort returned a healthy handle for a dying proxy.
- **2026-07-24 `efc1001b` — [#490](https://github.com/hherb/kastellan/pull/490): mail-worker live-test coverage** (sandbox, egress, daemon/planner legs). Tests + test-infra only; three `tests-common` de-dup lifts.
- **2026-07-23 `e1d37633` — [#486](https://github.com/hherb/kastellan/pull/486), closing [#388](https://github.com/hherb/kastellan/issues/388) + [#389](https://github.com/hherb/kastellan/issues/389):** install-dir trust probe, manifest-under-lock WARN (derive-then-warn, not reject), keyring first-init read-back-verify, merged overlapping secret-scrub spans. **The audit-remediation family is FULLY closed.**
- **2026-07-22/23 `ce144513` + `87afd8b2` — [#483](https://github.com/hherb/kastellan/pull/483) / [#487](https://github.com/hherb/kastellan/pull/487): `kastellan-worker-mail`, the 26th crate.** #487 was the first LIVE run against the real archive. [[mail-worker-localmail-verification]]
- **2026-07-22 `06700212` — [#482](https://github.com/hherb/kastellan/pull/482) / [#387](https://github.com/hherb/kastellan/issues/387):** the bwrap + Firecracker path binds resolved `..`/absolute lexically but not symlinks; bwrap now binds canonical-src → original-dest (TOCTOU-safe). Host-source paths only — guest-side paths stay lexical.
- **2026-07-20/21 `dd10bd68` → `c1fdb07c` → `4c03929f` — the provisioning-integrity family** ([#478](https://github.com/hherb/kastellan/pull/478) / [#480](https://github.com/hherb/kastellan/pull/480) / [#481](https://github.com/hherb/kastellan/pull/481)): guest kernel sha256-pinned in one shared `lib/guest-kernel.sh`, verified **at every VM boot**, image dir `root:<worker-group>` 1775 and `vmlinux` `root:root 0644`; same treatment for the Firecracker + Continuwuity downloads (verify-**before**-use asserted structurally). Sums are TOFU — documented honestly.
- **2026-07-19/20 `61890c48` / `1f353dd8` / `02ef016c` — the VM-entry arc COMPLETE** ([#474](https://github.com/hherb/kastellan/pull/474) / [#476](https://github.com/hherb/kastellan/pull/476) / [#477](https://github.com/hherb/kastellan/pull/477)): first real page rendered inside a micro-VM through a real sidecar, both slice-2 budgets **measured** (`example.org` 1.61 s = 1.8 % of `wall_clock_ms`; Wikipedia 628 MiB = 30.7 % of `mem_mb`); 15-way duplicated `[SKIP]` helpers lifted into `tests-common::microvm` (−866 lines). **Correction that keeps being needed:** this arc does NOT fix macOS [#286](https://github.com/hherb/kastellan/issues/286) — Firecracker is Linux-only; #286's named fix is the `MacosContainer` VM-netns backend ([#55](https://github.com/hherb/kastellan/issues/55)).

### Earlier history

One bullet per session, newest first, in [`archive/handover_20260803_pre-prune.md`](archive/handover_20260803_pre-prune.md) § "Earlier history" — covering the Firecracker micro-VM slices 1–5c, the python-exec warm/idle arc, the Matrix worker hardening + live-channel arc, the planner-feedback arc (#337–#340), the entity/L1-embedding arc, the L3 skill arc, the egress-proxy slices #1–#4, the comms/channel-bus slices, the crates.io 0.1.0 release and the hhagent→kastellan rename. Older snapshots: [`20260727`](archive/handover_20260727_pre-prune.md), [`20260719`](archive/handover_20260719_pre-prune.md), [`20260629`](archive/handover_20260629_pre-prune.md), [`20260615`](archive/handover_20260615_pre-prune.md), [`20260611`](archive/handover_20260611_pre-prune.md), [`20260605`](archive/handover_20260605_pre-prune.md), [`20260529`](archive/handover_20260529_pre-prune.md), [`20260510`](archive/handover_20260510_pre-prune.md).

---

## Open follow-up issues (filed but not picked)

Beyond those already listed under [Next TODO](#next-todo). Only currently-open issues; closed-issue detail lives in the archive snapshots and git history.

- [#501](https://github.com/hherb/kastellan/issues/501) — no long-lived channel sidecar (Matrix or email) ever gets leak-scanner fingerprints, **and the proxy fails open**, so it looks scanned.
- [#500](https://github.com/hherb/kastellan/issues/500) — `mail.get_message(full_headers)` sends `?full_headers=`, localmail reads `?headers=full`.
- [#485](https://github.com/hherb/kastellan/issues/485) / [#484](https://github.com/hherb/kastellan/issues/484) — enforce `SingleUse` for `wants_workspace_out` tools in release builds (not just a `debug_assert`); lazy per-task out dir instead of eager mkdir+rmdir.
- [#458](https://github.com/hherb/kastellan/issues/458) — installer: preserve hand-appended operator env across reinstalls (`kastellan.env.local`) + warn loudly when force-routing is off.
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
