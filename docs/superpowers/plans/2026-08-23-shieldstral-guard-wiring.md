# Plan — Shieldstral guard tier, the wiring slice

**Spec:** [`2026-08-22-shieldstral-guard-wiring-design.md`](../specs/2026-08-22-shieldstral-guard-wiring-design.md)
(amended 2026-08-23 with M2 + D8/D9/D10)
**Closes:** [#586](https://github.com/hherb/kastellan/issues/586); addresses
[#604](https://github.com/hherb/kastellan/issues/604) at the wiring layer.
**Baseline:** `main` `d51c9b20`, DGX **3759 / 0 / 54**.

Seven tasks. Tasks 1–4 are pure/unit work with no server and no Postgres; task 5 is the
chokepoint thread; task 6 is boot; task 7 is the e2e that pins what the chokepoint actually
does. **TDD throughout: the test that can fail is written before the code that satisfies it.**

---

## Task 1 — `llm-router`: the fields the tier needs

**Files:** `llm-router/src/messages.rs`, `llm-router/src/config.rs`

1. `Usage` gains `prompt_tokens_details: Option<PromptTokensDetails>` with
   `cached_tokens: Option<u32>`. Additive, `#[serde(default)]`, so no backend breaks.
   **Measured shape** (M2): `{"prompt_tokens":810,"prompt_tokens_details":{"cached_tokens":809}}`.
2. `RouterConfig` gains `guard_tau: Option<f32>` (`KASTELLAN_LLM_GUARD_TAU`) and
   `guard_timeout_ms: Option<u64>` (`KASTELLAN_LLM_GUARD_TIMEOUT_MS`). Both parse-or-`Err`,
   the shape `KASTELLAN_LLM_TIMEOUT_MS` already uses. **`RouterConfig` parses; it does not
   validate the range** — τ's range is a security decision and lives beside `decide` (task 3).
3. **`for_guard` takes the timeout as a parameter:** `for_guard(&self, timeout: Duration)`.
   The tri-state is unchanged; only the returned config's `timeout` stops being inherited.
   One method, not two, so the two cannot drift — and the compiler finds every caller.
   The existing `for_guard_..._timeout_is_inherited` test **changes**, as D2 predicted.

**Tests (unit, in-crate):** `usage` round-trip with and without `prompt_tokens_details`;
`guard_tau`/`guard_timeout_ms` present, absent, and non-numeric; `for_guard` sets the passed
timeout and never the parent's.

## Task 2 — D8: the context pin (`core/src/cassandra/guard_model/context_pin.rs`)

Pure module, `weights_pin`'s shape and for the same reasons.

```rust
pub const GUARD_PROMPT_OVERHEAD_TOKENS: u64 = 512;
pub const REQUIRED_GUARD_N_CTX: u64 = SCAN_BYTE_CAP as u64 + GUARD_PROMPT_OVERHEAD_TOKENS;

pub fn n_ctx_from_props(props: &serde_json::Value) -> Option<u64>;
pub fn context_verdict(reported: Option<u64>, required: u64) -> Result<u64, GuardContextError>;

pub enum GuardContextError { NoContextSize, TooSmall { reported: u64, required: u64 } }
```

`n_ctx_from_props` reads `default_generation_settings.n_ctx` first (**measured** as the
per-request number on the build both hosts run), falling back to a top-level `n_ctx`.
`context_verdict` takes `required` as a **parameter** so its accepting arm is reachable from
a unit test — the #598 rule, [[unreachable-success-path-proves-nothing]].

**Tests:** nested field; top-level fallback; neither; every non-numeric shape (null, string,
object, negative, float); `required - 1` refuses, `required` **passes**, `required + 1`
passes; and `REQUIRED_GUARD_N_CTX == SCAN_BYTE_CAP + GUARD_PROMPT_OVERHEAD_TOKENS` asserted
against the constants, not a literal.

## Task 3 — D9: the timeout derivation (`core/src/cassandra/guard_model/timeout.rs`)

Pure module. All arithmetic, clamping and basis reporting; no IO.

```rust
pub enum ProbeOutcome {
    Measured { uncached_tokens: u32, elapsed_ms: u64 },
    TooFewUncachedTokens { uncached_tokens: u32, elapsed_ms: u64 },
    NoTokenCount,
    Saturated { budget_ms: u64 },
    Failed { why: String },
}
pub enum Clamped { No, ToFloor, ToCeiling }
pub enum TimeoutBasis { Operator, Probed { tok_per_s: f32, derived_ms: u64, clamped: Clamped }, Unprobed { why: &'static str } }
pub struct GuardTimeout { pub timeout: Duration, pub basis: TimeoutBasis }

pub fn derive_guard_timeout(outcome: &ProbeOutcome) -> GuardTimeout;
pub fn guard_timeout_from(override_ms: Option<u64>, outcome: &ProbeOutcome) -> GuardTimeout;
pub fn probe_sample(prompt_tokens: Option<u32>, cached_tokens: Option<u32>, elapsed_ms: u64) -> ProbeOutcome;
```

`probe_sample` is where `cached_tokens` is subtracted and `MIN_UNCACHED_PROBE_TOKENS`
applied — pure, so M2 row 3 becomes a fixture. Constants per the spec's table.
**`Saturated` derives the CEILING**, the row a plausible implementation gets backwards.

**Tests:** every row of both spec tables; M2's DGX numbers (810 uncached / 160 ms →
~5,060 tok/s → ~26 s, `Clamped::No`); measurement 3's Mac (~135 tok/s → `ToCeiling`); a very
fast host → `ToFloor`; `probe_sample` with `cached=809/810` → `TooFewUncachedTokens`
(the mutation fixture); `elapsed_ms == 0` → not a division by zero; `override_ms` wins
without consulting the outcome.

## Task 4 — D1 + D4: the tier (`core/src/cassandra/guard_model/tier.rs`)

```rust
pub enum Unadjudicated { NotConfigured, Unmeasured, RouterError }
pub enum GuardOutcome { Block, Allow, AllowUnadjudicated { reason: Unadjudicated } }

pub fn consults_model(catalogue_score: f32) -> bool;      // score < BLOCK_THRESHOLD
pub fn validate_tau(tau: f32) -> Result<f32, TauError>;   // (0.0, 1.0], finite
pub fn resolve(adj: Option<Result<GuardAdjudication, ()>>) -> GuardOutcome;

pub struct GuardTier { client: GuardClient, tau: f32, timeout: GuardTimeout, n_ctx: u64 }
impl GuardTier {
    pub async fn from_router_config(cfg: &RouterConfig) -> Result<Option<Self>, GuardTierError>;
    pub async fn adjudicate_document(&self, body: &str) -> (GuardOutcome, Option<f32>, u64);
}
```

`from_router_config` is the boot sequence: tri-state → validate τ → build a **probe client**
at `PROBE_BUDGET_MS` → `/props` → D8 context check (**fatal**) → D9 probe (**never fatal**)
→ build the production client at the derived timeout. τ validation and the arm logic are
pure and tested without a server; only the sequencing needs one.

**Tests:** `resolve` over all doors; `consults_model` at `BLOCK_THRESHOLD - ε`, exactly at
it, and above; `validate_tau` over `{-0.1, 0.0, 1e-7, 0.79552656, 1.0, 1.0000001, NaN, inf}`.

## Task 5 — the chokepoint thread (D3, D5)

`ToolHostStepDispatcher` gains `guard: Option<Arc<GuardTier>>`, threaded
`dispatch` → `dispatch_with_sink` → `post_process::finalize`, exactly as `vault` was.

`finalize`: after the catalogue verdict, on `Allow` **and** when the tier is present, await
`adjudicate_document`. `post_process` stays thin — it awaits, calls `resolve`, emits.

Audit (D5): `policy / injection.blocked` gains `tier: "catalogue" | "guard_model"` plus `p`
and `tau` on the guard arm; the per-dispatch tool row gains a `guard` sub-object
`{state, p, tau, ms}` whenever the tier ran — **including on a cleared document**, which is
the decision's whole point.

## Task 6 — boot (D6)

`main.rs`: build the tier beside the existing "log what was actually resolved, once" block;
`?` on `GuardTierError` (a half-configured or unverifiable tier stops the daemon — D6, D8);
one `info!` carrying endpoint, model, τ, timeout **and its basis**, `n_ctx`, `policy_digest`;
an explicit not-configured line otherwise; a boot audit row. A `ToCeiling` clamp is a
`warn!`, not an `info!` — it is a finding about the host.

## Task 7 — layer 2: `core/tests/guard_tier_e2e.rs`

Real `dispatch_with_sink`, real worker, real Postgres, guard pointed at a **mock HTTP server
that returns what it was sent** (slice 1's lesson). Covers all four doors end to end, the
`tier` field on the block row, the `guard` sub-object on a **cleared** document, and the one
assertion layer 1 cannot make: **a catalogue Block leaves the mock with zero requests
received.**

---

## Verification

1. `cargo test -p kastellan-llm-router` + `-p kastellan-core` (lib) on the Mac as each task lands.
2. Full DGX sweep `--workspace --no-fail-fast -- --nocapture`, log under `$HOME` not `/tmp`
   ([[dgx-run-logs-tmp-scrubbed]]), **predict the count and reconcile the delta exactly.**
3. `cargo clippy --workspace --all-targets -- -D warnings` on **both** hosts — count the
   `Checking` lines ([[mac-cargo-buildlock-prefer-dgx]]); CI's rust 1.97 is the authority
   ([[local-clippy-not-ci-parity-rust-version]]).
4. Mutation-test the **eleven** named targets (six from the original spec, five from D8/D9),
   each against the layer named for it.
