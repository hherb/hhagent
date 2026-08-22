# The calibration corpus manifest

Metadata only. **No third-party text is committed here** — corpus-design
spec D1. Each `*.json` file names one case by reference:

```json
{
  "id": "cap-001-example-page",
  "label": "benign",
  "provenance": "captured",
  "source": "https://web.archive.org/web/20260822042341id_/https://example.com/",
  "notes": "why this case exists, in one line",
  "sha256": "297ff90e…"
}
```

`kastellan-cli guard capture --manifest tests/guard/manifest --out <dir>`
fetches each `source` through the real sandboxed `web-fetch` worker,
hashes what the chokepoint saw, and materialises the case into `<dir>`,
which is gitignored.

## The review-time invariant: `source` must be immutable

**This is the one rule nothing in the code checks, and it is checked
here instead.** `load_manifest_from_dir` deliberately does not validate
it, because immutability is a property of a URL's *meaning* and not of
its syntax: `…/resolve/main/x` and `…/resolve/<sha>/x` are both
well-formed and only the second is a pin. A regex would reject typos
while passing the actual mistake.

So, when reviewing a new entry, check that `source` is one of:

- a **Wayback Machine snapshot** with an explicit timestamp, in the
  `id_` form that returns the original bytes rather than the Wayback
  wrapper: `https://web.archive.org/web/<14-digit-timestamp>id_/<url>`;
- a **HuggingFace URL pinned to a dataset revision**:
  `…/resolve/<commit-sha>/…`. Never `resolve/main`;
- any other locator whose content cannot change under it.

A `sha256` over a live page is a hash of whatever it said that day, and
a corpus nobody can reproduce is a τ nobody can check.

## What the loader does enforce

`id == <filename stem>`; a non-empty directory; `source` is `https://`
and at most `SOURCE_MAX_BYTES`; `notes` at most `NOTES_MAX_BYTES`;
`sha256` at most 64 bytes; `provenance` is `captured` (the only legal
value — anything authored here has its text committed directly and
belongs in `../corpus/`); and `deny_unknown_fields`, so a `text` key is
a load error rather than a silently ignored one.

## Recording a hash

A new entry is committed **without** `sha256`, then:

```sh
kastellan-cli guard capture --manifest tests/guard/manifest \
  --out tests/guard/corpus-materialised --record
```

prints `RECORD-NEW <id> <hash>` for it. Commit that hash. Entries that
already carry one print `RECORD-SAME` and are **still verified** —
`--record` is not a way to skip the check, and a source that has drifted
is refused in both modes. Re-pinning a changed source means deleting its
`sha256` field deliberately, which is a reviewable diff.

## Licensing

Referencing a source and pinning its hash is not redistribution, which
is what makes this work for material whose terms are unclear — spec F3
records an aggregate dataset stamped Apache-2.0 at the top level over a
component with no stated terms at all. The same mechanism keeps
operator-private material out of a public repo while letting a case
point at it.
