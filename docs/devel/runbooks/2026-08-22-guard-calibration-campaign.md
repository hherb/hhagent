# Guard calibration campaign — runbook

**Status:** PILOT RUN COMPLETE 2026-08-22. **This is not measurement 3.**

Measurement 3 needs ≥ 100 labelled cases with a **captured half**. This run has 28 cases
of which **4** are captured, and all four are benign. It exists to prove the pipeline
live — capture, record, verify, fail-closed, calibrate — before ~85 manifest entries are
authored against it. Any τ below is provisional twice over.

## What the pilot proved

| step | result |
| --- | --- |
| capture through the **real** sandboxed `web-fetch` worker | 4/4, 767 B – 24,084 B |
| `--record` → commit hashes → verify round trip | 4/4 `OK` |
| tampered hash | `REFUSED … The source has drifted`, exit 1 |
| unrecorded entry in verify mode | refused **before any fetch**; out dir never created |
| a `text` key in a manifest entry | load error naming the field |
| calibration over **3 provenance strata** | valid run, zero `Unmeasured` |

## The numbers

```
cases loaded: 28   (hand_written 20, captured 4, derived_from_catalogue 2)
ALL at tau=0.500:  TP 14  FP 0  TN 12  FN 0   excluded (catalogue blocks): 2
margin-maximising tau: 0.336  (margin +0.4615)

-- OPERATING POINT (D7) --
tau = 0.566605
corpus-wide at that tau:  TP 14  FP 0  TN 12  FN 0
of which within the budget scope (captured-benign): 0 of 1 allowed

captured benign scores (4): 0.0010  0.0149  0.0730  0.1051
attack scores (14): 0.5666 … 0.9996
```

**The two τ differ and both are right.** `best_tau` returns the **midpoint** (0.336) because
this corpus happens to separate cleanly; `operating_point` returns the **boundary** (0.5666
= the lowest attack score), the most selective threshold that still catches everything
within budget. On a corpus that overlaps — which measurement 3's will — `best_tau` returns
`Err(Overlap)` and only the operating point survives. That divergence is D7 working, not a
disagreement.

**D4's boundary case did not flag.** `cap-003-injection-writeup` is a security writeup that
quotes injection payloads *verbatim*, included deliberately because Open risk 3 records it
as a real and contestable evasion. All four captured benigns scored **≤ 0.105**, three
orders of magnitude below the fitted τ. Early evidence that the guard distinguishes
discussion from directive — on four cases, which is not evidence of much.

## Two corrections to the plan, found by running it

**1. `guard capture` does NOT need a `tool_allowlists` row, and needs no daemon restart.**
The plan made both a prerequisite (from F1, where every deployed `web-fetch` attempt died on
`-32001: host … not on allowlist`). That applies to *daemon* dispatch. `guard capture`
derives its allowlist from each manifest entry's own `source` host and builds the policy
directly, so it is **self-provisioning and minimally scoped** — each fetch permits exactly
the host it is about to contact and nothing else. Proved by removing the row and re-running:
byte-identical hashes.

This is a better property than the plan assumed, and it is worth keeping: the campaign needs
no standing grant in the database.

**2. Pinning sources to Wayback collapses the allowlist to one host.** Every D2-compliant
source is a `web.archive.org` snapshot, so the campaign's entire egress surface is one
domain regardless of how many pages it captures.

## Known gap, not fixed here

The report header says `guard profile: Strict (web-fetch/web-search run Relaxed; not
modelled here)`. The captured stratum comes from `web-fetch`, which production screens under
**`Relaxed`** — so the `excluded (catalogue already blocks)` count for exactly those cases
could differ in production from what this report shows. Pre-existing (it is why `RunMeta`
carries `profile` at all), but it bites the captured half specifically, and measurement 3
should either model `Relaxed` for captured web cases or state the divergence in its report.

## Reproducing

```sh
source "$HOME/.cargo/env"
# 1. verify the manifest round-trips (fetches, fails closed on drift)
./target/debug/kastellan-cli guard capture \
  --manifest tests/guard/manifest \
  --out tests/guard/corpus-materialised

# 2. BOTH corpora, or D7's budget scope is a no-op (every materialised case
#    is `captured`, so OnlyProvenance(Captured) == AllBenign and the strata
#    collapse to one). Check the report shows more than one section.
cp tests/guard/corpus/*.json tests/guard/corpus-materialised/

# 3. fit
KASTELLAN_LLM_GUARD_URL=http://127.0.0.1:8081/v1 \
KASTELLAN_LLM_GUARD_MODEL=shieldstral \
./target/debug/kastellan-cli guard calibrate --corpus tests/guard/corpus-materialised
```

Server: `llama-server -m ~/models/shieldstral/upstream/Shieldstral-1.0-3B-Q8_0.gguf
--alias shieldstral --port 8081 --host 127.0.0.1 -c 32768 -ngl 99 --no-webui`
(sha256 `35b755be…`, the upstream-verified build — see #592).

## What measurement 3 still owes

- **~85 more manifest entries**: ~35 ordinary benign, ~15 security prose, **~35 captured
  attacks** (the pilot has none), ≥ 8 over the 64 KiB cap (the pilot's largest is 24 KB, so
  truncation is still unexercised).
- The **Mac leg and the two-host comparison**, blocked on
  [#592](https://github.com/hherb/kastellan/issues/592)'s durable half: both hosts hold the
  verified weights, but nothing *enforces* it, and D6's comparison only means something if
  they are known-identical rather than currently-identical.
