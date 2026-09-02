# Kastellan security audit — 2026-09-02

> Pre-release defensive audit of the whole workspace (27 Rust crates, the two
> Python workers, the privileged shell scripts, the Python and Rust dependency
> trees), conducted as seven parallel boundary audits plus a lead review of the
> chokepoints, two months after the [2026-07-02 audit](security-audit-2026-07-02.md).
> Every finding below was re-verified against source by the lead before it was
> fixed; **every fix landed on the audit branch with a regression test where one
> could be written hermetically**, and the DGX-only items are called out.

## Executive summary

The containment core holds: no finding lets a worker escape its OS sandbox,
the secrets vault, Postgres role isolation, the pairing protocol, the
JSON-RPC framing caps and the SSRF predicate all survived adversarial review
again. The real defects clustered in four places, and all four are closed on
this branch:

1. **Secrets reaching the planner and the audit log** through worker *error*
   text and non-python-exec output (H1).
2. **Persistent prompt injection** via agent-raised L1 insights, rendered
   unscreened into every future system prompt (H2).
3. **A gap between the two containment layers**: `clone(CLONE_NEWUSER)` /
   `clone3` were unconditional seccomp allows while `unshare` was killed, and
   Landlock — per-thread by kernel design — was applied *after* the networked
   workers had already built their HTTP runtime threads (H4, F4).
4. **Predictable, non-exclusive per-spawn directories under `/tmp`** for the
   egress sidecars, brokers, the Matrix channel and the Firecracker run dirs,
   plus world-readable secret-bearing files inside them — exploitable by
   another local uid on a shared host (H3, S1–S4, F5).

Supply chain: one Rust advisory (`h2` RUSTSEC-2026-0258) fixed; the
browser-driver's Python dependencies, previously installed unpinned from `>=`
floors, are now hash-pinned on both install paths; the license sweep of every
compiled crate found nothing outside the AGPL-compatible set; `pip-audit` is
clean for both Python workers.

## Findings and dispositions

Severity is against the threat-model invariant (a compromise reaches at most
the agent's own uid, its Postgres role, its scratch FS and the one tool's
allowlisted endpoints). "Fixed" means code + test on this branch.

| # | Sev | Boundary | Finding | Disposition |
|---|-----|----------|---------|-------------|
| H1 | High | tool_host | Worker `RpcError` messages (and every non-python-exec result) echoed redeemed `secret://` plaintext into the planner prompt, `audit_log.payload.err/result` and the JSONL mirror; shell-exec's `argv[0] "…" not in allowlist` denial was the direct instance | **Fixed**: scrub runs for every tool on both the `Ok` value and the error (`tool_host::post_process`, `secret_scrub::scrub_client_error`); shell-exec no longer echoes argv[0] |
| H2 | High | prompt/memory | Agent-raised L1 insights persisted with no injection screen and rendered into every later task's `<l1_insights>` block (injection that entered via web-fetch drove unrelated tasks) | **Fixed**: strict catalogue screen at promotion (`L1Error::InjectionBlocked`, audited as `l1.injection_blocked`) and again at prompt assembly; planner prompt now states the data-not-instructions rule |
| H3 | High (multi-user host) | egress/broker/matrix/microvm | `create_dir_all` under `/tmp` with `<prefix><pid>-<seq>` names adopted a pre-planted attacker-owned dir: sidecar UDS + MITM CA substitutable, `secret_hashes.json` 0644, `fc.json`/`launcher.pid` written through symlinks, VM images 0644 | **Fixed**: `kastellan_sandbox::private_dir` (exclusive `mkdir` 0700 + owner/mode verification, `O_EXCL` 0600 files) at every site; sidecar spawn refuses a pre-existing socket/CA; images pre-created 0600; persistent image hardened; orphan sweep reads only own real dirs with `O_NOFOLLOW\|O_NONBLOCK` |
| H4 | High | prelude/seccomp | `clone`/`clone3` unconditional allows → nested user namespace (the userns-LPE class) despite the `unshare` kill | **Fixed**: `clone` admitted only with `flags & NAMESPACE_CLONE_FLAGS == 0`; `clone3` → `ENOSYS` overlay (libc falls back to `clone`); real-kernel fork-based tests; bwrap and the VMM jail now also pass `--disable-userns` |
| F4 | Med-High | prelude/Landlock | Landlock is per-thread; net workers built their tokio/reqwest runtime threads in `Handler::from_env()` *before* `serve_stdio` locked down, so the network-facing threads had no Landlock | **Fixed**: `serve_stdio_with(build)` constructs the handler after lockdown (web-fetch/search/research, mail, email-in); brokers build their transport after `lock_down`; the Matrix worker restricts each runtime thread in `on_thread_start` |
| F2 | Med | prelude | Missing `KASTELLAN_SECCOMP_PROFILE` silently meant *no* seccomp; a Landlock `KernelTooOld` served one layer short; the guest init decoded a corrupt env token to an empty env (no lockdown) | **Fixed**: missing var is an error (`none` stays the explicit opt-out); Landlock create/enforce failure is an error; a corrupt guest env token refuses the boot |
| F7 | Low | prelude | Seccomp kills core-dumped worker memory (proxy CA key, Matrix E2E keys, mail token) | **Fixed**: `RLIMIT_CORE=0` + `PR_SET_DUMPABLE=0` in `lock_down` |
| E-F1 | Med | egress | SPKI pin overlay matched any *presented* certificate, so an appended genuine pinned cert satisfied the pin on a mis-issued chain | **Fixed**: pins matched against webpki's validated path via `verify_for_usage`'s `verify_path` callback (leaf, on-path intermediates, anchor); rcgen-built regression tests |
| E-F2 | Med | egress | Decision-report ingest used unbounded `read_line` (core OOM by a compromised sidecar); sidecar chose its own `worker` attribution | **Fixed**: 64 KiB record cap via `read_capped_record`; worker name asserted by the core |
| E-F3 | Med-Low | egress | Pins selected by exact host string, so a suffix allowlist entry (`.example.com`) left the sidecar with no pins at all | **Fixed**: selection uses the proxy's own matcher semantics; test |
| C-F1 | Med | matrix | Worker auto-joined any invite from anyone and forwarded every room; the core authorised the peer but never the room, so a third party could host the paired operator in a room they control | **Fixed**: invites from outside `KASTELLAN_MATRIX_PEERS` are declined; only two-party rooms (bot + one peer, invites counted) are forwarded; per-sender buffer fairness (`push_bounded_fair`) closes the drop-oldest flood; peer set handed to the worker by the daemon |
| C-F4 | Med | installer | Root installer downloaded the guest kernel to a predictable `.partial.$$` name in the agent-writable 1775 dir and `curl -o` followed a planted symlink | **Fixed**: download + verify in a root-private `mktemp -d`, then `install -m 0644` |
| C-F5 | Low-Med | matrix | VM-mode bot password written under a predictable, pre-creatable `/tmp/kastellan-matrix-<pid>` with a recursive mkdir and a symlink-following write | **Fixed**: `ensure_private_dir` (create-or-verify owner+0700) + `O_NOFOLLOW` write |
| W-2 | Low-Med | microvm-init | Guest init exec'd the worker as guest root with all caps | **Fixed**: host passes the daemon euid; init chowns the writable mounts, `setgroups/setgid/setuid`, `PR_SET_NO_NEW_PRIVS`, RW drives `nosuid,nodev`; an older host keeps today's behaviour loudly. **Needs the DGX Firecracker gate.** |
| T-F5 | Med (opt-in warm VM) | python-exec | A forked `setsid` grandchild survived its call; the next dispatch could read a prior call's secret from `/proc/*/cmdline` | **Fixed**: interpreter runs as its own process group, group-killed after `wait()` |
| S6 | Low | core/workers | `resolve_interpreter_root` could derive `$HOME`/`~/.local` and bind the daemon's config/state into a jail | **Fixed**: `guard_interpreter_root` refuses prefixes containing the daemon's state |
| R-F1 | Low | llm-router | Response bodies read unbounded (core OOM from a hostile model server) | **Fixed**: 64 MiB cap, `RouterError::BodyTooLarge` |
| R-F2 | Low | llm-router | Ambient `HTTP(S)_PROXY` silently re-routed every prompt | **Fixed**: `.no_proxy()` |
| R-F6 | Low | core | Audit JSONL mirror and observation captures were umask-default (world-readable under 022) | **Fixed**: 0700 dirs, 0600 files |
| R-F5 | Low-Med | scheduler | No per-plan step cap | **Fixed**: `MAX_STEPS_PER_PLAN = 64`, plan refused whole |
| R-F8 | Low | prompt | Approved `<skills>` entries rendered unescaped | **Fixed**: fields escaped like L1/recalled bodies; test |
| T-F4 | Low | tool_host | macOS watchdog killed only the direct child; the worker's children survived | **Fixed**: process-group kill on macOS |
| W-4 | Low | web-fetch | Worker-side allowlist ignored the port (legacy `--share-net` mode) | **Fixed**: 443-only on the initial URL and every redirect hop; test |
| W-5 | Low | browser-driver | `page.route` was the only egress control in direct-net mode; service workers bypass it | **Fixed**: context created with `service_workers="block"`; test |
| DB-F2 | Low | db | `pairing_codes` UPDATE grant was table-wide (a compromised runtime role could revive a consumed code) | **Fixed**: migration 0025 narrows it to `(consumed_at, consumed_by)` |
| CLI-F3 | Low | cli | `pair revoke` did not normalise the peer like `pair issue-token` | **Fixed** |
| N-1 | Low | net-classify | `0.0.0.0/8` beyond the all-zeros address and `fec0::/10` were not denied | **Fixed**; tests |
| SC-1 | Med | supply chain | browser-driver deps installed unpinned from `>=` floors on the host path; VM image hand-pinned only the three direct deps | **Fixed**: `workers/browser-driver/requirements.lock` (version + sha256 for every distribution); both paths `--require-hashes --only-binary=:all:` |
| SC-2 | Med | supply chain | `h2 0.4.15` — RUSTSEC-2026-0258 (unbounded empty DATA frames) | **Fixed**: 0.4.16 (`spin` un-yanked to 0.9.9 alongside) |

### Deferred with a reason (each is filed for follow-up, none blocks release)

- **Brokers (`embed-broker`, `search-broker`) are not force-routed** — their
  `Net::Allowlist` is enforced by no OS layer (`--share-net`); a broker
  compromised via a malicious SearxNG/embedding response reaches the whole host
  network. Needs its own egress sidecar plumbing (a design slice), and the
  threat model should state the trust assumption until then.
- **Guard-model tier does not see bytes past `SCAN_BYTE_CAP` / `fetch_handoff`
  slices** — those are catalogue-screened only. The tier is advisory by design
  (D10); widening it per slice is a latency/design decision.
- **`secret://` refs are not bound to a tool** — a planner can route any
  materialised ref to any tool. Compensated by the egress leak scanner and the
  now-universal output scrub; a per-tool allowed set at `materialize` is the
  right fix and a design change.
- **Egress `Host:` header ≠ CONNECT authority (domain fronting)** and the
  sidecar's own host-netns reach (`Net::ProxyEgress`) — both Low; the first
  needs HTTP head parsing in the MITM relay, the second an `IPAddressDeny`
  that would break the documented loopback-SearxNG setup.
- **`net_client` grants `bind`/`listen`/`accept`** to pure clients — splitting
  a `net_server` profile touches core policy, the browser shim and the
  brokers; Low while force-routing puts clients in a private netns.
- **Email replay** — a captured, DKIM-valid, token-bearing message from the
  paired address is accepted indefinitely (no Date/Message-ID freshness).
  Needs the worker to forward those headers; email is off by default.
- **gliner-relex weights** are fetched by `hf download` without a pinned
  revision or hash; the Hugging Face API was unreachable from the audit
  environment, so the pin could not be recorded here.
- **macOS worker-side resource caps** (only `RLIMIT_CPU`; Seatbelt has no
  memory/pids cap) — the accepted platform asymmetry, now with the concrete gap
  named.
- **Force-routing is opt-in** (`KASTELLAN_EGRESS_FORCE_ROUTING=1`, on in the
  supervised deployment, loud when off). Recommendation: make it the default
  before release and require an explicit opt-out.

## Verified sound (re-checked, not re-listed as findings)

Sandbox argv builders (bwrap, Seatbelt profile injection guards, cgroup scope
fail-closed, Firecracker plan/mounts/confine, guest-kernel pin ordering);
`tool_host::dispatch` chokepoint sealing; JSON-RPC framing (64 MiB record cap,
id check, serde depth); secrets crypto (AES-256-GCM, fresh nonces, AAD binding,
zeroization, keyring-only key); pairing (160-bit codes, DB-enforced expiry and
single use, constant-time compare, authorize-before-screen); email DMARC gate
parsing; every DB query parameter-bound, triggers `search_path`-pinned, no
`SECURITY DEFINER`; supervisor unit/plist escaping and 0600 env files;
`atomic_write`; the privileged scripts' pin verification; egress allowlist
matcher, IP pinning, MITM upstream trust (webpki + SNI = CONNECT host),
leak-scan cross-record carry-over; python-exec interpreter flags and caps; mail
attachment path handling; web-research/search input clamps; microvm cmdline
hex encoding.

## Gates

Audit container (x86_64, root, no bwrap / Landlock / KVM / unprivileged Postgres):
`cargo test --workspace --no-fail-fast` → **3980 passed / 4 failed / 55 ignored, 176
suites**; the 4 failures are environment-only (three `initdb: cannot be run as root`,
one chmod-based test root ignores) and reconcile to the DGX baseline plus 44 new tests.
`cargo clippy --workspace --all-targets --locked -- -D warnings` clean on rustc 1.98.0;
`cargo audit` 0 vulnerabilities; `pip-audit` clean; license sweep clean. See
`docs/devel/handovers/HANDOVER.md` for the authoritative table. Hermetic suites touched by the fixes were run
individually as each fix landed (prelude 61, sandbox 172, core lib 1990+,
egress-proxy 66, llm-router, web-common 89 with `--all-features`, python-exec
42, microvm-init 24, matrix-wire 8, net-classify 14, browser-driver pytest 46).
The Firecracker and live-Matrix paths compile (`--features live-matrix`
checked) but need the DGX for their e2e gates.

CI on PR [#660](https://github.com/hherb/kastellan/pull/660): all checks green on
`ae3ead6` — workspace check + clippy, the live-matrix check + clippy, `uv lock --check`,
and CodeQL (rust, python, actions). Two follow-ups landed after the audit commit: a
`manual_contains` lint that only the live-matrix clippy job compiles, and five CodeQL
`rust/cleartext-logging` alerts on the guest's privilege drop, which had interpolated the
numeric uid into stderr and panic messages; the guest now never echoes the value.
