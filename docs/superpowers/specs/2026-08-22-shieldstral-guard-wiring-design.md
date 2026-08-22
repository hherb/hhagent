# Shieldstral guard tier — the wiring slice

**Status:** design, 2026-08-22
**Predecessor:** [`2026-08-21-shieldstral-guard-slice-1-design.md`](2026-08-21-shieldstral-guard-slice-1-design.md)
(merged as [#585](https://github.com/hherb/kastellan/pull/585), `f90631da`)
**Closes:** [#586](https://github.com/hherb/kastellan/issues/586)

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

### D2 — The guard timeout is derived from M1, not chosen

`RouterConfig` gains `guard_timeout: Duration`, read from `KASTELLAN_LLM_GUARD_TIMEOUT_MS`,
default **15 000 ms**. `for_guard` sets the returned config's `timeout` from it instead of
inheriting `timeout` through the `..self.clone()`.

**The derivation, so it can be re-derived rather than re-guessed:** M1's measured maximum at
`SCAN_BYTE_CAP` is 3,586 ms. 15 s is ~4× that. It covers a host roughly four times slower
than the GB10 at a full 64 KiB document, plus the GPU-contention headroom Open risk 3 says
is unmeasured.

**It deliberately does not cover a CPU-only host** (~50 s by M1's linearity). There, the
right behaviour is to time out and fail open to catalogue-only screening — which is
today's behaviour — rather than stall every single dispatch for the better part of a minute.
A timeout that accommodated the slowest conceivable host would defeat the purpose of having
one.

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

## Testing

Two layers, because a pure function agreeing with itself proves nothing about what the
chokepoint actually does with it.

**Layer 1 — pure, in-crate, no server, no Postgres.** `resolve` over every `GuardOutcome`
door; the `consults_model` predicate at and either side of `BLOCK_THRESHOLD`; τ validation
across `{negative, 0.0, tiny, 1.0, just over 1.0, NaN, inf}`; the tri-state config builder
over all four (guard-set × τ-set) combinations; `guard_timeout` parsing including the
non-numeric refusal and the default.

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
`for_guard` inherit `timeout` again (layer 1). Any mutation only one layer can kill gets
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
2. **3.5 s per document at the cap is a real cost on the dispatcher path, and it is
   serial.** A task making ten large-document tool calls adds ~35 s. The tier is opt-in, so
   nobody pays this without asking for it, but the number belongs in the operator's decision
   and not only in this spec.
3. **The planner and the guard share one GPU on a single-host deployment, and M1 measured an
   idle one.** Contention makes both slower by an unmeasured amount. This is the strongest
   argument for D2's 4× headroom and the most likely reason a future session revisits it.
4. **`p` recorded per dispatch is a new, long-lived column of model output.** It is a float
   and carries no document content, but it is a behavioural fingerprint of what the agent
   read, retained for as long as `audit_log` rows are.
5. **M1 is one host.** The Mac leg is unmeasured. Linearity (M1) makes it predictable from
   that host's prompt-eval throughput, but predicted is not measured.
