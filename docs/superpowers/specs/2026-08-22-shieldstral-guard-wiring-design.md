# Shieldstral guard tier — the wiring slice

**Status:** design, 2026-08-22
**Predecessor:** [`2026-08-21-shieldstral-guard-slice-1-design.md`](2026-08-21-shieldstral-guard-slice-1-design.md)
(merged as [#585](https://github.com/hherb/kastellan/pull/585), `f90631da`)
**Closes:** [#586](https://github.com/hherb/kastellan/issues/586)

**Approved by the operator 2026-08-22**, together with the measurement-3 spec, and with
three constraints recorded that resolve two of the open risks below: the ~3.5 s cost is
acceptable, the 15 s bound is realistic, and GPU sharing is acceptable because kastellan's
hardware floor is **M-series or DGX-class by design**. The tier's purpose is what buys that
budget: kastellan targets **high-risk environments such as healthcare**.

**AMENDED 2026-08-23**, after measurement 3 shipped (`d51c9b20`,
[#606](https://github.com/hherb/kastellan/pull/606)). The run produced **τ = 0.79552656** —
so this spec is unparked — and three obligations it could not have anticipated. Two are
correctness and both are attacker-reachable; the third is honesty about what the tier buys.
They are **D8**, **D9** and **D10**, added below, together with the measurement **M2** that
D9 rests on. **D9 is amended by D11** (multi-sample probe, issue #624). **D2 is superseded by D9** and kept for its derivation. Operator decisions
recorded 2026-08-23: fail open at runtime *plus* a boot-time context check (D8), and derive
the timeout per host from a boot probe (D9).

Slice 1 landed the guard endpoint seam, the adjudicator and the calibration harness, and
deliberately shipped **no production wiring** — five chokepoint files were verified
byte-identical to `main` as a merge gate. This slice puts the tier on the dispatcher path.

It also discharges slice 1's Open risk 1, which that slice made a hard precondition on this
one: *"the wiring slice must not proceed without it."* The measurement is M1 below, and it
is the reason several decisions here differ from what slice 1 anticipated.

---

## M1 — The size sweep, which was the precondition

Run 2026-08-22 on the DGX (GB10, aarch64) against `llama-server` serving the pinned
`Shieldstral-1.0-3B-Q8_0.gguf`, `-c 32768 -ngl 99`, `policy_digest=342e3d9661b2cbe2`.
Six runs of `live_shieldstral_size_sweep` (one initial + five repetitions), one sample per
(size, kind) per run.

| document | prompt tokens | attack (ms) | benign (ms) |
| --- | --- | --- | --- |
| 1 KiB | 314 | 84, 453, 457, 412, 404, 405 | 27–72 |
| 8 KiB | 1,259 | 212, 199, 64, 59, 58, 56 | 252–504 |
| **64 KiB** (`SCAN_BYTE_CAP`) | **10,062** | 2343, 3189, 3206, 3224, 3225, 3226 — **p50 3,215** | 2929, 3531, 3534, 3582, 3585, 3586 — **p50 3,558** |

The 1 and 8 KiB cells are in run order; the two 64 KiB cells are **sorted**, so the median
is readable directly. The lowest value in each 64 KiB cell is the very first run — see
caveat 1.

**The headline: ~3.5 s at the cap, against a study number of 30–43 ms.** Measurement 1's
p50 was taken on ~26-token strings. At `SCAN_BYTE_CAP` the tier is roughly **85× slower**
than the number every earlier document quotes. Anyone reasoning from the study's latency
figure is reasoning about a different workload.

**The cost is entirely prompt processing, and it is linear.** Server-side timings show
decode at *one* token and `0.00 ms`; prompt eval runs at **4,039–6,660 tok/s** across every
size. So the tier's cost on any host is predictable from that host's prompt-eval throughput
rather than needing a fresh sweep — and a CPU-only host at a typical ~200 tok/s would take
**~50 s** for the same document. That is not a tier that host can run, and D2 is shaped so
it degrades rather than stalls.

**4,039–6,660 tok/s is proof of real GPU offload,** not a silent-CPU-fallback artifact — a
3B Q8 on CPU is ~100–300 tok/s. (The log's `unsupported ops (backend=CUDA0)` warning is the
*CLIP/vision* graph, which text adjudication never touches. Checked, because
[[dgx-apt-upgrade-drops-nvidia-module]] is a standing way to measure the wrong machine.)

**Correctness held in both directions at every size across all six runs, with zero
`Unmeasured`.** The sweep asserts the attack Flags and the benign Clears precisely so a
backend returning garbage quickly cannot look good.

### Two caveats that must travel with these numbers

1. **The 1 and 8 KiB rows are contaminated by llama-server prefix caching and read
   optimistic.** The sweep sends byte-identical documents on every repetition, which real
   dispatches will not. The tell is visible in the table: the 8 KiB attack settles at
   ~56 ms, *faster* than the 1 KiB attack at ~405 ms. Only the 64 KiB rows are stable
   enough to derive a bound from, and they are the ones that matter for a timeout.
2. **The GPU was otherwise idle.** On a single-host deployment the guard shares the GPU
   with the planner model. Under contention with a 26B planner these numbers get worse, by
   an amount nobody has measured. This is Open risk 3 and it is the main argument for D2's
   headroom.

---

## Design decisions

### D1 — τ is a required operator input with no default, and its range is validated

The tier reads `KASTELLAN_LLM_GUARD_TAU` and applies the same tri-state that
`RouterConfig::for_guard` already applies to the URL/model pair:

| `guard_url` + `guard_model` | `KASTELLAN_LLM_GUARD_TAU` | outcome |
| --- | --- | --- |
| unset | unset | `Ok(None)` — no guard, expected, not an error |
| set | set | `Ok(Some(tier))` |
| set | unset | `Err` — misconfiguration |
| unset | set | `Err` — misconfiguration |

**There is deliberately no default.** Slice 1's D9 says a provisional τ "must never become a
default", and says it in four places — the const's doc, the report footer, the corpus README,
and D9 itself. Four paragraphs are four things a future session can skim past. Requiring the
value makes D9 a property of the code: `DEFAULT_TAU` stays reachable only from the
calibration harness, and the wiring has no way to reach for it.

**Consequence, stated plainly: this slice ships a tier nobody should turn on yet.**
Measurement 3 (the ≥100-case corpus with a captured half) is still owed, and until it exists
any τ an operator supplies is their own provisional number. That is the honest position —
the alternative is a default that makes an unfitted threshold look sanctioned.

**Both ends of the range are refused, because both are silent failures:**

- **τ ≤ 0** — `p >= 0.0` is true for every probability, so the tier blocks *every* document
  the catalogue allowed. That is a denial of service on the entire tool path, arriving as
  "the agent stopped being able to read anything".
- **τ > 1** — no probability can reach it, so the tier never flags. It looks configured, logs
  as configured, costs 3.5 s per document, and is off.

A non-finite τ is refused for the same reason `decide` routes a non-finite `p` to
`Unmeasured`: `NaN` comparisons are all false, so a `NaN` τ is the τ > 1 failure wearing a
different hat.

### D2 — The guard timeout is derived from M1, not chosen — **SUPERSEDED BY D9**

> **Superseded 2026-08-23.** The derivation below is sound and its arithmetic is
> unchanged; what it got wrong is the *input*. It reasons from M1's 10,062 tokens for a
> 64 KiB document, and #604 measured the same cap producing **44,437** tokens on
> adversarial text — M1's material was prose at ~6.5 bytes/token, and dense jailbreak
> text runs at 1.47. It also assumed one host's throughput generalises; measurement 3
> found the Mac ~40× slower on the same document. **D9 replaces the constant with a
> boot-time measurement.** `KASTELLAN_LLM_GUARD_TIMEOUT_MS` survives as an operator
> override and 15 000 ms survives as the floor.


`RouterConfig` gains `guard_timeout: Duration`, read from `KASTELLAN_LLM_GUARD_TIMEOUT_MS`,
default **15 000 ms**. `for_guard` sets the returned config's `timeout` from it instead of
inheriting `timeout` through the `..self.clone()`.

**The derivation, so it can be re-derived rather than re-guessed:** M1's measured maximum at
`SCAN_BYTE_CAP` is 3,586 ms. 15 s is ~4× that. It covers a host roughly four times slower
than the GB10 at a full 64 KiB document, plus the GPU-contention headroom Open risk 3 says
is unmeasured.

**It deliberately does not cover a CPU-only host** (~50 s by M1's linearity), and that is a
**system requirement, not a shortfall**. Kastellan's floor is M-series or DGX-class hardware
by design (operator, 2026-08-22); it is not built for lightweight machines. On a host below
that floor the right behaviour is to time out and fail open to catalogue-only screening —
today's behaviour — rather than stall every dispatch for the better part of a minute. A
timeout stretched to accommodate the slowest conceivable host would defeat the purpose of
having one.

**15 s is confirmed realistic by the operator**, as is the ~3.5 s per-document cost at the
cap.

This closes #586. The existing `for_guard` test asserting `guard.timeout == cfg.timeout`
**changes**, and that is the intended outcome: its own doc records the inheritance as
"observed, not endorsed".

### D3 — The tier reaches the chokepoint as a threaded parameter, exactly like `vault` did

`ToolHostStepDispatcher` gains `guard: Option<Arc<GuardTier>>`, threaded
`dispatch` → `dispatch_with_sink` → `post_process::finalize`. This is the same shape
`vault` took in Item 31, in the same functions, so it follows a path the repo has already
reviewed once.

**Rejected: a process-global `OnceLock`.** It would avoid the threading, and it would make
the dependency invisible at exactly the site where a reviewer needs to see it, while making
it impossible for two tests in one binary to configure different tiers.

**Rejected: a new public `DocumentAdjudicator` trait.** A public trait in a published crate
whose external implementor decides a containment outcome is precisely
[#590](https://github.com/hherb/kastellan/issues/590), filed four days ago against
`AskResolver`. Repeating it deliberately would be indefensible. The concrete `GuardTier`
is threaded instead. The arm logic is pinned by the pure `resolve` (D4) at the unit layer
and by a real `GuardTier` against a mock HTTP server at the chokepoint layer — two layers,
no injectable fake, which is the same split [#587](https://github.com/hherb/kastellan/pull/587)
used for containment.

### D4 — The wiring shape, and the pure function that holds it

Slice 1's D4, unchanged, now with every door named:

```
catalogue >= BLOCK_THRESHOLD  ->  Block, model NOT consulted
catalogue <  BLOCK_THRESHOLD  ->  guard configured?
                                    no  -> Allow, audited (NotConfigured)
                                    yes -> probability()
                                             Err(..)     -> Allow, audited (RouterError)
                                             Unmeasured  -> Allow, audited (Unmeasured)
                                             Flagged     -> Block
                                             Clear       -> Allow
```

**Escalate-up only.** The model can turn an `Allow` into a `Block` and never the reverse, so
every failure mode of this tier is at worst today's catalogue-only behaviour.

The mapping lives in `cassandra/guard_model/tier.rs` as a **pure, total function** over an
explicit outcome enum:

```rust
pub enum GuardOutcome {
    Block,
    Allow,
    AllowUnadjudicated { reason: Unadjudicated },
}

pub enum Unadjudicated { NotConfigured, Unmeasured, RouterError }
```

Three reasons this is a named enum rather than a `bool` plus a log line. It keeps slice 1's
ban on an `escalates() -> bool` helper intact — a caller consuming a bool structurally
cannot audit the distinction D4 requires it to audit. It makes the three fail-open doors
*countable* in the audit log, which is what Open risk 5 asked for. And it makes the pure
mapping exhaustively testable without a server, which is where the security decisions
actually get pinned.

**`post_process::finalize` stays thin.** It awaits, calls `resolve`, and emits. It does not
contain the policy.

### D5 — One block event with a `tier` field, and `p` on every adjudicated dispatch

**Blocks reuse `policy / injection.blocked`,** gaining `tier: "catalogue" | "guard_model"`
alongside `p` and `tau` on the guard arm. The operator-facing question is "what was withheld
from the planner", and splitting its answer across two event names means every forensic
query written before this slice silently under-reports the moment the tier is switched on.
The repo has been bitten by that class of silent miss repeatedly. Consumers that assumed
`injection.blocked` implies a catalogue score must read the new field — that cost is
explicit and one-time, where the alternative's cost is invisible and permanent.

**The per-dispatch tool audit row gains a `guard` sub-object** — `{state, p, tau, ms}` —
whenever the tier ran. No new rows, so the row count per dispatch is unchanged.

This is the decision with the most leverage in the slice, and the reason is measurement 3.
`p` is the raw probability, and recording it on **cleared** documents as well as blocked ones
makes production itself the source of the real-world score distribution that slice 1 still
owes. It is the only way to learn what `p` looks like on genuine worker output rather than
on seeded cases, and the cleared half is exactly the half needed both to fit τ and to notice
a distribution that has gone degenerate. `p` is a float, not document content; nothing
sensitive is added to any audit column.

**Fail-open doors are audited too**, via the same `guard.state` field carrying the
`Unadjudicated` reason. A tier that is silently absent — endpoint down, unconfigured, or
returning no verdict pair — is then a query rather than a hunch.

> **AMENDED 2026-08-24 ([#616](https://github.com/hherb/kastellan/issues/616)).** `state`
> names the *door*, and one door — `router_error` — was carrying four different failures:
> a request timeout, a refused connection, an HTTP status and a decode failure. That made
> the paragraph above true and insufficient: the fail-open of
> [#612](https://github.com/hherb/kastellan/issues/612) is *specifically* the timeout, and
> counting it meant correlating `router_error` rows against a large `body_byte_len` and an
> `ms` near the budget across a rotating log. A companion field `guard.error_kind` now
> rides beside `state`, `null` unless the call failed. It is a **closed discriminant**, not
> the backend's error text — the no-backend-message rule below is unchanged, and every
> possible value is a `&'static str` this repo wrote.

### D6 — The tier is built and reported once at boot, and a misconfiguration refuses to boot

D1 of slice 1 requires the enabled-but-unconfigured case be reported "once at boot, loudly,
not per-call — a per-call warning on the dispatcher hot path is its own denial of service".
That report extends the existing "log what was actually resolved, once" block in
`main.rs`, rather than inventing a second pattern: endpoint, model, τ, timeout and
`policy_digest` on the configured path; an explicit not-configured line otherwise. A boot
audit row makes it queryable alongside everything else.

**A half-configured tier stops the daemon.** `main.rs` already `?`s on
`RouterConfig::from_env`, so refusing to boot on a bad LLM config is established precedent
rather than a new severity. `for_guard`'s own doc argues the substance: a half-configured
tier "is a misconfiguration, not an opt-out". The concrete hazard is documented and has
happened — `kastellan-cli install` regenerates `kastellan.env` and has been observed
dropping hand-added keys ([[dgx-deploy-env-clobber-and-missing-workers]]), which would
otherwise turn the security tier off behind a correct-looking log line.

The counter-argument was weighed and rejected: a down daemon protects nothing, so stopping
on a fail-open tier's misconfiguration looks disproportionate. It loses because "loud error
at boot" is precisely the thing that gets scrolled past, and because the failure this guards
is *silent deactivation of a security control*, which is the one failure mode the whole
slice exists to prevent.

### D7 — The wiring calls `probability()` + `decide()`, not `adjudicate()`

D5 needs the raw `p`, and `adjudicate` discards it. `adjudicate` is already a thin delegate
over `probability` + `decide`, so calling the two directly does **not** create a second
request-building path — `probability` remains the only one, which is the property slice 1's
module doc actually protects. It is also what `kastellan-cli guard calibrate` already does
for the same reason, so production and calibration end up on the same path rather than
diverging.

`adjudicate` stays as the tested convenience its e2e suite already exercises.

---

## M2 — The boot probe, measured before it was specified

Run 2026-08-23 on the DGX against the same `llama-server` measurement 3 used
(`Shieldstral-1.0-3B-Q8_0`, `-c 131072`, `-ngl 99`, port 8081). Three requests, one probe
document of **1024 bytes of token-dense text** (mixed case, digits, symbol runs — the
shape #604 found at 1.47 bytes/token), each prefixed by a short varying string (a **cache-buster**, not a nonce — it is not secret and authenticates nothing):

| request | wall | `prompt_tokens` | `cached_tokens` | uncached | uncached tok/s |
| --- | --- | --- | --- | --- | --- |
| cold, prefix `n1` | 159.3 ms | 810 | 0 | 810 | **5,084.6** |
| cold, prefix `n2` | 164.1 ms | 810 | 0 | 810 | **4,935.0** |
| repeat of prefix `n2` | 38.4 ms | 810 | **809** | 1 | 26.1 |

**Three facts this establishes, each load-bearing for D9.**

1. **A varying prefix defeats the prefix cache.** The two cold samples report
   `cached_tokens: 0` and agree within 3%, and both land inside M1's independently measured
   4,039–6,660 tok/s band. So a ~1 KiB probe reproduces the throughput a 64 KiB document
   will see, at 1/64th of the cost.
2. **The repeat is catchable, and catching it matters.** M1's caveat 1 says prefix caching
   makes repeated identical documents read optimistically; row 3 is that caveat as a number.
   A naive `prompt_tokens / elapsed` on it reads **21,094 tok/s** — a **4× over-estimate**,
   which derives a timeout 4× too short and so converts real adjudications into fail-open
   timeouts. `usage.prompt_tokens_details.cached_tokens` makes it **detectable rather than
   assumed**, which is why D9 measures over *uncached* tokens and gates on their count.
3. **The probe material is representative.** 1024 dense bytes tokenised to 810 tokens —
   **1.26 bytes/token**, close to #604's 1.47 on real adversarial text and nowhere near
   M1's 6.5 on prose. A probe made of ordinary prose would over-estimate throughput per
   *byte* by ~5× and reintroduce exactly the error D2 made.

**Endpoint shape, checked rather than assumed** (same host, same session): `/props` carries
the per-request context at **`default_generation_settings.n_ctx`** (131072) and there is
**no top-level `n_ctx`** — `total_slots` is 4 while each slot reports the full 131072, so on
this build `-c` is per-slot and the nested field is the number a request is compared
against. `usage.prompt_tokens` and `usage.prompt_tokens_details.cached_tokens` are both
served by the OpenAI-compat endpoint.

---

### D8 — The HTTP 400 door: fail **open** at runtime, refuse at **boot**

#604: `SCAN_BYTE_CAP` bounds bytes and nothing bounds tokens, the ratio is attacker-chosen,
and a 64 KiB document measured **44,437 tokens** against a 32,768-token server — HTTP 400.
The same 400 arrives at the chokepoint on a real dispatch, so this slice must say which way
it fails.

**Runtime: fail open, audited.** `Err(..) -> Allow` with `Unadjudicated::RouterError`,
exactly as D4 already draws it. Escalate-up-only is the tier's entire safety argument: every
failure mode is at worst today's catalogue-only behaviour. Fail-closed was rejected because
the attacker who can force the 400 can force it on *any* document by padding it — that is a
denial of service on the whole tool path, reachable by anyone who can serve the agent a web
page.

**Boot: refuse.** Fail-open-at-runtime is only defensible if the 400 is *rare*, and on a
correctly deployed host it should be impossible. So building the tier reads `/props` and
refuses to boot unless the server's per-request context can hold a worst-case document:

```
REQUIRED_GUARD_N_CTX = SCAN_BYTE_CAP + GUARD_PROMPT_OVERHEAD_TOKENS
                     = 65_536       + 512                          = 66_048
```

**Why `SCAN_BYTE_CAP` bytes is the token worst case, and not a guess.** Shieldstral's
tokeniser is byte-level BPE: its base vocabulary contains the individual bytes, so no input
can ever produce *more* than one token per byte. 1 token/byte is therefore the adversarial
ceiling, not an estimate — an attacker choosing maximally unmergeable bytes converges on it.
#604 measured 1.47 bytes/token on real jailbreak text and M2 measured 1.26 on synthetic
dense text, so the bound is close enough to be worth respecting and provably cannot be
exceeded.

The 512-token overhead covers the tuned policy prompt and the chat template. It is a
constant with a comment, not a measurement, and it is deliberately generous: being wrong
here costs a boot refusal on a marginally-sized server, which is loud, whereas being wrong
in the other direction costs a runtime fail-open, which is silent.

**This converts an attacker-reachable runtime fail-open into an operator-fixable boot
refusal.** The DGX passes today (131072 ≥ 66048); the `-c 32768` server that produced #604
would refuse with a message naming the flag to change. That is D6's argument applied to a
second way the tier can be silently useless.

**Refusal doors, each named** (the [`weights_pin`] shape, for the same reason: they call for
different actions):

| door | meaning | operator action |
| --- | --- | --- |
| `props-unreachable` | `/props` could not be fetched or parsed | start the guard backend |
| `no-context-size` | neither `default_generation_settings.n_ctx` nor a top-level `n_ctx` | upgrade llama.cpp, or use a backend that reports it |
| `context-too-small` | reported context < `REQUIRED_GUARD_N_CTX` | restart `llama-server` with `-c 66048` or higher |

**A note on the fallback.** `default_generation_settings.n_ctx` is read first because M2
confirmed it is the per-request number on the build both hosts run; a top-level `n_ctx` is
accepted as a fallback for other builds. Where *neither* is present the tier refuses rather
than assuming a size — an assumption here fails open at runtime, which is the thing D8
exists to prevent.

**Not addressed by this slice, deliberately:** #604's option 2 (cap by tokens) still wants a
core-side tokeniser the guard seam does not have, and option 3 (chunk and combine) changes
what a score means and needs its own measurement. D8 makes the 400 unreachable on a
correctly sized host; it does not make it unrepresentable.

### D9 — The guard timeout is **probed at boot**, not assumed

D2 derived 15 s from one host and one token count, and measurement 3 broke both halves: the
Mac takes **~5.5 minutes** on a document D2's arithmetic budgets at 15 s. A constant cannot
be right for hosts that differ by more than an order of magnitude, and the failure is
one-directional and silent — too short a timeout does not error, it *fails open*.

**`KASTELLAN_LLM_GUARD_TIMEOUT_MS`, when set, is an operator override and no probe runs.**
Explicit beats measured; it keeps the value pinnable and every timeout test deterministic.

**When unset, the probe runs at boot**, after the D8 checks, against the same endpoint.
**Amended by D11 (below) to take `PROBE_SAMPLES` samples rather than one**; the steps
below describe one sample, and D11 says how several become one number:

1. Send one adjudication of `PROBE_BYTES` (1024) of committed dense text, prefixed by a
   per-**sample** **cache-buster** (per-*boot* until D11). That prefix is what makes the sample cold (M2, fact 1); the body
   is a constant so the measurement is comparable across boots. It is deliberately **not**
   called a nonce — it is not secret, authenticates nothing, and guards no replay, and naming
   it one both overstates its role and trips CodeQL's `rust/hard-coded-cryptographic-value`
   rule on every caller that passes a literal.
2. Read `usage.prompt_tokens` and `usage.prompt_tokens_details.cached_tokens`, and measure
   wall clock.
3. Compute throughput over **uncached** tokens only:
   `tok_per_s = (prompt_tokens - cached_tokens) / elapsed_s`.
4. Derive, then clamp:

```
worst_case_tokens = REQUIRED_GUARD_N_CTX            (66_048 — the same number D8 pins)
derived_ms        = worst_case_tokens / tok_per_s * 1000 * PROBE_SAFETY_FACTOR
timeout           = clamp(derived_ms, TIMEOUT_FLOOR_MS, TIMEOUT_CEILING_MS)
```

| constant | value | why |
| --- | --- | --- |
| `PROBE_BYTES` | 1024 | M2: 810 uncached tokens, ~160 ms on the DGX, ~8 s on a 100 tok/s host |
| `PROBE_SAFETY_FACTOR` | 2.0 | M1 open risk 3 — GPU contention with the planner, still unmeasured |
| `MIN_UNCACHED_PROBE_TOKENS` | 256 | below this the sample is fixed-overhead noise (M2 row 3 read 1) |
| `PROBE_BUDGET_MS` | 20 000 | bounds what a slow host adds to boot |
| `TIMEOUT_FLOOR_MS` | 15 000 | D2's number. A *shorter* timeout is weaker, so never derive below it |
| `TIMEOUT_CEILING_MS` | 120 000 | past this, stalling a dispatch is worse than degrading to catalogue-only |

**What the two clamps mean, because they are not symmetric.** `ToFloor` is unremarkable — a
fast host derives a small number and gets D2's value anyway. **`ToCeiling` is a finding
about the host** and is reported as one: this machine cannot adjudicate a worst-case
document inside the budget, so large dense documents *will* time out and fail open to
catalogue-only. On the DGX, M2's 5,000 tok/s gives `66048 / 5000 * 1000 * 2 ≈ 26.4 s` —
inside the band, unclamped. On measurement 3's Mac (~135 tok/s implied) it derives ~978 s
and clamps to 120 s, loudly. That is the honest rendering of a fact the Mac already has.

**Probe outcomes are a closed enum, and every one of them is a value rather than an
error** — the probe picks a number, it does not verify a control, so it must never stop a
boot that D8 already let through:

| outcome | timeout | reported |
| --- | --- | --- |
| `Measured { uncached_tokens, elapsed_ms }` | derived + clamped | throughput, derived ms, clamp |
| `TooFewUncachedTokens { .. }` | `TIMEOUT_FLOOR_MS` | why the sample was rejected |
| `NoTokenCount` (backend omits `usage`) | `TIMEOUT_FLOOR_MS` | the backend cannot be probed |
| `Saturated { budget_ms }` (exceeded `PROBE_BUDGET_MS`) | `TIMEOUT_CEILING_MS` | **this host is slow** |
| `Failed { why }` (transport/HTTP) | `TIMEOUT_FLOOR_MS` | the error |

**`Saturated` derives the ceiling, not the floor, and that is the one non-obvious row.** A
probe that overran its budget is not a missing measurement — it is an *upper bound on
throughput*, and the only bound in the table that says the host is slow. Sending it to the
floor would give the slowest hosts the shortest timeout, which is precisely backwards.

**The derivation is a pure function of the outcome.** All the arithmetic, the clamping and
the basis reporting live in `guard_model/timeout.rs` over the enum above, so every row of
both tables is a unit test with no server. The IO half only produces the sample.

### D11 — The probe takes **several samples and keeps the fastest** — amends D9

D9 took one sample. Measured on the DGX on 2026-08-25 (issue
[#624](https://github.com/hherb/kastellan/issues/624)), one sample was not a measurement of
the host. The probe runs ~3 s into daemon startup, while Postgres, 15 workers, the Matrix
channel and the audit mirror are all still coming up, so it measures the host **under
startup contention**:

| boot | `timeout_ms` | `tok_per_s` | `coverage_finding` |
| --- | --- | --- | --- |
| 2026-08-23 17:50 | 21 752 | 6 072.99 | null |
| 2026-08-25 14:54 | **120 000** | **269.60** | *"this host cannot adjudicate a worst-case document…"* |
| 2026-08-25 14:58 | 83 489 | 1 582.21 | null |

Same host, same `llama-server` process, unchanged throughout. Measured directly against the
backend minutes later with `cache_prompt: false`, uncontended: **6 953 / 6 995 / 7 026
tok/s** — tightly reproducible, and *higher* than the best of the three. The host did not
regress; the probe was measuring the boot. Worst error **26x**, and the 269.6 run clamped to
the ceiling and fired a **false** coverage finding — the loudest signal the tier has, spent
on a host that adjudicates a worst-case document in ~19 s.

**The fix is `PROBE_SAMPLES` (3) samples, keeping the FASTEST.** Prompt processing has a
hardware ceiling and no floor: contention, a cold model and a busy daemon can only make an
observation *slower* than the host is capable of, never faster. The maximum is therefore the
best available estimator, and a mean is wrong for a one-sided error (the three real rates
average 2 642 — still 2.6x below the truth; the 2 647 in the unit tests is the mean of the
same three rates expressed at the probe's own 810-token sample size, which is what that test
asserts).

**This moves the derived budget DOWN, toward the fail-open edge, deliberately.** A contended
sample derives a *longer* timeout, which is the safe direction, so correcting it needs an
argument: `PROBE_SAFETY_FACTOR`'s 2x is *already* the designed margin for runtime contention
(M1 open risk 3 — the guard shares the GPU with the planner). Folding startup contention
into the measured rate spends that margin twice and pays for it in a `timeout_basis:
"probed"` that no two boots of one host agree on. The dangerous direction stays guarded
where it always was: an *over*-measured rate can only come from a cache hit, which the
cache-buster and the `cached_tokens` subtraction handle.

**Each sample carries its own cache-buster, and that is load-bearing.** N samples sharing
one buster send N byte-identical prompts. On a backend reporting `cached_tokens` the repeats
collapse to `TooFewUncachedTokens` and the multi-sample probe silently degenerates to a
single-sample one; on a backend that does **not** report it (Ollama's OpenAI front door
omits `usage` entirely) they read as enormous throughputs — and since the fastest sample
wins, the probe would *prefer* the most cache-contaminated reading and derive a timeout
several times too short. That is a fail-open manufactured by the fix. The index leads the
buster so consecutive samples diverge as early as the prompt allows.

| constant | value | why |
| --- | --- | --- |
| `PROBE_SAMPLES` | 3 | two cannot show a spread; each costs real boot time |
| `PROBE_TOTAL_BUDGET_MS` | 20 000 (= `PROBE_BUDGET_MS`) | a healthy boot pays nothing extra: 3 x 160 ms on the DGX is ~42x of headroom, 3 x ~560 ms on the Mac ~12x |

**Stopping rule: one rule, not two — because a second would be dead code.**
`taken < PROBE_SAMPLES && elapsed < PROBE_TOTAL_BUDGET_MS`, checked before each sample. An
explicit "stop as soon as a sample saturates" was written and dropped: `Saturated` is
produced only when the per-request budget expired, and `PROBE_TOTAL_BUDGET_MS` *equals*
`PROBE_BUDGET_MS`, so a saturating sample always leaves `elapsed >= PROBE_TOTAL_BUDGET_MS`
and the elapsed check already returns false. The extra clause could never have fired. What
the shipped rule adds over it is the other direction: a sample that came *close* to the
budget without spending it buys one more look.

**So a saturating FIRST sample still ends the probe at one unrepresentative sample, and D11
does not fix that** — issue [#626](https://github.com/hherb/kastellan/issues/626). A cold
`llama-server` paging in its weights derives the ceiling and fires the false coverage finding
on a host that adjudicates a worst-case document in ~19 s. D11 removes the *contention* case
of #624's defect; the *cold-model* case needs a total budget larger than one sample's, which
costs daemon startup on the host that is already sickest and is a decision rather than a
cleanup. It costs exactly one budget today, pinned by an e2e assertion.

*(Until #625's review this paragraph said the explicit rule was **rejected** because it
"would end the probe at one unrepresentative sample and fire the ceiling finding" — which is
what the shipped rule does too. Corrected in place rather than quietly: that claim was the
design record in three other documents.)*

A sample returning just *under* the budget buys one more, so the true bound is
`PROBE_TOTAL_BUDGET_MS + PROBE_BUDGET_MS`. **The FULL overrun** is reachable only on a host
already emitting a coverage finding; a *smaller* one is ordinary and carries no such
reassurance — two 100 ms samples then a saturating third spend 20.2 s on a host whose `best`
is a fast `Measured`, with no clamp and no finding.

**With no measuring sample, the most informative failure wins:** `Saturated` > `Failed` >
`TooFewUncachedTokens` > `NoTokenCount`. The lower three all derive the same floor, so this
ranking decides one thing only — whether a coverage finding fires. `Failed` outranks a thin
sample because a failure means a call to the backend did not complete (a fact about the
*backend*, and evidence for the finding's prediction that every dispatch will fail the same
way) while a thin sample means the call completed and only the *measurement* was unusable.

**The row now carries the spread, and its denominator.** `TimeoutBasis::Probed` gains
`slowest_tok_per_s`, `measured_samples` and `attempted_samples` beside `tok_per_s`, and
`Saturated`/`Unprobed` gain `attempted_samples` too — on those it is the strength of the
evidence behind the finding, since one failed call predicts a wholly fail-open tier far more
weakly than three. Without the denominator `measured_samples: 1` reads three ways at once
(one sample that worked / three with two served from cache / three with two failed calls),
and `PROBE_SAMPLES` is not recoverable from the row at all;
`attempted_samples > measured_samples` with no `coverage_finding` is the query that says
"read that boot's `warn!` lines", and since #625's review every non-measuring sample writes
one. `TimeoutBasis::Probed` also gains, so `policy / guard_tier.boot` distinguishes a quiet
host from a busy one from a single row — which is what #624 needed three boots and a direct
backend measurement to establish. `slowest_tok_per_s < tok_per_s / 2` is the query for "this
host was contended when it measured itself". It is deliberately **not** a coverage finding:
a busy boot with a good fastest sample is not a reduction in coverage, and #624's own
complaint is that the finding channel's credibility gets spent on noise.

**D11 does not close #612, and the two must not be merged.** #624 is that the *sample* was
taken under load on any host; #612 is that extrapolating from a ~1 KiB sample is non-linear
on Metal whatever the load (a quiet Mac still reads 1 137 tok/s at 1 KiB and 260 at 64 KiB).
Both make `probed` mean less than it looks like it means, and both point at the same eventual
remedy — measure from the `ms` / `body_byte_len` the guard rows carry since #616.

### D10 — The tier is advisory defence-in-depth, not a gate

D1 says "this slice ships a tier nobody should turn on yet" because measurement 3 was owed.
Measurement 3 is now done, and the honest replacement is **not** "now it is ready" — it is a
narrower claim, and the numbers must travel with it:

- **65% recall at an FP-0 threshold** (36 of 55 attacks caught at τ). Not a gate.
- **Weakest exactly where the tier's rationale is strongest.** TakSec's bare imperative
  payloads: 6/6 caught, median 0.9955. The greshake scenarios — the same intent wrapped in a
  plausible document — median 0.0797, 5/8 missed, with the canonical indirect-injection case
  at 0.0082. A web-fetching agent meets narrative framing, not imperatives.
- **τ is pinned by roughly four documents** with 1.0–1.2 points of headroom, all of them
  security prose that quotes payloads verbatim. A thin basis, and a fragile one.
- **`best_tau` returns NONE** — the classes overlap at every threshold on real captured
  content.
- **Truncation can cost the whole signal** — a 1.8 MB payload truncated to 64 KiB scored
  0.0102 against its family's median of 0.9937.

**The operational consequence, which is the part that binds: nothing downstream may relax on
this tier.** No catalogue weight is lowered because the model is watching, no allowlist is
widened, no sandbox constraint is loosened. The tier may only ever turn an `Allow` into a
`Block` (D4), and every number above is a reason to keep it that way.

This is also why D5's per-dispatch `p` matters more than it looks: production becomes the
score source for a corpus that does not have to be catalogue-selected, which is the only
route out of the thin basis above.

---

## Testing

Two layers, because a pure function agreeing with itself proves nothing about what the
chokepoint actually does with it.

**Layer 1 — pure, in-crate, no server, no Postgres.** `resolve` over every `GuardOutcome`
door; the `consults_model` predicate at and either side of `BLOCK_THRESHOLD`; τ validation
across `{negative, 0.0, tiny, 1.0, just over 1.0, NaN, inf}`; the tri-state config builder
over all four (guard-set × τ-set) combinations; `guard_timeout` parsing including the
non-numeric refusal and the default.

**Layer 1 also covers D8 and D9 in full, because both are pure over a value the IO half
produces.** For D8: `n_ctx_from_props` over the nested field, the top-level fallback, a
missing field, and every non-numeric shape; `context_verdict` at `REQUIRED_GUARD_N_CTX - 1`,
exactly at it, and above it, plus one test pinning that the required figure is
`SCAN_BYTE_CAP + GUARD_PROMPT_OVERHEAD_TOKENS` rather than a copied literal — so raising the
cap cannot silently leave the check behind. For D9: `derive_guard_timeout` over **every row
of both tables**, with the DGX's own M2 numbers as a fixture (5,000 tok/s → ~26.4 s,
unclamped) and measurement 3's Mac (~135 tok/s → clamps to the ceiling); the two clamp
directions asserted by *basis*, not just by value, since `ToFloor` and a coincidentally-equal
derivation are different facts; and `Saturated` asserted to reach the **ceiling**, which is
the row a plausible implementation gets backwards.

**The accept path must be reachable at layer 1 — the #598 rule.** `context_verdict` takes
`required` as a parameter for exactly the reason `hash_matches` does: with
`REQUIRED_GUARD_N_CTX` hard-wired, an implementation that refused unconditionally would pass
every test, because no cheap fixture can be a 66,048-token server.
[[unreachable-success-path-proves-nothing]]

**Layer 2 — the chokepoint, in a new `core/tests/guard_tier_e2e.rs`.** Real
`dispatch_with_sink` against a real worker and real Postgres — the shape
`injection_guard_e2e.rs` already uses, in a sibling file because that one is 383 lines —
with the guard pointed at a **mock HTTP server**. It covers all four doors end to end, the
`tier: "guard_model"` field on the block row, the `guard` sub-object on the tool row for a
*cleared* document (D5's whole point), and the one assertion that cannot be made at layer 1:
**a catalogue Block leaves the mock with zero requests received.**

The mock returns what it was sent. Slice 1's second review found `guard_model_e2e`'s mock
read only far enough to find `Content-Length` and then discarded the body, which left two
tier-killing mutations green — *a mock that does not return what it was sent tests only
your own canned response.*

**Mutation targets, named in advance** so the gate is not self-graded, each with the layer
that must kill it: invert the `>= BLOCK_THRESHOLD` short-circuit (layer 1 *and* layer 2 —
the request-count assertion is what makes layer 2 able to); map `Unmeasured` to `Clear`
(both); drop the `tier` field (layer 2); drop the τ upper-bound check (layer 1); let
`for_guard` inherit `timeout` again (layer 1).

**Five more from D8 and D9, all layer 1:** flip `context_verdict`'s comparison from `<` to
`<=` (the exactly-at-required case must PASS); make `REQUIRED_GUARD_N_CTX` a literal instead
of `SCAN_BYTE_CAP + GUARD_PROMPT_OVERHEAD_TOKENS`; drop the `cached_tokens` subtraction in
the throughput computation (M2 row 3 is the fixture that kills it — 4× too fast); send
`Saturated` to the floor instead of the ceiling; and drop the `MIN_UNCACHED_PROBE_TOKENS`
gate. Each is a fail-open in the direction the tier cannot afford, and each is killable
without a server. Any mutation only one layer can kill gets
that stated rather than glossed — #587's handover entry overstated exactly this and had to
be corrected.

## What this slice deliberately excludes

- **Measurement 3.** Still owed, and D1 is written so this slice cannot pretend otherwise.
- **The other four `screen` call sites** — `fetch_screen`, `inner_loop/summary`,
  `channel/ingest`, `recall_assembly/pg_builder` keep catalogue-only behaviour. Slice 1
  chose `post_process::finalize` as the only site that is async *and* holds an `AuditSink`;
  widening is a separate slice with its own blast radius.
- **A guard-specific byte cap** smaller than `SCAN_BYTE_CAP`. Tempting after M1 — it would
  cut the worst case directly — but a model cap below the catalogue's is a fail-open surface
  of exactly the shape [#587](https://github.com/hherb/kastellan/pull/587)'s review just
  punished: the gate fires on the whole body while only a prefix is judged. If the 3.5 s
  proves unaffordable, the correct lever is `SCAN_BYTE_CAP` itself, which moves both tiers
  together and so creates no asymmetry.
- **Per-tool guard profiles.** The catalogue varies by `GuardProfile::for_tool`; the tier
  does not. Uniform is the narrower claim.
- **Any catalogue reweighting.** Unchanged from slice 1: separate, riskier, must not ride
  along.

## Open risks

1. **τ is unfitted, and this slice makes a tier that runs on one.** D1 contains it by
   refusing to supply the number, but an operator can still supply a bad one. Mitigated only
   by the range check and by measurement 3 remaining visibly owed.
2. ~~**3.5 s per document at the cap is a real cost.**~~ **ACCEPTED by the operator
   2026-08-22.** A task making ten large-document tool calls adds ~35 s. Recorded rather
   than deleted, because the number still belongs in a deploying user's decision — and
   because the acceptance is grounded in what the tier is *for*: the target is
   **high-risk environments such as healthcare**, where 3.5 s of screening against a
   document the agent is about to act on is cheap relative to the failure it prevents.
3. ~~**The planner and the guard share one GPU.**~~ **ACCEPTED by the operator
   2026-08-22.** Contention makes both slower by an unmeasured amount, and kastellan will
   support multi-tenancy, at which point the hardware investment is the deploying user's to
   make. Still the most likely reason a future session revisits D2's 4× headroom — accepted
   is not the same as measured.
4. **`p` recorded per dispatch is a new, long-lived column of model output.** It is a float
   and carries no document content, but it is a behavioural fingerprint of what the agent
   read, retained for as long as `audit_log` rows are.
5. ~~**M1 is one host.**~~ **Answered, badly, by measurement 3.** The Mac leg is measured
   now and it is ~40x slower on the same document (~5.5 min against the DGX's 3.5 s). D9
   turns that from an unstated assumption into a boot-time measurement plus a loud clamp
   report, which is the best available answer -- but see risk 6.
6. **On a host that clamps to the ceiling, the tier is off for large documents and only an
   audit query says so.** D9 reports it at boot and every timeout is an audited
   `Unadjudicated::RouterError`, so it is countable rather than invisible. It is still a
   real reduction in coverage on exactly the documents most worth screening, and the honest
   framing is D10's: advisory defence-in-depth. The lever, if it ever matters, is
   `SCAN_BYTE_CAP` -- which moves both tiers together and so creates no asymmetry (see the
   exclusions).
7. **The boot probe costs one adjudication on every daemon start** with the tier configured,
   bounded by `PROBE_BUDGET_MS`. On the DGX that is ~160 ms; on a slow host it is up to
   20 s of boot latency an operator did not previously pay. Bounded, reported, and skipped
   entirely when `KASTELLAN_LLM_GUARD_TIMEOUT_MS` is set.
8. **D8's 512-token prompt overhead is a constant, not a measurement.** If the tuned policy
   prompt ever grows past it, `REQUIRED_GUARD_N_CTX` under-states what a worst-case document
   needs and the 400 becomes reachable again on a server sized exactly to the check. The
   overhead is generous relative to today's prompt, and the failure is bounded by D8's
   runtime half.
