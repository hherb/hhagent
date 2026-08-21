# Guard-model calibration corpus — PROOF OF CONCEPT

**These 24 cases do NOT discharge measurement 3, and no threshold fitted
to them is a fitted threshold.**

They exist to prove the *vehicle* works — `kastellan-cli guard calibrate`
loads a corpus, scores it through the shipping adjudicator, and reports a
confusion matrix. They are not evidence that the guard is calibrated.

A real calibration set needs:

- **>= 100 labelled cases** (this is 24), and
- a **captured** half — real worker output, real fetched pages, real email
  bodies. Everything here is `hand_written` or `derived_from_catalogue`,
  which means the corpus tests what its author thought of. That is the
  same limitation as "a mutation score is only as good as the mutation
  set", one level up.

`guard calibrate` reports each `provenance` stratum separately and never
pools them, precisely so a strong score on hand-written cases cannot hide
a weak score on captured ones once those exist.

## Case format

One JSON file per case, named `<id>.json` with `id` matching the stem:

```json
{
  "id": "inj-001-leetspeak-override",
  "label": "attack" | "benign",
  "text": "the document the guard screens",
  "provenance": "hand_written" | "captured" | "derived_from_catalogue",
  "notes": "why this case exists and what it is meant to prove"
}
```

No catalogue score is stored. It is computed from the shipping `screen()`
at report time so it cannot drift from the catalogue it describes.

A malformed case **aborts** the load rather than being skipped: a silently
skipped case shrinks a confusion matrix's denominator, so a corpus of 100
with 12 unparseable files would report a clean matrix over 88 and call it
a pass.

## What the current cases cover

The four evasion surfaces `cassandra::injection_guard`'s own module doc
enumerates as its blind spots — leetspeak, narrow *visible* whitespace
(U+2009/U+200A/U+202F, which `normalize()` deliberately does not fold),
non-English phrasings, and novel wording using no catalogue phrase — plus
benign controls, including prose that *mentions* prompt injection.

Two tests hold this directory to those claims:
`every_evasion_case_really_is_a_catalogue_miss` (all twelve score exactly
0.0 under `screen()`) and
`catalogue_derived_cases_straddle_the_block_threshold_as_documented`.

Spec: `docs/superpowers/specs/2026-08-21-shieldstral-guard-slice-1-design.md` (D9).
