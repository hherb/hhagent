# ROADMAP archive — Phase 0 (foundations)

Lifted out of [`../ROADMAP.md`](../ROADMAP.md) on 2026-08-14 to keep the live
roadmap focused on open work. **Every item here is complete**; nothing was
dropped, only relocated. Commit hashes are preserved so the build sequence
stays reconstructible.

---

## Phase 0 — Skeleton & First Sandboxed Worker (Linux)

- [x] Cargo workspace, AGPL-3.0 license, README, .gitignore — `140eec5`
- [x] `kastellan-core` (bin+lib stub) — `140eec5`
- [x] `kastellan-sandbox` crate skeleton (trait + policy struct) — `140eec5`
- [x] `kastellan-supervisor` crate skeleton — `140eec5`
- [x] Architecture & threat-model doc skeletons — `140eec5`
- [x] Linux bwrap backend (`linux_bwrap.rs`): unshare-all, FS bind, --clearenv, --setenv, die-with-parent, new-session, as-pid-1 — `eae3df4`, `f2411ec`
- [x] AppArmor `unprivileged_userns` workaround: `scripts/linux/install-bwrap-apparmor-profile.sh` + `LinuxBwrap::probe()` — `eae3df4`
- [x] Sandbox negative tests (/etc/passwd + /home invisible, listed paths visible, net unreachable, relative paths rejected) — `eae3df4`
- [x] `kastellan-protocol` crate: JSON-RPC 2.0 server/client over stdio (MCP-stdio compatible) — `f2411ec`
- [x] `workers/shell-exec`: argv allowlist, no shell interpretation (`KASTELLAN_SHELL_ALLOWLIST`) — `f2411ec`
- [x] `core::tool_host::spawn_worker`: spawn worker under sandbox, return connected protocol Client — `f2411ec`
- [x] End-to-end test: core → bwrap → shell-exec → JSON-RPC echo + POLICY_DENIED + METHOD_NOT_FOUND — `f2411ec`

## Phase 0 hardening — Defence in depth (Linux)

- [x] Landlock LSM as second FS-allowlist layer in the worker (ABI v6) — `3210f70`, `97d4465`
- [x] seccomp-bpf syscall filter — per-profile allow-list (`Strict` kills `socket()`, `NetClient` permits) — `3210f70`, `97d4465`
- [x] Worker prelude crate (`workers/prelude`): `serve_stdio` calls `lock_down()` before serving — `3210f70`
- [x] `tool_host` derives lockdown env (`KASTELLAN_LANDLOCK_RW` / `KASTELLAN_SECCOMP_PROFILE`) so callers can't skip worker-side layers — `3210f70`
- [x] cgroup v2 CPU/memory caps via `systemd-run --user --scope` (MemoryMax + MemorySwapMax=0 + CPUQuota + TasksMax); probe fails closed without a live `systemd --user` — `3cea642`
- [x] Policy-driven `cpu_quota_pct` / `tasks_max` + `setrlimit(RLIMIT_CPU)` `cpu_ms` enforcement (cross-platform `prelude/rlimit.rs`) — closes #6, 2026-05-14
- [x] Per-task `Workspace` RAII type (`<root>/<task_id>/{in,out,tmp}`, single owner, `extend_policy` wiring) — `9333311`
- [x] Spawn timeout / wall-clock kill (`WorkerSpec.wall_clock_ms`, watchdog thread, `kill(-1)`-fanout guard) — `57edfb2`

## Phase 0b — macOS Port (Seatbelt)

> Done before adding more workers, to stop Linux-isms leaking through the codebase.

- [x] `macos_seatbelt.rs`: SandboxPolicy → `.sb` (TinyScheme) generator; strict profile denies unrestricted mach-lookup (#1) — `2fa46a2`
- [x] `sandbox-exec` invocation + `setsid` fresh-session isolation (#2) — `2fa46a2`
- [x] setrlimit CPU via shared `prelude::rlimit` (mem/wallclock deferred to container backend / parent watchdog) — 2026-05-14
- [x] Network containment via `(deny network*)` + allowlist — `2fa46a2`
- [x] All sandbox containment + e2e tests mirrored green on macOS — `2fa46a2`

## Phase 0 cont. — Postgres bring-up

- [x] Local Postgres via PGDG apt + user-level supervisor unit (`scripts/linux/install-postgres.sh`, PG 18; macOS via Homebrew) — 2026-05-09
- [x] Localhost-only UDS, peer auth, dedicated `kastellan` role, locked-down `initdb` (`kastellan-db-init`, idempotent) — 2026-05-09
- [x] `pgvector` extension; full-text search via native `tsvector`+GIN; graph storage via relational `entities`/`relations` behind a `Graph` trait — 2026-05-09 (closes #9/#10 won't-fix)
- [x] `db/migrations/` skeleton (`memories`/`tasks`/`entities`/`relations`/`audit_log`/`secrets`); `vector(1024)` (bge-m3 dim) — 2026-05-09
- [x] `sqlx` embedded `MIGRATOR` run at core startup, fail-closed — 2026-05-09
- [x] Secrets at rest: AES-256-GCM + OS keyring (`db::secrets`, AAD-bound, `Zeroizing`); migration 0004 — closes #12, 2026-05-10

## Phase 0 cont. — Audit log

- [x] Non-superuser `kastellan_runtime` role + DB-layer REVOKE on `audit_log` (append-only enforced by Postgres); migration 0002 — 2026-05-10
- [x] Append-only audit writer at the `tool_host::dispatch` chokepoint; migration 0003 NOTIFY trigger; runtime-pool `SET ROLE` on every connection — closes #11, 2026-05-10
- [x] JSONL on-disk mirror under `~/.local/state/kastellan/` (`audit_mirror::spawn_mirror`, daily rotation, fsync per write) — 2026-05-10
- [x] CLI viewer: `kastellan-cli audit tail` (no DB connection required) — 2026-05-10

## Phase 0 cont. — LLM router stub

- [x] OpenAI-compatible HTTP client (`kastellan-llm-router`, `Router::send`, reqwest + rustls) — Option J, 2026-05-10
- [x] Local backend pointer (vLLM/SGLang :8000 Linux, Ollama :11434 macOS; `KASTELLAN_LLM_*` env) — 2026-05-10
- [x] Frontier backend pointer — unwired (`PolicyDeniedFrontier`) until the Phase-5 policy gate; key sourced from `db::secrets`, never env — 2026-05-10
