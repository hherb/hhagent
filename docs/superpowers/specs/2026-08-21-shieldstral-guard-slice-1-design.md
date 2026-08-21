# The Shieldstral adjudicator — guard-model slice 1

Base: `main` @ `bb937df7`. Predecessor: the feasibility study
[`2026-08-13-shieldstral-guard-model-feasibility-study.md`](2026-08-13-shieldstral-guard-model-feasibility-study.md),
whose measurements 1 (Q4 + Q8), 2, 4 and 5 are done and on `main` (`07b6451e`).

## Why this exists

`core/src/cassandra/injection_guard.rs` is a 22-entry English **substring** catalogue.
Its own module doc enumerates what it cannot see: narrow visible whitespace is not
folded, leetspeak is not folded, non-English equivalents are absent. A catalogue miss
is an `Allow` under both profiles, silently.

The study measured Shieldstral against exactly those surfaces and it flagged all of
them (leetspeak 0.9036, narrow whitespace 0.9945, German 0.9972, indirect injection
0.9998), on SHA-256-identical weights across both hosts with a maximum per-case
divergence of 0.0044. The code prerequisite is already merged: `llm-router` carries
`logprobs`/`top_logprobs` and the pure `logprob_score` scorer.

What is missing is the adjudicator itself, somewhere to point it, and a way to fit its
threshold. That is this slice.

## Two findings that reshaped the design

Both were found while reading the code against the study, and both contradict the
study's written plan. They are recorded here because the study is otherwise the
authority a later session will reach for.

### F1 — the study's `0.45–0.70` band is very nearly empty

The proposed shape was *"catalogue 0.45–0.70 → Shieldstral adjudicates"*. That band is
almost unreachable. Catalogue weights are only `{0.40, 0.50, 0.75}` and `screen` sums
them with a cap at 1.0, so **any two rules firing already totals ≥ 0.80**. Enumerating
every subset gives a reachable score set of:

```
{0.0, 0.40, 0.50, 0.75, 0.80, 0.90, 1.0}
```

Intersected with `[0.45, 0.70)` that is **exactly one value, 0.50** — reachable only by
`"leak the api key"` or `"open a reverse shell"` firing alone with nothing else. Two
patterns out of twenty-two. Built as written, the tier would sit on the hot path and
essentially never fire.

The value the study argues for lives in the **catalogue miss**, which scores 0.0 and is
`Allow` today. `RELAXED_CHAT_TEMPLATE_WEIGHT` is 0.40 and so adds nothing to the band
either.

> A test pins this enumeration (`reachable_catalogue_scores_are_exactly_seven_values`)
> so that a future reweighting which *would* populate a band fails loudly here rather
> than silently invalidating D4's reasoning. Note the band is not *empty* — 0.50 is in
> it — so a test asserting emptiness would be false; what it asserts is the reachable
> set.

### F2 — `observation replay` is a plan-level tool and cannot score this tier

The study names `kastellan-cli observation replay` as measurement 3's vehicle
("extend it to score a candidate stage"). It cannot be, and the mismatch is structural,
not a missing feature. `replay_capture(capture, chain)` walks `CaptureJson.plans` and
runs each through `ChainReviewStage`, which reviews **plans** against CASSANDRA
principles. The injection guard adjudicates **document text** — worker output, fetched
pages, email bodies — reached through `extract_scannable_text`.

The seven fixtures in `tests/observation/captures/` contain prompts, plans and audit
rows. They contain no screened documents at all. Extending that subcommand would mean
two schemas and two code paths behind one name; slice 1 builds a separate vehicle
instead (D5) and leaves `observation replay` untouched.

## What slice 1 delivers

1. `guard_url` / `guard_model` on `RouterConfig` — the endpoint seam (D1).
2. `core/src/cassandra/guard_model/` — the adjudicator: a pure prompt artefact, a pure
   decision function, and a thin async shell (D2, D3, D6).
3. `core/src/guard_calibration/` + `kastellan-cli guard calibrate` — the scoring
   vehicle and its report (D5, D7, D8).
4. `tests/guard/corpus/` — a seeded proof-of-concept corpus (D9).

**Deliberately NOT here: the production wiring.** The chokepoint is untouched, so this
slice cannot regress the daemon and needs no live gate to be safe. The band decision
(D4) is *designed* here and *implemented* in the follow-up, once a threshold exists to
implement it with.

## Design decisions

### D1 — The guard endpoint is its own config and never falls back to the planner's

`RouterConfig` gains `guard_url: Option<String>` and `guard_model: Option<String>`,
from `KASTELLAN_LLM_GUARD_URL` and `KASTELLAN_LLM_GUARD_MODEL`. Both default to `None`,
mirroring the existing `frontier_url`/`frontier_model` pair.

**Unconfigured must not fall back to `local_url`.** That endpoint serves the planner
model, which would answer the `<Instruct>`/`<Query>` prompt with fluent prose rather
than a calibrated `yes`/`no` logit pair — producing a number that looks exactly like a
score and means nothing. Unconfigured therefore yields `Unmeasured` (D2), never a
probability.

The study's original deferral of this seam ("needed only while oMLX lacks logprobs") no
longer holds: llama.cpp is pinned on both hosts, so the guard never shares the planner's
endpoint and the seam lands here rather than being avoided.

**Consequence for the wiring slice:** an enabled-but-unconfigured tier fails open on
every call. That must be reported **once at boot**, loudly, not per-call — a per-call
warning on the dispatcher hot path is its own denial of service.

### D2 — The adjudication is three-valued, and `Unmeasured` is not a score

```rust
pub enum GuardAdjudication { Flagged, Clear, Unmeasured }
```

`binary_token_probability` already returns `Option<f32>` and yields `None` unless *both*
verdict spellings are observed. Collapsing that `None` into a number is precisely the
fail-open defect the study made unrepresentable: a sentinel floor renormalises to
exactly 0.5 with neither spelling present — reading as "below τ", i.e. safe — and to
0.9999 with only one.

So `None` propagates as a distinct state all the way out. Anything that wants to treat
it as "allow" must say so at the point where that is a reviewable security decision.

### D3 — Fail-open is a property of the wiring, not of the adjudicator

`adjudicate` reports; it never decides to allow. It returns `GuardAdjudication` or a
`RouterError`, and has no opinion about what a caller does with either.

This is deliberate scope discipline: the study requires fail-open on router error
(the sandbox and the egress allowlist are the boundary, not this), and that posture
should be legible at **one** site in the next slice rather than distributed between a
scorer, an adjudicator and a dispatcher.

### D4 — The tier fires on everything the catalogue did not Block

Given F1, the study's band is replaced by:

```
catalogue >= BLOCK_THRESHOLD  ->  Block, model NOT consulted
catalogue <  BLOCK_THRESHOLD  ->  adjudicate
                                    Flagged     -> Block
                                    Clear       -> Allow
                                    Unmeasured  -> Allow, audited   (wiring slice)
```

**Escalate-up only**: the model can turn an `Allow` into a `Block` and never the
reverse, so a guard-model failure can only ever be as permissive as today's behaviour,
never more. Not consulting the model above the threshold is not an optimisation for its
own sake — there is no verdict it could return that would change the outcome, so the
call would be pure latency.

**The cost this buys, stated plainly:** an LLM call on nearly every worker output,
where today there is none. That makes latency at realistic document sizes a gating
number for the wiring slice rather than a nice-to-have (see Open risks).

**Which call site the wiring targets, so it need not be re-derived.** `screen` and
`screen_with_profile` are pure and sync with five callers — `tool_host/post_process`,
`tool_dispatch/fetch_screen`, `inner_loop/summary`, `channel/ingest` and
`recall_assembly/pg_builder`. Only `post_process::finalize` is async *and* holds an
`AuditSink`, so it is the one site that can await an adjudication and record it. The
shared pure functions keep their shape; adjudication is added around that one call,
not inside them. The other four keep catalogue-only behaviour in the first wiring
slice — a narrower blast radius, and a deliberate limitation to state rather than to
discover.

### D5 — A separate calibration vehicle, not an extension of `observation replay`

Given F2. `core/src/guard_calibration/{corpus,report}.rs` holds the logic;
`kastellan-cli guard calibrate --corpus DIR` is a thin shell over it. This is the same
lib-plus-thin-CLI split `observation::replay` + `observation_replay.rs` already uses, so
the report formatter is a pure function testable without a model or a network.

Scoring runs through **the shipping Rust adjudicator**, not a parallel implementation.
That is the study's own lesson applied: the Python probe had four fail-open defects that
the Rust port made unrepresentable, so a τ fitted in Python would be fitted against
code that does not run in production.

### D6 — The prompt is a tuned artefact, pinned by a cross-implementation digest

`INSTRUCT` and `QUERY` are consts, copied byte-identically from
`scripts/eval/shieldstral_logprobs_probe.py`. `POLICY_DIGEST` uses that harness's
algorithm — `sha256(INSTRUCT + "\0" + QUERY)`, first 16 hex — so a test asserts the
Rust digest **equals the recorded `342e3d9661b2cbe2`**.

This is stronger than a self-consistent checksum. A self-consistent one catches a
reword only if someone remembers to update it; pinning to the *Python* constant proves
the port did not quietly reword the artefact in transit. The ablation is why it matters:
identical weights and identical documents, changing only this block, moved a textbook
indirect prompt injection from 0.9998 to 0.0038 — confidently safe. Same class as
[[plan-text-is-a-defect-source]].

Changing the prompt on purpose means: update both files, update the digest, and re-run
the corpus.

### D7 — Catalogue scores are computed at calibrate time, never stored

A corpus case records `{id, label, text, provenance, notes}` and **not** its catalogue
score. The score is computed from the shipping `screen()` when `guard calibrate` runs,
so it cannot drift from the catalogue it describes. A stored score would silently become
a lie the first time a rule weight changed — and the whole point of D4's split is that
the catalogue score decides whether the model is consulted at all.

### D8 — The report refuses to pool provenances, and refuses to score unmeasured cases

`guard calibrate` prints, over the cases the tier would actually see (catalogue below
the block threshold, with the excluded count named):

- the confusion matrix at τ, and the score distribution
- the τ that maximises the margin (min attack score − max benign score), and that margin
- the **`Unmeasured` count, which must be 0 for the run to be valid** — an unmeasured
  case is not a pass, and a run containing any is reported as invalid rather than as a
  slightly smaller sample
- the matrix **split by `provenance`**

The provenance split is the load-bearing one. A corpus written by whoever builds the
adjudicator tests what that person thought of — the study's own "a mutation score is
only as good as the mutation set", one level up. Pooling hand-written cases with
captured ones lets a strong score on the former hide a weak score on the latter, which
is the only half that is evidence about production.

### D9 — The seeded corpus is a proof of concept and does NOT discharge measurement 3

Recorded loudly because the artefacts will read as calibration to a later session: this
slice ships a seeded corpus of hand-written cases across the catalogue's four documented
evasion surfaces plus benign controls, and `guard calibrate` will print a confusion
matrix, a margin and a suggested τ over it.

**None of that is a fitted threshold.** Measurement 3 wants ≥ 100 labelled cases whose
captured half comes from real worker output. Until that exists:

- any τ this produces is **provisional** and must not be promoted to a default;
- the numbers are evidence that the *vehicle* works, not that the *guard* is calibrated;
- the wiring slice must not cite them as its gate.

The `provenance` field and D8's split exist so this distinction survives in the tooling
rather than only in this paragraph.

## Control flow

```
worker output (JSON)
  │
  ├─ extract_scannable_text(v, SCAN_BYTE_CAP)      existing, pure, 64 KiB cap
  │
  ├─ screen_with_profile(text, profile)            existing, pure, unchanged
  │     │
  │     ├─ score >= BLOCK_THRESHOLD ──────────────► Block          (model not consulted)
  │     │
  │     └─ score <  BLOCK_THRESHOLD
  │           │
  │           └─ guard_model::adjudicate(&router, &cfg, text)      [WIRING SLICE]
  │                 │
  │                 ├─ build_messages(text)         pure  (policy.rs)
  │                 ├─ Router::send(..with_logprobs(20))
  │                 ├─ first_position_alternatives + binary_token_probability
  │                 └─ decide(p, tau)               pure  (decide.rs)
  │                       │
  │                       ├─ Flagged    ──────────► Block
  │                       ├─ Clear      ──────────► Allow
  │                       └─ Unmeasured ──────────► Allow, audited
```

Everything left of the `[WIRING SLICE]` marker exists today and is unchanged by this
slice. Everything right of it is built here but called only from `guard calibrate` and
from tests.

`InjectionDecision` gains **no `Review` variant**. The band lives at the async call
site, so the pure catalogue keeps its binary contract and its five existing callers are
untouched. The `#[non_exhaustive]` attribute stays as-is for a future need.

## Module layout

```
llm-router/src/config.rs                     + guard_url, guard_model
core/src/cassandra/guard_model/mod.rs        async shell + public API
core/src/cassandra/guard_model/policy.rs     INSTRUCT/QUERY/POLICY_DIGEST/build_messages
core/src/cassandra/guard_model/decide.rs     GuardAdjudication + decide()
core/src/cassandra/guard_model/tests.rs      unit tests
core/src/guard_calibration/mod.rs            public API
core/src/guard_calibration/corpus.rs         CorpusCase + loader
core/src/guard_calibration/report.rs         confusion matrix, tau sweep — all pure
core/src/bin/kastellan-cli/guard_calibrate.rs  arg parsing + dispatch only
tests/guard/corpus/*.json                    the seeded corpus
```

Every file is intended to stay well under the 500-line cap; the three-way split of
`guard_model` is what keeps the pure halves separable from the async shell rather than a
size workaround.

## Testing

Pure-first, TDD. Nothing below needs a model or a network except the last item.

**`policy.rs`** — the rendered message shape (`<Instruct>` / `<Query>` / `<Document>`
framing, system + user roles); the digest pin against `342e3d9661b2cbe2`; a document
containing the framing markers does not break the envelope.

**`decide.rs`** — table-driven over `(p, tau)`: below, above, and **exactly at** τ;
`None` yields `Unmeasured` and never `Clear`. A mutation flipping the comparison must
fail a test.

**`injection_guard`** — the F1 enumeration: no reachable catalogue score lands in
`[0.45, 0.70)` except 0.50, and the reachable set is exactly the seven values above.

**`corpus.rs`** — malformed JSON, unknown `label`, missing `provenance`, empty
directory, and a non-UTF-8 file all produce a named error rather than a skipped case. A
silently skipped case would shrink the denominator of a confusion matrix.

**`report.rs`** — a hand-built `Vec` of scored results renders the expected matrix;
τ-sweep picks the margin-maximising threshold; an `Unmeasured` case marks the run
invalid; the provenance split does not pool.

**`guard_model/mod.rs`** — the async shell against the hand-rolled `TcpListener` stub
that `llm-router/tests/local_backend_e2e.rs` already establishes: happy path, HTTP 500,
malformed body, and a response whose alternatives contain neither verdict spelling
(⇒ `Unmeasured`). No new dev-dependency.

**Live, `#[ignore]`** — against a real llama.cpp Shieldstral, following the existing
`web_search_e2e::real_search_against_searxng` pattern. This one also carries a **size
sweep** (1 KiB / 8 KiB / 64 KiB documents), so the latency number the wiring slice needs
exists before that slice commits to D4.

## What this slice deliberately excludes

- **The production wiring.** Needs a threshold, and there is none yet (D9).
- **`InjectionDecision::Review`.** Unnecessary once the band lives at the call site.
- **Any catalogue reweighting.** F1 makes it tempting; it is a separate, riskier change
  to a deterministic guard with test-pinned behaviour and a per-class Block-capability
  invariant, and it must not ride along.
- **The Constitutional-Guard second opinion** (study §2) and the Stage 1 hook (§3).
- **A ≥ 100-case corpus.** D9.

## Open risks

1. **Latency at realistic document sizes is unmeasured.** Measurement 1's p50 of
   30–43 ms was taken on ~26-token strings. `SCAN_BYTE_CAP` is 64 KiB — roughly 16k
   tokens — and D4 puts the model on nearly every worker output. Prompt processing, not
   the single decode token, will dominate. The `#[ignore]` size sweep exists to produce
   this number; **the wiring slice must not proceed without it.**
2. **The seeded corpus is a self-graded exam.** D9 and D8's provenance split contain it
   but do not remove it.
3. **τ is not fitted, and a provisional τ is the kind of number that gets promoted by
   accident.** D9 is the mitigation; it is a paragraph, not a mechanism.
4. **An enabled-but-unconfigured guard fails open on every call.** D1's boot-time report
   is the mitigation and it lands with the wiring, not here.
5. **A second endpoint is a second thing that can be down.** The guard runs on its own
   llama.cpp server on both hosts. Fail-open means an outage degrades silently to
   today's catalogue-only behaviour — correct, but it means the tier's absence needs to
   be observable, which is an audit question for the wiring slice.
6. **The guard inherits the planner's 180 s request timeout, and that is four orders of
   magnitude past its target latency.** `for_guard` copies `timeout` along with the rest
   of the config, so a configured guard uses `KASTELLAN_LLM_TIMEOUT_MS` — default
   `180_000`. That is right for a 26B planner generating a plan and wrong for a 3B
   classifier decoding one token at a measured p50 of 30–43 ms.

   Risk 5 models the endpoint being **down**, where `connect_timeout` (5 s) applies and
   the failure is fast. This is the endpoint being **up but hung** — accepted connection,
   no response — where nothing fires for three minutes. Under D4 the tier sits on nearly
   every worker output, so that is a per-document stall on the dispatcher path, in the
   same class as the per-call boot warning D1 rules out for being its own denial of
   service.

   **Deliberately not fixed in this slice, because fixing it means inventing a number.**
   Open risk 1 says latency at realistic document sizes is unmeasured; a timeout chosen
   before that measurement would be a guess wearing a constant's clothing, and this slice
   ships no wiring for it to protect. The `for_guard` test that asserts
   `guard.timeout == cfg.timeout` records the inheritance as *observed*, not as endorsed.

   **The wiring slice owes a guard-specific bound** derived from the size sweep — a
   `KASTELLAN_LLM_GUARD_TIMEOUT_MS` or an equivalent clamp in `for_guard`. Tracked as a
   follow-up issue so it is not re-derived from scratch:
   [#586](https://github.com/hherb/kastellan/issues/586).
