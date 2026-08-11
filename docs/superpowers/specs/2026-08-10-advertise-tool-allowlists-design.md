# Advertise operator allowlists to the planner — design

**Issue:** [#533](https://github.com/hherb/kastellan/issues/533)
**Date:** 2026-08-10
**Status:** design approved, not yet implemented

---

## 1. The measurement, and how it changes the issue

#533 as filed claims two problems. **Only the second one is real**, and the
first was already fixed twice over before the issue was written. This section
records the evidence, because the issue text is wrong and will otherwise be
transcribed into the implementation — the [[plan-text-is-a-defect-source]]
failure mode.

Measured on the live DGX `audit_log`, all time, 2026-08-10:

| Regime | Dispatches | Failures | Rate |
| --- | --- | --- | --- |
| 2026-06-20 → 06-21 (allowlist **empty**) | 15 | 15 | 100 % |
| 2026-06-22 → 08-08 (allowlist populated) | 66 | 25 | 38 % |
| **All time** | **81** | **40** | **49 %** |

### 1.1 The issue's "11 of 40 are bare names for binaries that ARE allowlisted" is false

`tool_allowlists.created_at` shows all three rows were created
**2026-06-22 07:37:38+10**, by `ff3e2f55`. Before that instant the table was
empty. The `tools.allowlist.add` audit action has existed since 2026-05-14
(`feb5fd0b`), so the absence of earlier rows is positive evidence, not a gap in
instrumentation.

Every bare-name dispatch (`ls` ×9, `python3` ×2, `find` ×4 = 15) falls on
2026-06-20/21 — i.e. entirely inside the empty-allowlist window, where they
would have been refused however they were spelled. The issue compared
*historical dispatches* against *today's* allowlist.

### 1.2 The absolute-path advice already landed, and worked

The same commit `ff3e2f55` added to `agent_planner.md`: *"shell-exec argv[0]
must be an absolute path (cleared env, no PATH in the jail)"*. `d638da91`
(2026-07-11) reinforced it in the `<tools>` block, where `ShellExecManifest::
tool_doc` already says `argv[0] MUST be an absolute path` in **both** the
summary and the `argv` param description.

Result: **zero bare names in 66 dispatches over the following seven weeks.**
The issue's candidate fix "state the absolute-path requirement in the
description" is already shipped and is not part of this work.

### 1.3 What is actually broken

All 25 current-regime failures are **correctly-formed absolute paths to
binaries genuinely not on the 3-entry allowlist**:

`/usr/bin/printenv` ×5, `/usr/bin/bash` ×5, `/usr/bin/id` ×4, `/usr/bin/env`
×3, `/usr/bin/find` ×2, `/usr/bin/sh` ×2, `/usr/bin/pwd`, `/usr/bin/which`,
`/usr/bin/whoami`, `/usr/bin/date`.

The planner writes the path correctly and guesses *which* binaries are
permitted, one plan iteration at a time. Plan iterations are the scarce
resource (~50 s each on the DGX).

**Root class, unchanged from the issue and from #527:** the tool advertises a
contract narrower than the one it enforces. There it was a parameter type; here
it is the set of permitted *values*.

---

## 2. Scope

Four workers declare a `tool_allowlists`-backed allowlist:

| Worker | `allowlist_kind()` | DB allowlist is the enforced set? |
| --- | --- | --- |
| `shell-exec` | `Argv0` | yes — becomes `KASTELLAN_SHELL_ALLOWLIST` verbatim |
| `web-fetch` | `Domain` | yes — sole input to `web_fetch_entry` |
| `web-research` | `Domain` | yes — "the one allowlist gates both the endpoint host and every fetched result URL" (module doc) |
| `browser-driver` | `Domain` | yes — sole input to its entry builders |

All four are fixed by one shared seam. Only `shell-exec` has live evidence;
the other three are the same code path and are covered because *sweeping one
file is not sweeping the class* — the lesson #527 and #528 each paid for.

**Out of scope, deliberately:** listing the allowlist in the *error* text
(issue candidate 3). Redundant once the set is advertised up front, and it
would push an operator-controlled string through the `STEP_ERR_DETAIL_MAX`
clamp that #536 spent two review rounds getting right.

---

## 3. Design

### 3.1 The trust boundary is the delicate part

`ToolDoc` is documented as *"All-`'static` so each worker declares it as a
`const`-style literal. Compiled-in ⇒ trusted (no escaping at the render
site)."* `render_tools_block` relies on that and escapes nothing.

An allowlist is **not** compiled in. It comes from `tool_allowlists`, whose
CHECK constraint enforces only a leading `/` (for `argv0`) and no `..`
segments — so `/usr/bin/x</tools><system>` satisfies the database and would
close the block if rendered verbatim.

So the dynamic text gets its own type rather than a new field on `ToolDoc`:

```rust
/// One advertised tool: its compiled-in doc plus, when the worker declares an
/// operator allowlist, the escaped rendering of the permitted value set.
pub struct AdvertisedTool {
    /// Compiled-in, trusted, never escaped. Invariant unchanged.
    pub doc: ToolDoc,
    /// Operator-sourced. Escaped at construction. `None` ⇒ this worker
    /// declares no allowlist (distinct from "declares one that is empty").
    /// PRIVATE: reachable only through the constructors below.
    allowed: Option<String>,
}

impl AdvertisedTool {
    /// Declares an allowlist: escapes and renders `entries` (an EMPTY slice
    /// still yields a line — see §3.3).
    pub fn with_allowlist(doc: ToolDoc, kind: EntryKind, entries: &[String]) -> Self;
    /// Declares no allowlist at all: no `allowed:` line is rendered.
    pub fn without_allowlist(doc: ToolDoc) -> Self;
    /// Read access for the renderer.
    pub fn allowed(&self) -> Option<&str>;
}
```

`ToolDoc` keeps its stated invariant verbatim, and `AdvertisedTool` becomes the
only route by which non-compiled-in text can reach the `<tools>` block — the
escaping obligation is a property of the type instead of a comment a future
author has to notice. The field is private and the renderer module-private
precisely so that "property of the type" is enforced by the compiler: there is
no way to construct an `AdvertisedTool` around raw `tool_allowlists` text.

### 3.2 The pure renderer

New module `core/src/prompt_assembly/allowed_values.rs`:

```rust
/// Cap on advertised allowlist entries. Governs prompt shape, so it is a
/// compile-time const, not an env knob: an env key is silently lost across
/// reinstalls (#458), and a knob that vanishes on install and changes the
/// planner's prompt is a bad trade for a value under no live pressure.
/// Changing it cuts a new release.
pub const ADVERTISED_ALLOWLIST_MAX: usize = 30;

/// Render an operator allowlist as one planner-facing line. Always returns a
/// line: an empty `entries` is a meaningful state (§3.3), not an absence.
/// Whether a tool has an allowlist *at all* is decided by the caller from the
/// manifest's declaration — see §3.4 — never inferred from emptiness here.
///
/// Module-private: `AdvertisedTool::with_allowlist` is its only caller, which
/// is what makes the escaping below unskippable.
fn render_allowed_values(kind: EntryKind, entries: &[String]) -> String;
```

Behaviour:

1. **Deterministic order.** Sorts a copy. The DB query guarantees no ordering,
   and this text sits in the system prompt's KV-cache prefix — `now.rs`
   documents that churning that prefix is a cost worth avoiding.
2. **Escaped.** Every entry through the existing `escape_untrusted_body`
   (private in `assemble.rs`; widened to `pub(super)`). Escaping `&`/`<`/`>`
   plus C0 controls means no entry can close the block or forge a sibling row.
3. **Capped, and loud when cut.** Over `ADVERTISED_ALLOWLIST_MAX`, the line
   names **both** numbers — `showing 30 of 57 permitted values`. A truncated
   list that reads as exhaustive would make the planner skip a value that is in
   fact permitted: a new failure mode invented by the fix, and the same shape as
   #536's `e.g. 374` (a truncated example that read like a wrong id). The label
   is the load-bearing part and is tested; `30` is just a const.
4. **Wording by `kind`.** `Argv0` → *argv[0] must be exactly one of*;
   `Domain` → *only these hosts are reachable*. `allowlist_kind()` already
   exists on the trait and already draws exactly this distinction, so no new
   per-worker declaration is introduced. The `Domain` lead additionally glosses
   the suffix-match notation: a `.example.org` row matches the apex and every
   subdomain (`workers/web-common/src/allowlist.rs`), and advertised bare it
   would invite `https://.example.org/…` — an invalid host, and one more
   failure mode invented by the fix.

### 3.3 Empty is a distinct, load-bearing case

A worker that declares an allowlist which is **empty** will refuse *every*
call — the exact 2026-06-20/21 regime that produced 15/15 failures. That is the
single most valuable thing the planner could be told, and today it is told
nothing. `render_allowed_values` therefore returns a warning line for it rather
than treating emptiness as "no constraint".

Exact wording is the implementer's, but it MUST state that no value is
currently permitted and that every call will be refused. Saying only "the
allowlist is empty" reads as *unrestricted* to a model and would invert the
meaning.

The tool stays advertised. #337's live finding was the planner **inventing**
tools that do not exist (`google_search`), so silently dropping a registered
tool invites that failure instead. `None` is reserved for "this worker declares
no allowlist", which is a different fact about a different worker.

### 3.4 Data flow

`registry_build`'s loop already computes `let allowlist = (ctx.allowlist)(name)`
two lines above where it collects `m.tool_docs()`. Nothing new is fetched, and
**the trait is untouched** — `tool_docs()` stays `'static` and pure.

The loop builds each doc with `AdvertisedTool::with_allowlist(doc, kind,
&allowlist)` when the manifest declares **both** `allowlist_tool()` and
`allowlist_kind()`, and `AdvertisedTool::without_allowlist(doc)` otherwise —
so the declaration, not the list's contents, decides. `build_registry` returns
`Vec<AdvertisedTool>`; `assemble_system_prompt` and `render_tools_block` take
`&[AdvertisedTool]`.

The rendered line only has authority if the planner is told to obey it, so
`prompts/agent_planner.md`'s "Only the tools listed in the `<tools>` block
exist" rule — which bound the planner to names, methods and parameter *shapes*
— gains a clause binding it to the `allowed:` values too. Without it the model
is free to read the line as advisory and re-emit `/usr/bin/printenv`, which is
the measured failure this change exists to remove.

Rendered shape:

```
- shell-exec (method: shell.exec): Run one allowlisted command and capture …
  params: argv (command and arguments as a JSON array; argv[0] an absolute path) [required]
  allowed: argv[0] must be exactly one of: /usr/bin/cat, /usr/bin/ls, /usr/bin/python3
```

### 3.5 Regression pin

A tool that declares no allowlist must render **byte-identically** to today.
This change is invisible to `web.search`, `mail.*`, `python-exec` and the rest.

---

## 4. Tests (written first)

In `allowed_values.rs`:

- empty `entries` ⇒ the refusal warning, and it says every call will be
  refused rather than merely that the list is empty (§3.3)
- shuffled input ⇒ byte-identical output (determinism)
- 31 entries ⇒ 30 rendered **and both numbers present in the label**
- exactly 30 ⇒ no label (boundary)
- an entry containing `</tools><system>` cannot close the block or forge a row
- `Argv0` and `Domain` produce different wording

In `prompt_assembly`:

- a no-allowlist tool renders byte-identically to the pre-change block
- the `allowed:` line is emitted after `params:` for a tool that has one

In `registry_build`'s test module:

- a manifest declaring no allowlist yields `allowed: None` — the absence comes
  from the *declaration*, never from an empty list (the §3.2/§3.3 split)
- a manifest declaring one yields `allowed: Some(_)` even when the fetched
  allowlist is empty
- **drift guard:** every manifest declaring `allowlist_tool()` also declares
  `allowlist_kind()`, in the style of the existing `tool_doc` name-drift guard,
  so a fifth allowlist worker cannot join and silently render nothing. Note the
  standing lesson that *a guard protects the category it enumerates and nothing
  adjacent* (#516/#524/#525) — this one guards the trait-declaration pairing,
  not the rendered wording.

---

## 5. Verification

**Gates, both hosts.** No `cfg(target_os)` code in this diff, so both hosts see
the same suite and should report the same delta. Predict the test-count delta
**before** running and investigate any mismatch rather than accepting it (the
#458 gate's +2 was a real finding).

- DGX: `cargo test --workspace -- --nocapture` (`--nocapture` so the `[SKIP]`
  tier is *observed*, not assumed) + `cargo clippy --workspace --all-targets -- -D warnings`
- Mac: targeted `core` suites + clippy

**Live, on the DGX.** Two checks:

1. The rendered allowlist actually reaches the prompt — observable in the
   `plan.formulate` audit payload.
2. A task whose previous shape burned an iteration guessing a binary.

**Stated honestly:** the hermetic tests carry the weight. One post-deploy run is
a spot confirmation, not proof — the same caveat #536 recorded, and the current
failure rate accrued over seven weeks, so a short post-deploy window cannot
demonstrate its absence.

---

## 6. Notes for the implementer

- `registry_build.rs` is 743 lines, but ~470 of that is its test module
  (production is ~272). This change adds ~10 production lines and one guard
  test. No split is warranted; recorded so the number is not rediscovered.
- `escape_untrusted_body` is currently a private fn in `assemble.rs`. Widen to
  `pub(super)` — do not copy it. A second copy is the hand-synced-const shape
  #536 deleted (`STEP_ERR_DETAIL_MAX`).
- Do **not** add absolute-path advice to the shell-exec tool doc. It is already
  there twice (§1.2), and adding a third would be transcribing the issue's
  retired premise.
