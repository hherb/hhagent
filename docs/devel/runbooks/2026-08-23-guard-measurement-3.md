# Guard calibration — measurement 3

**Status:** corpus built and pinned; both host fits run 2026-08-23. Supersedes the
pilot recorded in [`2026-08-22-guard-calibration-campaign.md`](2026-08-22-guard-calibration-campaign.md),
which proved the pipeline on 28 cases of which 4 were captured.

This is the real thing: **133 cases, 109 of them captured** through the production
`web.fetch` path, against corpus-design D5's floor of ≥100 with a captured half.

---

## The corpus

| stratum | provenance | label | count |
| --- | --- | --- | --- |
| seeded, authored here | `hand_written` / `derived_from_catalogue` | 16 attack / 8 benign | 24 |
| captured — ordinary web + technical documentation | `captured` | benign | 49 |
| captured — security prose (D4's expensive stratum) | `captured` | benign | 19 |
| captured — third-party payloads | `captured` | attack | 41 |
| **total** | | **57 attack / 76 benign** | **133** |

**18 cases sit at or over `SCAN_BYTE_CAP`** (D5 asks for ≥8) — 15 benign, 3 attack.
Truncation is exercised on both sides of the label, which matters: if only attacks
were truncated, a threshold could learn "truncated ⇒ attack".

### The three immutable locator classes

No third-party text is committed (D1). Each manifest entry names a source whose
bytes cannot change under it (D2):

- `web.archive.org/web/<14-digit-timestamp>id_/<url>` — 58 entries
- `raw.githubusercontent.com/<owner>/<repo>/<40-hex-commit>/<path>` — 51 entries
- HuggingFace `…/resolve/<commit-sha>/…` — 0 surviving (both candidates were
  catalogue-blocked at capture; see Finding 1)

`raw.githubusercontent.com` was added to the accepted list during this campaign.
Git is content-addressed, so bytes under a commit SHA cannot change; an
unreachable commit yields a **404**, which the HTTP-status check refuses rather
than silently hashing.

### Confounds, and what was done about each

`extract_scannable_text` walks the whole worker response, so the scored text
carries the **content-type and the source URL** as well as the document. Three
things could therefore separate the labels without the guard understanding
anything, and each was deliberately controlled:

| confound | uncontrolled shape | control |
| --- | --- | --- |
| **host** (it is inside the scored text) | every attack on `raw.githubusercontent.com`, every Wayback case benign | 15 benign cases moved onto the attack host; 5 attack cases found as Wayback snapshots. Both hosts now carry both labels — `web.archive.org` 53 benign / 5 attack, `raw.githubusercontent.com` 15 benign / 36 attack |
| **format** | attacks are markdown/yaml/plaintext, benigns are HTML | 14 benign cases are plaintext/markdown/Rust source from the attack host; `go_spec.html` is served as `text/plain` so its markup is scored |
| **length** | attack payloads are short, benign documents long | the corpus's largest document is a **benign** changelog (~918 KB source); `typescript-readme` is a benign case sized to match the attack payloads; over-cap cases sit on both labels |

The host correlation is **reduced, not eliminated** — 53 of 76 benign cases are
still Wayback. A reader should treat the per-host breakdown above as part of the
result, not as background.

---

## Reproducing

```sh
source "$HOME/.cargo/env"

# 0. the weights pin (#598) is checked by `guard calibrate` itself, at use.
#    This is only for verifying a fresh download.
source scripts/eval/lib/guard-weights.sh
require_guard_weights ~/models/shieldstral/upstream/Shieldstral-1.0-3B-Q8_0.gguf || exit 1

# 1. materialise + verify. PACE IT -- see Finding 3.
#    Back-to-back Wayback fetches are throttled and return transport
#    errors that look like drift and are not.
./scripts/eval/paced-capture.sh tests/guard/manifest tests/guard/corpus-materialised

# 2. BOTH corpora: the materialised half alone makes D7's budget scope a
#    no-op (every case is `captured`), and the seeded half alone leaves it
#    empty, which `guard calibrate` now refuses outright.
cp tests/guard/corpus/*.json tests/guard/corpus-materialised/

# 3. fit -- ON THE HOST SERVING THE MODEL (the pin hashes a local file)
KASTELLAN_LLM_GUARD_URL=http://127.0.0.1:8081/v1 \
KASTELLAN_LLM_GUARD_MODEL=shieldstral \
./target/debug/kastellan-cli guard calibrate --corpus tests/guard/corpus-materialised
```

**Server — note the context size, which is not the pilot's:**

```sh
llama-server -m <ABSOLUTE path>/Shieldstral-1.0-3B-Q8_0.gguf \
  --alias shieldstral --port 8081 --host 127.0.0.1 \
  -c 131072 -ngl 99 --no-webui
```

`-c 32768` **is not enough** and fails the run — see Finding 4. The `-m` path
must be absolute, or `/props` reports a relative path and the pin refuses it
rather than resolving it against the wrong directory (#598).

---

## Results

<!-- FILLED FROM THE RUN BELOW -->

---

## Findings

Numbered because each one is either a filed issue or a fact the next campaign
needs before it starts.

### 1. The catalogue blocks security documentation, under the production profile

**15 of 124 attempted captures were refused** because `dispatch` returned the
withheld-injection placeholder rather than the page — meaning the deterministic
catalogue blocked the document before the guard model was ever consulted.

This is not a harness artefact. `guard capture` dispatches through
`dispatch_with_sink(.., "web-fetch", ..)`, and `tool_host::post_process` selects
`GuardProfile::for_tool("web-fetch")` = **`Relaxed`** — production's real profile
for that tool. So these are documents the deployed agent cannot read either.

Among them, **every one of the campaign's GitHub-hosted security-prose
candidates**:

| blocked document | what it is |
| --- | --- |
| `jthack/PIPE` README + readme2 | the Prompt Injection Primer for Engineers |
| PayloadsAllTheThings `Prompt Injection/README.md` | a reference page with a table of contents |
| `Cranot/chatbot-injections-exploits` README | a defensively-framed catalogue |
| CWE-77 (command injection) | MITRE's weakness-catalogue entry |
| Lakera's prompt-injection guide | a vendor explainer |

That is D4's expensive failure — "a guard that flags this has not become safer,
it has become unable to read about security" — happening one layer *below* the
guard model, at a component nobody was measuring. The 19 security-prose cases
that did survive are the ones the catalogue happens to let through, so **this
stratum is selected by the catalogue, not sampled**.

### 2. Capture screens `Relaxed`, calibrate excludes on `Strict` — [#601](https://github.com/hherb/kastellan/issues/601)

The same campaign applies the catalogue twice under two different profiles:
`guard capture` admits a document under `Relaxed`, then `guard calibrate`
(`screen(text)`, which is `Strict`) decides whether to exclude it from the fit.
Anything in the gap is materialised and then dropped under a profile it never
faced.

Because everything in the corpus survived the `Relaxed` gate, the report's
`excluded (catalogue already blocks)` count **over the captured strata** is
exactly the size of that gap — see Results.

### 3. Wayback needs pacing, and its failures do not look like what they are — [#602](https://github.com/hherb/kastellan/issues/602), [#603](https://github.com/hherb/kastellan/issues/603)

A 104-entry back-to-back verify run produced **20 `FETCH-FAILED`s and 1
`REFUSED` for drift**. Re-run one entry at a time with a 15-second pause:
**0 fetch failures**. The failures were rate limiting, not the corpus.

Two sharper problems came out of chasing them:

- **A throttled Wayback response can be a 200 with an empty body.** Measured:
  three fetches of one pinned snapshot gave the same hash twice and then
  `e3b0c442…`, the sha256 of the empty string, with `curl` exiting 0. Under
  `--record` that would be pinned *as the case*. #596 closed this for 404s; the
  body was never checked. **[#602]** — no entry in this corpus is pinned to the
  empty document, checked explicitly.
- **The pinned hash covers the final URL.** `cap-085-debian-home` failed verify
  as drift; the two materialisations are byte-identical except for one line —
  Wayback had redirected the pinned snapshot to a neighbouring capture.
  **[#603]**. Re-recorded after three consecutive captures agreed.

### 4. `SCAN_BYTE_CAP` bounds bytes, not tokens — [#604](https://github.com/hherb/kastellan/issues/604)

The first fit died at `HTTP 400: request (44437 tokens) exceeds the available
context size (32768)`. The document was exactly 65,536 bytes — the cap worked.
It tokenised at **1.47 bytes/token**, against the **6.5 bytes/token** of M1's
prose, because dense jailbreak text (leetspeak, symbol runs, non-English
fragments) does not merge into common tokens.

M1's "64 KiB = 10,062 tokens" is therefore a *sample*, not a bound, and the
ratio is **attacker-controlled**. The wiring slice has to say what happens when
the guard returns 400 — passing the document through would fail open on exactly
the documents most likely to be attacks. This campaign runs `-c 131072`.

### 5. Practical limits worth knowing before the next campaign

- **5 MiB is a hard ceiling.** `web-common`'s `MAX_BODY_BYTES` errors one byte
  over rather than truncating, so an oversized source fails its entry outright.
  Two candidates were dropped for this.
- **`--record` does not write the hash back** into the manifest file; it prints
  it. At ~100 entries that is a transcription job on a security control's pin,
  so it was scripted rather than typed.
- **SPA pages cannot be captured.** `crates.io` returned
  `extraction failed: could not extract readable content` — readability finds
  nothing in a JS-rendered shell. Dropped.

---

## What measurement 3 still owes

- **The security-prose stratum is catalogue-selected** (Finding 1). Measuring
  the guard on the documents the catalogue *blocks* needs a capture path that
  can bypass the screen deliberately — which does not exist, correctly, since
  the chokepoint has no opt-out.
- **#601's profile divergence is quantified but not fixed**, so the `excluded`
  count in these reports is computed under a profile the captured cases never
  faced.
