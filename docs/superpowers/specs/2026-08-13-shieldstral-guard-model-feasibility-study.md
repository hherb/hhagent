# Shieldstral 1.0 Feasibility Study — model-based guard tier for CASSANDRA

**Date:** 2026-08-13
**Status:** Investigation, no code. Input for the Phase 5 "Model-based CASSANDRA guard tier" ROADMAP item.
**Amended 2026-08-15:** the macOS runtime changed to oMLX, which serves no logprobs; the guard model moves to llama.cpp. See the amendment under [Cross-platform inference](#cross-platform-inference--the-one-thing-that-could-sink-it).
**Measurement 1 RUN 2026-08-15 — PASS.** llama.cpp serves logprobs and multimodal on macOS; 14/14 at τ=0.5, margin +0.796, p50 40 ms. **The go/no-go is cleared.** Biggest finding is not the pass but that the **policy prompt is load-bearing** — the same weights scored 11/14 with a *negative* margin under a naively-worded `<Instruct>`. Harness: `scripts/eval/run-shieldstral-llamacpp.sh`.
**Re-run on Q8_0 2026-08-16 — PASS, and the harness was repaired to make it honest.** The shipping quantisation is now **Q8_0 on llama.cpp on both hosts** (vLLM ruled out with reasons below); 14/14, margin **+0.8151**, p50 43 ms. Two defects in the wrapper were found and fixed on the way, both of the "a check that cannot fail" family — see [Measurement 1, Q8 leg](#measurement-1-q8-leg-2026-08-16). **Measurement 2 (`llm-router` plumbing) is DONE.**
**Question asked:** is Mistral's Shieldstral suitable for kastellan — specifically as a
decision gate for *whether the more expensive CASSANDRA analysis is needed*, decided at
high confidence?

**Short answer:** yes as the guard-model choice, and in preference to the currently
pencilled-in IBM Granite Guardian 4.1 — but the *gate* framing needs splitting in two.
Using the score to **add** review is fail-safe and buildable now. Using it to **skip**
review is fail-open, is the exact posture the ROADMAP already rules out for Guardian, and
is additionally blocked on Stage 3 existing (it does not). Recommendation and the
five measurements that gate it are at the bottom.

---

## Why this document exists

ROADMAP Phase 5 carries **"Model-based CASSANDRA guard tier — IBM Granite Guardian 4.1
(defense-in-depth, advisory only)"**, investigated 2026-06-15. Shieldstral (released
2026-08-04) is a direct competitor for that slot with better published numbers at
3/8ths the size, the same Apache-2.0 licence, and one structural property Guardian lacks:
the safety policy is a plain-language question supplied **at inference time**, so the same
weights serve several unrelated hook points without a fine-tune each. That property is
what makes it worth re-opening a decision that was already made.

## Name disambiguation

- **Shieldstral 1.0 / `Shieldstral-1.0-3B`** — the subject of this document. Open-weights
  Apache-2.0 safety classifier, Mistral, 2026-08-04.
- **`mistral-moderation`** — Mistral's earlier *hosted* moderation endpoint, fixed
  taxonomy, API-only. Not this, and not usable here (egress + no policy control).
- **Guardian** in the ROADMAP means **IBM Granite Guardian 4.1**, not Mistral. Mistral's
  own press describes Shieldstral as a "guardian" model; the two are unrelated.
- Peers in the same class: GPT-OSS-Safeguard-20B, Qwen3Guard-8B, Nemotron-3.5 Content
  Safety 4B, Llama Guard 4 12B, ShieldGemma.

## Capability

| Property | Value |
| --- | --- |
| Parameters | 3B (Ministral-3-3B-Base-2512 backbone + Pixtral vision encoder) |
| Licence | **Apache-2.0**, open weights, commercial use permitted |
| Training | 54.1M contrastive pairs, 12 languages |
| Trained context | 32k tokens |
| Languages | en, fr, es, de, it, pt, nl, zh, ja, ko, ar, ru |
| Modality | text + image |
| Footprint | fits 16 GB VRAM in BF16, single GPU |
| Runtimes | vLLM ≥ 0.26.0 (recommended), SGLang, Transformers, llama.cpp/GGUF |

**How it is called.** The policy is not baked into the weights. The caller supplies an
`<Instruct>` block (context + strictness) and a `<Query>` — a plain yes/no question such as
*"Does this content promote physical violence?"* — plus the content to judge. The model is
invoked with `max_tokens=1`; at the output position it unembeds only toward the `yes` and
`no` token ids and softmax-renormalises them into a **continuous calibrated score**, with a
default threshold τ=0.5. In practice that means one chat completion with
`logprobs=true, top_logprobs=20`, and renormalising the two probabilities client-side.

That is the whole reason this model is interesting for kastellan: a *tunable confidence
number per arbitrary policy question*, from one forward pass, with no retraining. It
collapses prompt classification, response moderation, refusal detection and toxicity
detection into a single problem shape.

**Published benchmarks** (Mistral's own; see caveat below):

| Benchmark | Shieldstral 3B | Reference |
| --- | --- | --- |
| Text safety, avg F1 over 13 benchmarks | **84.9** | GPT-OSS-Safeguard-20B 84.9, Qwen3Guard-8B 84.0, Nemotron-3.5-CS-4B 83.3, Llama Guard 4 12B 69.1 |
| Multimodal safety, avg F1 | 83.8 | best in its evaluated set |
| VLGuard | 97.7 | |
| WildGuardTest (prompt safety) | 88.1 | |
| Policy adaptability | 91.3 | |
| Refusal detection | 91.5 | |

**Limitations Mistral itself states:** reduced reliability on adversarial or obfuscated
inputs (encoded/transliterated text) and on documents well past 32k tokens; multilingual
prompt classification lags on Arabic and Indonesian, and on RTP-LX scores 70.3 against
Nemotron-3.5-Safety-4B's 86.1.

**Caveat that matters more than any number above:** as of publication there was **no
independent third-party benchmark**. Latency, false-positive rate and real-world policy
adaptability are unverified by anyone but the vendor. Every figure in the table is a
vendor claim.

## Licence and hard-constraint fit

| Constraint (`CLAUDE.md`) | Verdict |
| --- | --- |
| AGPL-compatible dependencies only | **PASS.** Apache-2.0 is compatible. No CDDL/BUSL/SSPL/"source-available" clause, no Mistral Non-Production License, no acceptable-use addendum that would bind the project. |
| Linux + macOS first-class | **CONDITIONAL** — see "Cross-platform inference" |
| No NVIDIA / DGX hard dependency | **PASS.** 3B runs on Apple Silicon; llama.cpp/GGUF is a listed runtime. |
| Rust core, Python only inside sandboxed workers | **PASS.** It is reached over HTTP through `kastellan-llm-router`, exactly like the planner. No new language, no new process, no PyO3. |
| Every worker sandboxed before it runs | **N/A.** Not a worker. It is an inference backend behind the existing sole core-side LLM egress. |

## Cross-platform inference — the one thing that could sink it

The scoring mechanism depends on **token logprobs**, and that is where the two legs diverge.

- **Linux/DGX:** vLLM on `http://127.0.0.1:8000/v1` — already the default
  `KASTELLAN_LLM_LOCAL_URL` for Linux (`llm-router/src/config.rs:81`). vLLM has served
  `logprobs`/`top_logprobs` for a long time. No new egress, no new port, no new dependency.
- **macOS:** ~~Ollama on `http://127.0.0.1:11434/v1` — the per-OS default.~~ **Superseded
  2026-08-15 — see the amendment below.** The macOS default is now oMLX on
  `http://127.0.0.1:8000/v1`, which does **not** serve logprobs, so the guard model runs
  on a *second* runtime rather than on the default one.

> ### Amendment, 2026-08-15 — the macOS leg changed runtime
>
> macOS moved to **oMLX** as the default chat + embedding backend (`:8000`), for
> performance on Apple silicon; the switch is unrelated to this study but lands squarely
> on it. Measured against the live oMLX server that day:
>
> - **oMLX does not return token logprobs.** `logprobs`/`top_logprobs` are absent from
>   `/v1/chat/completions` altogether. `top_logprobs` *is* declared on `ResponsesRequest`
>   (`/v1/responses`) — the only mention of logprobs anywhere in the 120 KB OpenAPI
>   document — but it is accepted and ignored: a call with `top_logprobs: 20` **and**
>   `include: ["message.output_text.logprobs"]` returns HTTP 200 with output content
>   carrying only `text` and an empty `annotations`, and **no response schema in the
>   document declares a logprobs field at all**. Declared-but-inert, which is worth
>   re-checking after an oMLX update — it suggests the wiring is partly present.
> - **Shieldstral itself runs fine on oMLX.** `Shieldstral-1.0-3B-MLX-8bit` returned
>   correct bare verdicts through `/v1/chat/completions` — `yes` on a prompt-injection /
>   exfiltration sample, `no` on a benign control — at `max_tokens=4`, `temperature=0`.
>   So the *model* works on the default runtime; only the *score* is unavailable.
>   That is precisely the degradation this section warned about: a hard, unmovable
>   τ=0.5 with no confidence band.
>
> **Resolution: llama.cpp, not Ollama.** `llama-server` is the designated macOS fallback
> runtime for models or capabilities oMLX cannot serve, and is reported to support both
> halves Shieldstral needs — logprobs **and** the multimodal (Pixtral vision) path that
> Ollama would not have given us. It is OpenAI-compatible, so it costs no code, only an
> explicit `KASTELLAN_LLM_LOCAL_URL` (llama.cpp has no conventional port). This keeps the
> cross-platform constraint satisfied — DGX vLLM `:8000` and macOS llama.cpp both serve
> calibrated scores — so the banded design survives intact and there is **no Linux-only
> security behaviour**. The llama.cpp capability claim is research, not yet measured here;
> confirming it is measurement 1 below.
>
> **Two consequences for sequencing.** (a) The Ollama hand-import path is dropped, and
> with it the v0.12.11 floor and the broken-stub-template hazard that a hand-rolled
> Modelfile carries. (b) **Do not build a `guard_url`/`guard_model` seam in `RouterConfig`
> yet.** A second endpoint is needed only for as long as oMLX lacks logprobs; if it gains
> them, the guard model runs on the existing `local_url` and that seam becomes dead code
> in the sole core-side LLM egress. Defer until measurement 1 settles.

**If either fails, the calibrated score is unavailable on macOS** and the model degrades to
a bare `yes`/`no` token — i.e. a hard, unmovable τ=0.5 with no confidence band. That
destroys the *entire* premise of the question asked ("with highest confidence"): a
confidence-banded triage gate cannot be built on a backend that will not return a
confidence. It would also produce a Linux-only security behaviour, which the cross-platform
constraint forbids. **Measure this before writing any code.**

A 16 GB Mac cannot hold Shieldstral in BF16 alongside anything else; Q4_K_M brings it to
roughly 2 GB, which is fine, but quantisation moves the calibration — a threshold tuned on
the DGX's BF16 weights is not transferable to a quantised Mac. Calibrate per deployment,
or pin one quantisation across both hosts.

## Operational fit — the `llm-router` gap

`ChatRequest` (`llm-router/src/messages.rs:75`) carries `model`, `messages`, `max_tokens`,
`temperature`, `chat_template_kwargs`. There is **no `logprobs`/`top_logprobs` field, and no
logprobs on the response side.** Adding them is small and purely additive — the same
`#[serde(skip_serializing_if = "Option::is_none")]` pattern `chat_template_kwargs` already
uses, so a backend that has never heard of them still sees a byte-identical payload — but
it is a change to the sole core-side LLM egress and needs its own pins on both legs.

One trap in the same area: `RouterConfig::disable_thinking` defaults **on** and emits
`chat_template_kwargs: {"enable_thinking": false}` on every local chat completion.
Shieldstral is not a reasoning model and its chat template has no such switch. Verify the
field is either ignored or that the guard call opts out — a 4xx here would surface as the
same misleading `RouterError::Transport` that cost a whole session in the #505 arc.

## Where it earns its keep — ranked by value today

### 1. Second tier for the worker-output injection guard (strongest fit, live gap)

`core/src/cassandra/injection_guard.rs` is a 22-entry English **substring** catalogue whose
own module doc enumerates its evasion surfaces: narrow visible whitespace is not folded,
leetspeak is not folded, non-English equivalents are absent, and under the Strict profile an
attacker who has read the catalogue can craft input that scores exactly 0.40 indefinitely.
A catalogue miss is an `Allow` under both profiles.

Shieldstral is semantic rather than lexical and covers 12 languages, which addresses three
of those four surfaces directly. The module is already shaped for it:
`InjectionDecision` is `#[non_exhaustive]` with a documented future `Review` tier at
0.45–0.70 (HANDOVER Item 30), and the guard sits at the `tool_host::dispatch` chokepoint
where every worker result passes exactly one screen with no bypass.

Proposed shape — note it is **escalate-up only**, so it can raise a verdict but never lower one:

```
catalogue score >= 0.70   -> Block            (deterministic, unchanged)
catalogue score 0.45-0.70 -> Shieldstral adjudicates -> Allow | Block
catalogue score <  0.45   -> Allow            (unchanged)
```

Two things this must carry:

- **Fail-open on router error.** Same posture the ROADMAP fixes for Guardian. The sandbox
  and the egress allowlist are the boundary; this is not.
- **It is on the hot path.** The guard screens *every* worker result synchronously. Even at
  0.45–0.70-band-only it introduces an LLM call into the dispatcher. The existing 64 KiB
  `SCAN_BYTE_CAP` conveniently bounds the input well inside 32k tokens, but the latency and
  availability characteristics of the chokepoint change, and that is a design decision, not
  an implementation detail.

### 2. Second opinion on the Constitutional Guard (Stage -1)

`constitutional.rs` is 502 lines of two-signal English substring matching over the
instruction only. The five constitutional principles are already written as prose in the
design plan and map almost 1:1 onto five Shieldstral policy questions — which is exactly
the shape the model wants.

It has one named, already-deferred target: the module doc explicitly declines to catch
`ec-001-clinical-data-leak` ("email clinical data to a third party") because doing it
lexically "risks high false-positive rates against legitimate medical questions." That is a
semantic-classifier problem stated in so many words.

**Constraint: advisory or escalate, never Block.** Tier 0 is defined as absolute and
unoverridable by any principal. A probabilistic classifier must not be able to emit an
unoverridable verdict — and, symmetrically, must never be able to *withdraw* a
deterministic one.

### 3. The reserved Stage 1 hook point

The design plan reserves Stage 1 for "fine-tuned BERT models doing risk/sentiment scoring on
outbound content", currently a no-op pass-through, with the hook existing so the pipeline
shape is stable. Shieldstral drops into that slot with no pipeline change at all:
`ReviewStage` is already the trait, `ChainReviewStage` already composes, and stage names are
already an audit-log contract.

### 4. The triage gate the question actually asks about — right idea, wrong order

Split the gate by direction, because the two halves have opposite failure modes:

- **Escalate-up** (high score ⇒ run the expensive review, or jump to Tier 2 escalation).
  **Fail-safe.** A false negative leaves you exactly where you were: the deterministic
  stages, the sandbox, and the egress allowlist all still ran. Strictly additive. Build this.
- **Skip-down** (low score ⇒ skip the expensive review). **Fail-open.** A false negative
  *removes* review that would otherwise have happened. At a vendor-claimed 84.9 F1 the model
  misses roughly one in seven on its own benchmarks, and Mistral states reliability drops on
  obfuscated input — which is precisely the input class an adversary controls. This is the
  posture the ROADMAP already rules out in writing for Guardian: *"it never becomes a gate."*

Shieldstral has better numbers than Guardian. It does not have a different epistemic status.

If the skip band is built anyway — and there is a real cost argument for it once Stage 3
exists — it needs all four of these, not a subset:

1. It may skip **only Stage 3** (LLM semantic review). Never Stage -1, never Stage 0, never
   the sandbox or the egress allowlist.
2. **Forbidden when `plan.data_ceiling` outranks `Personal`.** The design plan's own
   model-availability table already says classified data with no reviewer ⇒ auto-block
   outbound + escalate. A skip must not become a cheaper route around that.
3. **Forbidden when the plan contains any outbound step** (a `Net::Allowlist` worker,
   `mail.send`).
4. The low threshold must be **calibrated on kastellan's own labelled data with a measured
   false-negative rate** — never Mistral's τ=0.5, which is tuned to their benchmark mix.
   Every skip writes an audit row carrying the score, so the skip decision is reviewable
   after the fact and its error rate is measurable in production rather than assumed.

**And the sequencing point that settles it: Stage 3 does not exist.** `ChainReviewStage` is
`[ConstitutionalGuard, DeterministicPolicy]` today. There is no expensive analysis to gate,
so the skip band saves exactly nothing right now while adding a fail-open path. Build the
escalate-up direction first, accumulate audited scores against real traffic, and let that
data decide whether the skip band is ever justified.

## Where it does not fit

- **Not a replacement for the deterministic classification invariants.** I1/I2/I3 in
  `deterministic.rs` are arithmetic over declared labels. A classifier is strictly worse at
  arithmetic, and these are load-bearing.
- **It is a content-safety classifier, and kastellan's hardest questions are contextual
  appropriateness.** "Does this recipient match what the user asked for?" needs the
  instruction, the plan, the lineage and the destination allowlist as joint context. The
  91.3 adaptability figure measures adaptation to new *safety* policies — not to
  agentic-policy questions that are nothing like its 54.1M safety training pairs. **Treat
  every kastellan-specific policy question as out-of-distribution until measured on our own
  fixtures.** This is the single biggest reason not to buy the benchmark table at face value.
- **Multimodality is dead weight today.** No image reaches the review layer.
  `browser-driver` screenshots would be the first plausible consumer.
- **Stage 4 (longitudinal) is not a classifier problem.** It is a memory and pattern
  problem over CASSANDRA's own isolated Postgres.
- **Do not reach it via Mistral's hosted API.** Local weights are the point: they let
  `ClinicalConfidential` content be screened with no egress at all, which is exactly what
  the Stage 2 privacy gate demands ("restrict Stage 3 to local model only"). Routing this
  through the `llm-router` frontier leg would push clinical text across the very boundary
  the privacy gate exists to defend, and it would do so silently.

## Privacy and cost

The privacy story is a genuine *advantage* over the Stage 3 frontier reviewer: a second
local reviewer at 1/8th the size of the 26B planner, usable on content the privacy gate
forbids sending anywhere. On the DGX, VRAM is not the constraint — a second loaded model and
a serialised forward pass on the request path is. Note the standing measurement rule from
HANDOVER: **do not benchmark against a loaded DGX**; a full-workspace `cargo test` run
contends for the same CPU and the failure looks like a runaway-thinking bug.

## What to measure before committing (the gate)

1. ~~**macOS logprobs.**~~ **ANSWERED 2026-08-15 — PASS. The go/no-go is cleared.**
   Measured on an M-series Mac, `llama.cpp` build 9910 (`f5525f7e7`, Homebrew),
   `noctrex/Shieldstral-1.0-3B-GGUF` **Q4_K_M** + `mmproj-F16`, via
   `scripts/eval/run-shieldstral-llamacpp.sh`. `/v1/chat/completions` with
   `logprobs: true, top_logprobs: 20` returns **20 alternatives at position 0, with both
   `yes` and `no` present on every one of 14 calls** — so no score was a `-10.0` floor
   artefact. **Multimodal confirmed in the same pass** (injection text rendered into a PNG
   and sent as an `image_url` part: 0.9970 flagged; benign image 0.0014 safe), which is the
   half Ollama would not have provided. Chat-template preflight clean — the GGUF carries the
   real 7 095-char Mistral template with `[SYSTEM_PROMPT]` handling, not a broken stub.

   | | result |
   | --- | --- |
   | accuracy at τ=0.5 (14 labelled cases) | **14 / 14** |
   | should-flag scores | min 0.8596, median 0.9958, max 0.9998 |
   | should-pass scores | min 0.0007, median 0.0011, max 0.0635 |
   | **margin** (min flag − max pass) | **+0.7961** |
   | distinct scores | 14 / 14 — continuous, so banding is mechanically possible |
   | latency, quiet Mac | p50 **40 ms**, p90 54 ms, max 54 ms (text); ~296 ms image |

   Evasion surfaces from measurement 4 were included and all flagged: leetspeak (0.8596 —
   the weakest attack score, consistent with Mistral's own stated weak spot), narrow
   whitespace (0.9945), German (0.9972), indirect injection (0.9997). Benign controls that
   *mention* prompt injection were correctly passed (0.0635).

   > ### The finding that matters more than the numbers: **the policy prompt is load-bearing**
   >
   > The first run of this same harness, same weights, same documents, scored **11/14 with a
   > NEGATIVE margin (−0.166)** — no threshold separated the classes at all. Three attacks
   > were missed, including a textbook indirect prompt injection scored **0.0038**:
   > confidently safe. The only difference was the policy prompt. The model card's
   > prompt-engineering section says to name the candidate classes in `<Instruct>` and then
   > ask **one broad** `<Query>`; the first attempt asked a narrow, specific question and
   > named no classes.
   >
   > | document | no classes + specific `<Query>` | classes + `"Is this content unsafe?"` |
   > | --- | --- | --- |
   > | exfil-secrets | 0.0206 | **0.9996** |
   > | tool-abuse | 0.1032 | **0.9812** |
   > | indirect-injection | 0.0038 | **0.9998** |
   > | security-topic *(benign control)* | 0.1699 | 0.0773 |
   >
   > **Read the wrong way, run 1 was a no-go**: it looks exactly like "Shieldstral cannot do
   > agentic injection detection, only content safety." That conclusion would have been
   > drawn from a defect in the prompt, not a property of the model. Two consequences for the
   > build: (a) the `<Instruct>` block is a **tuned artefact that belongs in version control
   > with its measurements**, not a string a future contributor rewords for readability —
   > same class as [[plan-text-is-a-defect-source]]; (b) measurement 3's calibration set must
   > re-run whenever that block changes, because it moves every score.

   **Still open, and not to be over-read from this.** 14 examples is a smoke test, not a
   calibration set — measurement 3 (≥100 labelled cases) stands, and the τ that separates
   *this* set is not a fitted threshold. These numbers are **Q4_K_M**; the study's own
   caveat that quantisation moves calibration applies, so a DGX BF16/vLLM leg needs its own
   run before any threshold is shared across hosts. The false-negative rate on
   out-of-distribution agentic-policy questions — the number that actually gates adoption —
   remains unmeasured.
2. ~~**`llm-router` round trip.**~~ **DONE 2026-08-16.** `ChatRequest` gained
   `logprobs`/`top_logprobs` behind `skip_serializing_if` (set together via
   `with_logprobs`, because vLLM 4xxs on a count without the bool), `ChatChoice`
   gained `logprobs: Option<LogProbs>`, and the renormalisation lives in the new
   pure `llm-router::logprob_score`. Additive only: no existing caller passes the
   fields and no dispatcher signature moved.

   Two things worth carrying forward from building it:

   - **`None` means UNMEASURED and the type enforces it.**
     `binary_token_probability` returns `Option<f32>` and yields `None` unless
     **both** verdict spellings are observed. This is the Python probe's
     fail-open defect made unrepresentable rather than commented against: a
     sentinel floor on the missing side renormalises to exactly 0.5 when neither
     spelling is present — which reads as "below τ", i.e. safe — and to a
     confident 0.9999 when only one is. Mutation-tested: reintroducing the floor
     (`unwrap_or(-10.0)`) fails four tests.
   - **Token identity is a tokenizer problem.** `Ġyes` (byte-BPE) and `▁yes`
     (SentencePiece) survive any amount of trimming, so a matcher on the display
     string can floor an entire run at once when the backend changes. The scorer
     prefers the wire's `bytes` (which decode to plain ` yes` on every family)
     and falls back to marker-stripping on the display form. **The first test for
     this was vacuous** — `Ġyes` with `bytes: " yes"` passes whether or not
     `bytes` is consulted, because the normaliser strips `Ġ` by itself — so it
     was rewritten with display forms no folding rule can rescue.

   **`disable_thinking` is safe against Shieldstral's template — measured, not
   assumed.** Against llama.cpp both calls returned **HTTP 200 with an identical
   26-token prompt**, and the one carrying
   `chat_template_kwargs: {"enable_thinking": false}` reported `cached_tokens: 25`
   — positive evidence the *rendered prompt was byte-identical*, which is a
   stronger claim than "no 4xx". So the guard call needs no opt-out seam.

   Still owed on this measurement: the **vLLM leg is unpinned**, because the DGX
   has no vLLM serving Shieldstral (below). The Ollama leg is pinned by a fixture
   copied from a real DGX Ollama 0.22.0 response rather than reconstructed.
3. **A calibration set.** `tests/observation/captures/` holds **7** fixtures
   (five principles, one clinical-leak edge case, one safe control). Seven is not a
   calibration set — a threshold fitted to it means nothing. Target ≥ 100 labelled
   plans/worker-outputs. The vehicle already exists: `kastellan-cli observation replay`
   replays captures through the production `ChainReviewStage`; extend it to score a
   candidate stage and report a confusion matrix rather than a verdict delta.
4. **Adversarial run.** Push the catalogue's own documented evasion surfaces — narrow
   visible whitespace (U+2009/200A/202F), leetspeak, non-English phrasings — through
   Shieldstral. That specific capability is what is being bought, and it is the same class
   of input Mistral flags as its weak spot. Buying it unmeasured would be buying the claim.
5. **Latency.** p50/p99 of a single `max_tokens=1` call on a quiet DGX and a quiet Mac.
   This lands on the dispatcher hot path; a number is required, not an estimate.

### Measurement 1, Q8 leg (2026-08-16)

**Runtime decision: llama.cpp + `Shieldstral-1.0-3B-Q8_0.gguf` + `mmproj-BF16` on BOTH
hosts.** Same bits, same chat template, one calibration — so a fitted τ transfers instead of
needing a per-host story. The alternatives were both measured and rejected:

- **vLLM on the DGX.** The architecture is *not* the blocker this time — Shieldstral's
  `config.json` declares `Mistral3ForConditionalGeneration`, which is in the 272-arch
  registry of the DGX's `nvcr.io/nvidia/vllm:26.02-py3` container. Two other things are:
  that container ships **vLLM 0.15.1** against a model card asking for **≥ 0.26.0** (the
  model postdates the image by six months), and vLLM's GGUF path is an **experimental,
  single-file-only, now out-of-tree plugin** with `mmproj` being a llama.cpp convention. So
  vLLM would serve **BF16 safetensors** — different weights, and the study's own
  "quantisation moves calibration" caveat then forbids sharing a threshold. It buys
  throughput nothing else needs and costs the property the decision was made for.
- **Ollama on the DGX.** Newly measured and *capable*: Ollama **0.22.0** returns
  `logprobs`/`top_logprobs` on `/v1/chat/completions` (probed 2026-08-16 — the study had
  this as research only). Rejected anyway, because the GGUF would need a hand-rolled
  Modelfile, which is the broken-stub-template hazard that bit the Agents-A1 import, and
  because two packagings means two calibrations.

**Result: PASS, 14/14, and slightly better separation than Q4.**

| | Q4_K_M (2026-08-15) | **Q8_0 (2026-08-16)** |
| --- | --- | --- |
| accuracy at τ=0.5 | 14 / 14 | **14 / 14**, all measured |
| should-flag | min 0.8596, median 0.9958 | min **0.9036**, median 0.9920 |
| should-pass | max 0.0635 | max 0.0886 |
| **margin** | +0.7961 | **+0.8151** |
| distinct scores | 14 / 14 | 14 / 14 |
| latency (quiet Mac) | p50 40 ms, p90 54 ms | p50 **43 ms**, p90 57 ms |
| multimodal inject / benign | 0.9970 / 0.0014 | 0.9968 / 0.0013 |
| chat template | 7 095 chars | 7 095 chars, verified via `/props` |

Leetspeak — Mistral's own stated weak spot and the weakest attack score in both runs —
improves most (0.8596 → 0.9036). The `<Instruct>` block was unchanged (`POLICY_DIGEST`
`342e3d9661b2cbe2`), so these two runs are comparable.

#### The harness could not have produced these numbers, and that is the transferable part

Both defects are the "a check that cannot fail" family the previous review round was already
hunting in this same file — one layer over, in the *setup* rather than the measurement.

1. **The readiness loop could never wait.** `curl` exits **7** on a refused connection, and
   under `set -eu` a command substitution's status becomes the assignment's — so
   `code=$(curl …)` killed the script on iteration 1, every time, with curl's own 7 surfacing
   as the harness's exit status. The loop could therefore only ever succeed against a server
   **someone else had already started**. Its comment reasoned carefully about curl's *stdout*
   (`000`, hence no `|| echo 000` fallback) and never considered its *exit status*.
   Consequence worth stating plainly: the wrapper the study names as its reproduction path
   had never run end to end, and the Q4 numbers were produced by hand.
2. **An occupied port was indistinguishable from a healthy start.** With the loop fixed, a
   second server already on the port means our `llama-server` cannot bind and dies, while the
   readiness probe takes its 200 from the stranger — and `--alias` makes every Shieldstral
   server answer to the same name, so the run reports a clean pass **over unknown weights**.
   Now refused explicitly (exit 8). This is the most likely mechanism behind (1) going
   unnoticed.

**And the chat-template preflight was checking the wrong thing.** It grepped the server's
startup log for a template line; llama.cpp's startup wording is build-dependent and the build
measured here prints **no such line at any verbosity**, so a grep miss was indistinguishable
from "this GGUF carries no template" — collapsing a clean model and the exact
silent-corruption hazard being guarded into one output. It now asks `/props` what template is
actually in force and rejects both an absent one and a chatml fallback (llama.cpp's
substitute when a GGUF carries none, which parses fine and silently reframes every
`<Instruct>` block). That is how the Q8 template was confirmed at 7 095 chars.

**Still owed:** the **DGX leg**. `llama-server` is not installed there, so "one τ across
hosts" is currently an argument from identical bits, not a measurement. Until it runs, no
threshold should be described as cross-host.

## Effort estimate

| Slice | Size |
| --- | --- |
| Measurements 1, 4, 5 (no code beyond a probe script) | ~½ session, and it is a genuine go/no-go |
| `llm-router` logprobs plumbing (measurement 2) | ~1 session — small and additive, but it touches the sole core-side LLM egress, so both legs need pins |
| `ShieldstralStage` + injection-guard `Review` tier, escalate-up only, fail-open | 1–2 sessions |
| Calibration-harness extension (measurement 3) | ~1 session of code; the labelling is operator time and is the real cost |

Roughly 3–4 sessions to a shippable advisory tier, with the go/no-go probe first.

## Loud gotchas

- **Every performance number is a vendor claim with no independent replication.** Treat the
  table as a reason to run the measurement, not as the measurement.
- ~~**The macOS logprobs leg is the single point of failure for the whole design.**~~
  **RESOLVED 2026-08-15.** Checking it first is exactly what paid: oMLX has no logprobs
  (found before any code was written), and llama.cpp does — measured, with multimodal, at
  p50 40 ms. See measurement 1. **The replacement gotcha is the policy prompt**: same
  weights and same documents went from 11/14 with a negative margin to 14/14 at +0.796 on
  the wording of `<Instruct>` alone.
- **Quantisation moves the calibration.** A threshold fitted on BF16 does not transfer to
  Q4_K_M. Pin one quantisation across hosts, or calibrate per host and say so.
- **The advisory posture is not a formality.** It is what keeps a probabilistic component out
  of the containment boundary. Anything that lets this model *lower* a deterministic verdict —
  skip a stage, downgrade a Block, withdraw a constitutional hit — converts a defence-in-depth
  layer into a hole in the threat model. The ROADMAP's Guardian wording is the right posture
  and should be carried over verbatim.
- **English-only is no longer an excuse but is also no longer the constraint it was.**
  `constitutional.rs` documents English-only coverage on the grounds that the user is an
  anglophone. Shieldstral's 12 languages are a genuine widening for the *injection guard*
  (fetched web content is arbitrary), but note Arabic scores poorly and Indonesian is not in
  the 12 at all.

## Recommendation

**ADOPT-CONDITIONALLY, as the Phase 5 model-based guard tier, in preference to IBM Granite
Guardian 4.1** — subject to the five measurements above, measurement 1 being a hard go/no-go.

The case over Guardian: better published numbers at 3B vs 8B, the same Apache-2.0 licence,
the same zero-new-egress deployment through the existing `llm-router` local backend, and one
property Guardian does not have — policy as a runtime prompt, so a single set of weights
serves the injection guard, the constitutional second opinion and the Stage 1 hook without a
fine-tune per hook. Its calibrated score is also what makes a *banded* decision possible at
all, which is the thing the original question was reaching for.

**Carry Guardian's posture unchanged: advisory, fail-open, never the boundary.** The OS
sandbox and the egress proxy's deterministic allowlist/SSRF/pinning remain the only things
that actually contain a compromise. This lowers attempt volume and enriches the audit log;
it does not change the threat model, and `docs/threat-model.md` should not gain a line
claiming it does.

**On the specific question asked** — a gate deciding whether deeper CASSANDRA analysis is
needed: build the **escalate-up** half now, as the injection-guard `Review` tier, where the
gap is real and documented and every failure mode is fail-safe. The **skip-down** half is
the right long-term shape but is blocked on Stage 3 existing, and when it is built it must
carry all four constraints listed above. "With highest confidence" is achievable — that is
what the calibrated score buys — but the confidence has to be measured on kastellan's own
labelled data, because the number that matters here is a false-negative rate on
out-of-distribution agentic-policy questions, and no published benchmark reports it.

## Sources

- [Shieldstral 1.0 model card — Mistral Docs](https://docs.mistral.ai/models/model-cards/shieldstral-1-0)
- [Introducing Shieldstral — Mistral AI](https://mistral.ai/news/shieldstral/)
- [`mistralai/Shieldstral-1.0-3B` — Hugging Face](https://huggingface.co/mistralai/Shieldstral-1.0-3B)
- [Mistral AI Releases Shieldstral 1.0 3B — MarkTechPost](https://www.marktechpost.com/2026/08/07/mistral-ai-releases-shieldstral-1-0-3b/)
- [Shieldstral Takes Your Safety Policy at Inference Time — digitalapplied](https://www.digitalapplied.com/blog/mistral-shieldstral-runtime-policy-guard-model-agents)
- [Mistral's open model Shieldstral matches much larger safety models — the-decoder](https://the-decoder.com/mistrals-open-model-shieldstral-matches-much-larger-safety-models/)
- [Mistral introduces Shieldstral — SiliconANGLE](https://siliconangle.com/2026/08/05/mistral-introduces-shieldstral-provide-lightweight-policy-aware-moderation-ai-models/)
- [Mistral releases Shieldstral — MLQ News](https://mlq.ai/news/mistral-releases-shieldstral-a-compact-open-weight-guardrail-for-custom-ai-safety-policies/)
- [Mistral's Shieldstral Packs Policy-Adaptive Safety Screening Into 3B Parameters — unite.ai](https://www.unite.ai/mistrals-shieldstral-packs-policy-adaptive-safety-screening-into-3b-parameters/)
- [Add logprobs support to OpenAI-compatible endpoint — ollama/ollama#16117](https://github.com/ollama/ollama/issues/16117)
- [OpenAI compatibility — Ollama docs](https://docs.ollama.com/api/openai-compatibility)
