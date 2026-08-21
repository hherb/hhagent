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

One JSON file per case, named `<id>.json` with `id` matching the stem.
**The loader enforces this** (`CorpusError::IdStemMismatch`), because the
corpus tests below select their populations *by id prefix* — so a case
whose id drifted off its filename would silently drop out of the test
written to validate it. It is also what makes ids unique without a
separate duplicate check.

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

An **unknown field** aborts too (`deny_unknown_fields`). Without that, a
typo'd key — `"note"` for `"notes"` — is silently ignored, and a case
whose stated reason for existing lands in a field nobody reads is a case
whose claims are never checked.

The same reasoning governs the exit status: a run in which any adjudicated
case was **unmeasurable**, or in which **nothing was adjudicated at all**
because the catalogue already blocks every case, exits **1**. Both print
`RUN INVALID`. A confusion matrix over zero cases is not a clean matrix.

## What the current cases cover

The four evasion surfaces `cassandra::injection_guard`'s own module doc
enumerates as its blind spots — leetspeak, narrow *visible* whitespace
(U+2009/U+200A/U+202F, which `normalize()` deliberately does not fold),
non-English phrasings, and novel wording using no catalogue phrase — plus
benign controls, including prose that *mentions* prompt injection.

Three tests hold this directory to those claims, and each asserts the
*identities* rather than a count — a count stays green while a whole
family is swapped out:

- `every_evasion_case_really_is_a_catalogue_miss` — the exact twelve ids,
  grouped by the four families, each scoring exactly 0.0 under `screen()`,
  with no stray `inj-*` case outside a declared family.
- `every_benign_control_is_a_catalogue_miss_and_stays_adjudicated` — the
  eight `safe-*` controls. They are `hand_written` too, so the evasion
  test never saw them; a future catalogue entry pushing one to
  `>= BLOCK_THRESHOLD` would silently *exclude* it, shrinking the benign
  population and **inflating** apparent specificity.
- `catalogue_derived_cases_straddle_the_block_threshold_as_documented` —
  the exact scores (0.50 / 0.50 / 1.00 / 1.00), not merely which side of
  the threshold they fall on. `score < BLOCK_THRESHOLD` is also satisfied
  by 0.0, so a case that stopped matching the catalogue entirely would
  otherwise still pass.

Spec: `docs/superpowers/specs/2026-08-21-shieldstral-guard-slice-1-design.md` (D9).
