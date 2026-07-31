# 5 — Build, test, and run

---

## Before any command

Cargo is not on the default PATH in non-interactive shells. Always source the
Rust environment first, or add this line to your shell profile:

```sh
source "$HOME/.cargo/env"
```

---

## Building

```sh
# Build everything
cargo build --workspace

# Build only one crate (faster when iterating on a specific piece)
cargo build -p kastellan-sandbox
cargo build -p kastellan-core
```

The workspace builds clean and CI gates on
`cargo clippy --workspace --all-targets -D warnings`. Do not introduce new
warnings.

---

## Running the test suite

```sh
# All tests in all crates
cargo test --workspace

# All tests, with stdout/stderr shown (useful for diagnosing [SKIP] messages)
cargo test --workspace -- --nocapture

# One crate
cargo test -p kastellan-sandbox

# One integration test file
cargo test -p kastellan-sandbox --test linux_smoke

# One test by name substring
cargo test -p kastellan-sandbox argv_starts_with_bwrap
```

### What "ignored" means

The test suite typically reports a small number of ignored tests. These are
tests that require hardware or services that may not be present:

- Apple `container` CLI (macOS Tahoe+ only)
- A real GLiNER/ReLeX ML model
- A real frontier LLM endpoint

Ignored tests are not failures. They skip gracefully and do not affect the
green/red status of a CI run. The total pass count grows over time; check
`HANDOVER.md` for the current target rather than memorising a fixed number.

### What `[SKIP]` lines mean

Unlike `ignored`, `[SKIP]` lines appear in test output when a sandbox test
detects that `bwrap` or `sandbox-exec` is not functional on the current host.
**A green run with `[SKIP]` lines is a false positive** — the tests passed
without actually testing containment.

To check whether real sandboxing is active:

```sh
cargo test -p kastellan-sandbox -- --nocapture 2>&1 | grep -E 'SKIP|ok'
```

If you see `[SKIP]` on Linux, re-run the AppArmor setup from
[chapter 2](./02-dev-env-linux.md#step-3).

---

## Running the daemon

The agent core binary is built at `target/debug/kastellan`:

```sh
./target/debug/kastellan
```

It starts, runs migrations (if needed), and then blocks waiting for tasks.
Exit with Ctrl-C.

The CLI for interacting with a running daemon:

```sh
./target/debug/kastellan-cli --help
./target/debug/kastellan-cli audit tail
./target/debug/kastellan-cli tasks list
./target/debug/kastellan-cli ask "Summarise the last 5 emails from Alice"
```

---

## Useful environment variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `KASTELLAN_LLM_LOCAL_URL` | `http://127.0.0.1:8000/v1` (Linux) / `:11434/v1` (macOS) | Local LLM endpoint |
| `KASTELLAN_LLM_LOCAL_MODEL` | `""` | Model name to pass to the local LLM |
| `KASTELLAN_SHELL_ALLOWLIST` | `[]` | JSON array of allowed argv patterns (read by the shell-exec worker) |
| `KASTELLAN_EGRESS_FORCE_ROUTING` | `1` (on) | Route `Net::Allowlist` workers through their per-worker egress proxy. Fail-closed. |
| `KASTELLAN_WEB_SEARCH_ENDPOINT` | (unset) | SearxNG `/search` endpoint for the web-search worker |
| `KASTELLAN_{WEB_FETCH,WEB_SEARCH,WEB_RESEARCH,BROWSER_DRIVER}_ALLOWLIST` | (per worker) | Per-worker `host:port` egress allowlist for the net workers |
| `KASTELLAN_MAIL_ENDPOINT` | (unset) | localmail `/v1` base URL for the read-only mail worker |
| `KASTELLAN_MAIL_TOKEN_FILE` | (unset) | File holding the mail worker's localmail bearer token |
| `KASTELLAN_MAIL_ATTACHMENT_MAX_BYTES` | 25 MiB | Size cap for `mail.get_attachment` downloads |
| `KASTELLAN_EMAIL_ENDPOINT` | (unset) | Enables the inbound email channel (localmail `/v1`); unset ⇒ channel absent |
| `KASTELLAN_EMAIL_{SUBSCRIPTION,ADDRESS,AUTHSERV_ID,TOKEN_FILE}` | — | Required once the email channel is enabled (`KASTELLAN_EMAIL_WORKER_BIN` is optional) |
| `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA` | (unset) | Extra CA anchor for the egress proxy's upstream leg, keyed by a single private-IP origin |
| `KASTELLAN_STATE_DIR` | `~/.local/state/kastellan` | Audit JSONL output directory |

### Opt-in worker enable flags

Some workers are gated behind an explicit enable flag (off by default):

| Flag | Worker |
|------|--------|
| `KASTELLAN_PYTHON_EXEC_ENABLE=1` | `python-exec` |
| `KASTELLAN_BROWSER_DRIVER_ENABLE=1` | `browser-driver` |
| `KASTELLAN_GLINER_RELEX_ENABLE=1` | `gliner-relex` (its `#[ignore]` real-model e2e tests) |

### Micro-VM backend flags (Linux, opt-in)

| Flag | Default | Purpose |
|------|---------|---------|
| `KASTELLAN_PYTHON_EXEC_USE_MICROVM=1` | off | Run `python-exec` inside a Firecracker micro-VM instead of bwrap |
| `KASTELLAN_WEB_FETCH_USE_MICROVM=1` | off | Same, for `web-fetch` |
| `KASTELLAN_WEB_SEARCH_USE_MICROVM=1` | off | Same, for `web-search` |
| `KASTELLAN_WEB_RESEARCH_USE_MICROVM=1` | off | Same, for `web-research` |
| `KASTELLAN_BROWSER_DRIVER_USE_MICROVM=1` | off | Same, for `browser-driver` |
| `KASTELLAN_MICROVM_CONFINE_VMM=0` | on | Opt **out** of wrapping the `firecracker` VMM process in its own bwrap+cgroup jail (confinement is on by default, fail-closed) |

---

## Python worker tests

The Python workers (`gliner-relex`, `browser-driver`) live outside the Cargo
workspace. Their tests use `uv` (a fast Python package manager):

```sh
cd workers/gliner-relex && uv run pytest
cd workers/browser-driver && uv run pytest
```

These tests are independent of the Rust test suite — they run against the
Python source directly without spawning a Rust process. End-to-end tests that
drive these workers from `core` (under the real sandbox) live in `core/tests/`
and are gated behind `#[ignore]` + an enable flag, so they only run when the
model / browser engine is installed.
