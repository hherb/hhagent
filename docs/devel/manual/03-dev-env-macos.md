# 3 — Dev environment: macOS

Tested on macOS 14 Sonoma and 15 Sequoia, Apple Silicon (M-series) and Intel.
macOS 26 (Tahoe) adds Apple's `container` CLI for micro-VM workers; that is
optional and not required for basic development.

---

## Step 1 — Install Xcode Command Line Tools

```sh
xcode-select --install
```

This installs `clang`, `make`, and the macOS SDK headers that Rust's C
dependencies need.

---

## Step 2 — Install Homebrew (if not already installed)

```sh
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
```

Follow the on-screen instructions to add Homebrew to your PATH.

---

## Step 3 — Install Rust

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustc --version    # 1.78+ required; the dev box + CI track current stable
```

---

## Step 4 — Install Postgres

```sh
brew install postgresql@18
```

Homebrew installs Postgres but does not start it automatically. The project
manages its own per-user Postgres instance, so you do not need the Homebrew
service running. Initialise the per-user cluster:

```sh
cargo run -p kastellan-db --bin kastellan-db-init
```

This creates a cluster in `~/.local/share/kastellan/postgres/` configured for
Unix socket connections with peer auth.

---

## Step 5 — Verify sandbox-exec availability

kastellan uses `sandbox-exec` (macOS Seatbelt) to isolate worker processes.
It ships with macOS and needs no extra install. Confirm it is present:

```sh
which sandbox-exec    # should print /usr/bin/sandbox-exec
```

No AppArmor profile step is needed on macOS.

---

## Step 6 — First build

```sh
source "$HOME/.cargo/env"
cargo build --workspace
```

First build takes 2–5 minutes. Subsequent incremental builds are fast.

---

## Step 7 — Run the test suite

```sh
cargo test --workspace -- --nocapture
```

Healthy output on macOS is `0 failed` across every crate, with a small
number of `ignored` tests. The exact pass count grows commit by commit
(see the latest `HANDOVER.md`). Ignored tests need the Apple `container`
CLI (macOS Tahoe+) or a real GLiNER model; neither is required for
normal development.

---

## Optional: Local LLM for integration tests

The scheduler integration tests that call `formulate_plan` need an LLM. On
macOS the default is **oMLX** on `:8000` — it serves MLX-quantised models
through the same OpenAI-compatible surface and is materially faster than
Ollama on Apple silicon, the gap widening with model size. Install its
models from oMLX's own admin UI, then:

```sh
export KASTELLAN_LLM_LOCAL_URL=http://127.0.0.1:8000/v1
export KASTELLAN_LLM_LOCAL_MODEL=Qwen3.8-27B-8bit
export KASTELLAN_LLM_EMBEDDING_MODEL=embeddinggemma-300m-bf16
```

These are the defaults, so an unset environment reaches the same place; set
them explicitly when you want a different model.

### The macOS fallback runtime: llama.cpp

oMLX is the default, not the only option. **llama.cpp's `llama-server` is the
designated fallback on macOS** for anything oMLX cannot serve — it also speaks
OpenAI-compat, so it needs no code, only a URL. Point the router at it with an
explicit `KASTELLAN_LLM_LOCAL_URL` (llama.cpp has no conventional port, which
is why it is not a default anywhere).

The concrete gap driving this today is **token logprobs**. oMLX does not
return them: `logprobs`/`top_logprobs` are absent from `/v1/chat/completions`
entirely, and while `top_logprobs` *is* declared on `/v1/responses` it is
accepted and ignored — no response schema in oMLX's OpenAPI document emits
them. Measured against the live server on 2026-08-15; re-check when oMLX
updates, since a declared-but-inert parameter suggests the wiring is partly
there.

Anything needing a **calibrated score** rather than a bare token therefore
cannot use oMLX. The live example is the Phase 5 model-based guard tier
(Shieldstral), whose whole design rests on renormalising the `yes`/`no`
logprobs into a confidence band — see
[`docs/superpowers/specs/2026-08-13-shieldstral-guard-model-feasibility-study.md`](../../superpowers/specs/2026-08-13-shieldstral-guard-model-feasibility-study.md).
llama.cpp is reported to cover both halves that model needs, logprobs and
multimodal; that is the fallback's first real customer and the measurement
that will confirm it.

Ollama also still works, and remains the Linux install default:

```sh
brew install ollama
ollama serve &          # runs in background
ollama pull gemma2:9b   # or any OpenAI-chat-compatible model
export KASTELLAN_LLM_LOCAL_URL=http://127.0.0.1:11434/v1
export KASTELLAN_LLM_LOCAL_MODEL=gemma2:9b
```

Most unit and integration tests use a mock HTTP server and do not require a
real LLM. Only the end-to-end `observation` tests and the `cli_ask_e2e` test
do live LLM calls.

---

## Troubleshooting

| Symptom | Likely cause | Fix |
|---------|-------------|-----|
| `sandbox-exec: Operation not permitted` | SIP or permissions issue | Check System Preferences → Privacy & Security |
| `connection refused` in Postgres tests | DB not running | `cargo run -p kastellan-db --bin kastellan-db-init` |
| `command not found: cargo` | Rust env not sourced | `source "$HOME/.cargo/env"` |
| Seatbelt `[SKIP]` in sandbox tests | Sandbox probe failed | Run `cargo test -p kastellan-sandbox -- --nocapture` to see why |
