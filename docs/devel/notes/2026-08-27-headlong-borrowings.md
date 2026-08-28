# Cross-project study — what kastellan should take from **Headlong**

**Date:** 2026-08-27
**Status:** Investigation / design input (no code change in this note)
**Question:** [laude-institute/headlong](https://github.com/laude-institute/headlong)
is an agent microharness built around *persistent agency*. Which of its ideas
transfer to kastellan, and which are incompatible with our threat model?

> Source: `laude-institute/headlong` @ `32192c79` (2026-08-26), **Apache-2.0**
> (AGPL-compatible, so nothing here is licence-blocked even if we lifted code —
> which we do not propose to do). Read: `README.md`, `philosophy.md`,
> `deploy/SECURITY.md`, and `design/{trajectory_spec,tiered_memory,context_assembly,
> unified_progressive_resolution_memory,THINKERS_spec,monolith_backoff}.md`.
> Quotations below are from those files.

---

## 1. What Headlong is

A complete agent harness in **<10K lines of Bash**. Its core loop (`shellm`) is a
recursive-language-model implementation: *"The agent thinks by writing shell
commands, running them, and reading the output. No tool system besides Bash is
needed."* Its defining property is that the agent never stops — a human message
*"doesn't start a session. It lands in the agent's thought stream as one more
observation, and the agent decides if and when to respond."*

The pieces: `shellm` (the loop), `traj` (append-only JSONL trajectory DAG with
fork/merge), `context` (trajectory → LLM messages, with tiered compaction),
`thinkers` (a dispatcher running reactive thought processes off the trajectory as
a bus), `mem`/`skills` (file-backed memory and SOPs).

## 2. Why most of it does not transfer

Headlong's threat model is the **inverse** of ours, stated plainly in
`deploy/SECURITY.md`:

> *"The identity is an agent that runs arbitrary bash on its box with its API
> keys, and every message a person sends goes straight into the agent's context.
> Prompt injection is therefore always possible, from any channel… The box is
> dedicated and burnable… because it allows all outbound traffic and an injected
> agent could send those secrets out… Do not tell it secrets you would not post
> in a public channel."*

Against `CLAUDE.md`'s hard constraints:

| Headlong idea | Verdict for kastellan | Why |
| --- | --- | --- |
| **Bash as the only tool; the model writes and runs shell** | ❌ **Reject** | Directly contradicts "Rust core, no eval, no metaprogramming, no dynamic import" and "every worker is sandboxed before it runs". This is the exact capability our architecture exists to deny. |
| **Self-improvement by fork / test / merge of its own codebase** (*"we have pulled over 50 of its commits back into main"*) | ❌ **Reject** | An agent with commit access to its own harness voids the threat-model invariant: worst-case compromise must reach at most the agent's own user, role and scratch FS — not the code that enforces those bounds on the next boot. |
| **One shared mind, no per-user sessions** (*"assume anything you tell the agent is shared with everyone who talks to it"*) | ❌ **Reject** | kastellan is single-user by design, and the `DataClass` / data-ceiling machinery is the opposite bet: classification floors exist precisely to stop content crossing contexts. |
| **Docker-by-default sandboxing** | ❌ **Superseded** | Weaker than bwrap + Landlock + seccomp double containment (no second layer installed by the worker on itself), and it makes a privileged external daemon a hard dependency of the security boundary. Their own installer concedes the fallback: without Docker, *"the commands would run directly on your machine as you."* |
| **`shellm-docker-broker`** — host-side policy server, *"never present in the mind's environment"* | ✅ **Already have it, stronger** | This is `tool_host::dispatch()` — the dispatcher chokepoint — arrived at independently. Worth noting only as convergent evidence that the chokepoint is the right shape. |

Everything worth taking is in the **memory / context / loop-pacing** layer, where
Headlong has been running a real agent continuously for months and has hit
failure modes we have not yet reached.

---

## 3. Borrowing #1 — step identity as a *schema* invariant, for `audit_log`

**Filed as [#628](https://github.com/hherb/kastellan/issues/628). Highest value, lowest risk. Recommend doing this first.**

### Where we are

`audit_log` is `(id, ts, actor, action, payload JSONB)` (migration `0001_init.sql`).
`task_id` rides *inside* `payload` by convention — 58 occurrences across 19 files
under `core/src` — and nothing enforces its presence, its name, or its type. There
is no plan-iteration or step grouping at all: the rows of one plan iteration are
distinguishable only by `ts` ordering and by reading the payloads.

### What Headlong does

`design/trajectory_spec.md` promotes exactly this to a guaranteed property of the
format, with a rule worth pasting into `db/src/audit.rs` verbatim:

> *"These are guaranteed invariants of the format (**writers stamp exact links;
> readers must not guess**)."*

The invariants themselves:

- every step carries a `step_id`;
- every machinery step of a run carries `run_id` = the run header's `step_id`
  — *"`run_id` — not file position — is what ties a step to its run"*, because
  many runs interleave in one file;
- a dispatch edge is explicit: `trigger_step` names the step that caused this
  run, `launched_by` names who launched it;
- causal links are stamped at the **transport**, not by the code path that
  happens to be running. On `reply_to` they say so directly: *"Stamping at the
  transport means the fact does not depend on which code path — or which model —
  sent the reply."*

### Why this is a fix we have already made twice

HANDOVER records #616's root problem exactly:

> *"a request timeout, a refused connection, an HTTP status and a decode failure
> were one string, so the fail-open #612 is entirely about **could not be
> counted** — only inferred, by correlating `router_error` rows against a large
> `body_byte_len` and an `ms` near the budget, across a rotating log."*

The fix there was to make one question an equality query (`guard.error_kind`).
The same shape recurred in #619 (`basis` bands: `= 'operator'` silently omits
out-of-band hosts; use `LIKE 'operator%'`). Both are instances of the general
defect Headlong's spec closes structurally: **the audit log answers questions by
correlation rather than by equality, because the causal structure lives in prose
and convention rather than in columns.**

### Proposed shape

1. Promote `task_id BIGINT NULL` to a real column on `audit_log`, with an index,
   backfilled from `payload->>'task_id'` where present.
2. Add a grouping key for one plan iteration (`plan_seq INT NULL`, or a generic
   `caused_by BIGINT NULL` referencing `audit_log.id`). `caused_by` is the more
   Headlong-shaped choice — it is `trigger_step` — and it generalises past the
   scheduler to channel ingress and the egress proxy.
3. Write the reader rule down as policy, not folklore, and pin it with a test.

### Migration posture — take their wording too

We have a live DGX with existing rows, so the backfill is partial by
construction. Headlong states the reader contract for exactly this case:

> *"Logs written before 2026-07-10 predate these fields. Readers must tolerate
> their absence (render such steps as a plain ungrouped stream), **never
> reconstruct membership heuristically**."*

That is the rule to adopt before the migration lands, not after. A heuristic
regrouping of pre-migration rows would manufacture exactly the false causal story
the change exists to prevent.

### Bonus

Phase 5's audit UI gets a tree to render for free, instead of a flat list plus
client-side payload sniffing.

---

## 4. Borrowing #2 — tiered context compaction, to fill `MemoryLayer::L4`

**Filed as [#629](https://github.com/hherb/kastellan/issues/629). Biggest capability gain; the slot is already declared and empty.**

### Where we are

`core/src/prompt_assembly/assemble.rs` builds, in order:

```text
now → l0_meta_rules → l1_insights → skills → recalled → tools → handoff → base
```

There is **no episodic block at all**. A task started today knows nothing about
what happened yesterday unless something explicitly wrote an L2 row. Within one
task, `TaskContext.plans` carries a summary across replanning iterations, and
that is the entire horizon.

Meanwhile `db/src/memories.rs` already declares:

```rust
/// L4 — session digests. Reserved; no writer in the slice that
/// introduced this enum.
```

So the layer exists, the DB accepts it, and nothing writes it. Headlong has spent
months designing the thing that goes in that slot.

### The mechanism (`design/tiered_memory.md`)

A logarithmic pyramid of summaries with fanout `F` (default 10): tier *k* has one
entry per `F^k` steps, so a life of *N* steps needs `⌈log_F N⌉` tiers — *"7 tiers
covers a million steps at F=10"*. Higher tiers are rolled up **from the tier
below, not re-summarised from raw**, which is what makes the work exponentially
cheap. Built lazily: a tier-*k* block seals when the log crosses `F^k`.

Crucially, it is built **forward-only from a start marker**, so enabling it on an
existing long log does not trigger a synchronous historical rebuild:

> *"On first use, a trajectory records a start marker (`rollups/meta.json`
> `start_index`)… So tiered memory works going forward from enablement with no
> synchronous historical build."*

### The two disciplines that make it safe for *our* posture

These are the reason this is worth copying rather than reinventing:

> *"**The raw log is the source of truth; the tiers are an index, not
> testimony.** A summary of a summary is a rumor — at the coarse tiers you're
> reading the model's paraphrase of its paraphrase, several removes from anything
> that actually happened. So the cited step-ids aren't a nicety, they're the
> point: treat a coarse entry as a *pointer* to where something happened and
> roughly what, distrust its exact wording, and drill to the raw steps whenever a
> span genuinely bears on the current decision."*

> *"the system must still work (more slowly) if you delete every rollup and keep
> only the log."*

And the framing:

> *"**This is a paging problem, not prompt formatting.** The context window is
> fast memory, the trajectory on disk is the backing store, and summaries are
> compressed pages of the past. The load-bearing core is small — **recency plus
> fetch-by-id**… The tiers are the *optimization* on top of that core."*

### Mapping onto kastellan

| Headlong | kastellan |
| --- | --- |
| `trajectory.jsonl` (raw, append-only) | `audit_log` — already append-only *by GRANT*, not merely by convention |
| cited `step_id`s in each rollup | cited `audit_log.id` ranges (needs §3's `task_id` column to be cheap) |
| tier-1..k rollup files | `memories` rows at `MemoryLayer::L4`, one per sealed block, `level` in the row |
| `recap --context` staircase | a new `<history>` block in `prompt_assembly::assemble` |

Three consequences of that mapping that matter for our threat model:

1. **Digests are derived, discardable and rebuildable.** A compromised or
   hallucinating summariser degrades recall; it destroys no evidence, because the
   append-only GRANT on `audit_log` is what holds the record.
2. **The `<history>` block is model-authored, therefore untrusted.** It escapes
   via `escape_untrusted_body`, exactly like `<recalled>` and `<l1_insights>` —
   *not* verbatim like `<l0_meta_rules>` and `<skills>`, which are operator-gated.
3. **Drill-down is a tool call, not a prompt trick.** "Pull audit rows `N..M`" is
   a dispatcher-mediated read of our own log, so it inherits the chokepoint.

### Related, deferred

`design/unified_progressive_resolution_memory.md` goes further: one envelope for
episodic *and* semantic memory with `level` / `children` / `parent` links, so
recall becomes an HNSW-style descent (*"~4 LLM calls for 10,000 memories"*)
instead of a flat scan. That is a genuinely interesting answer to how L2 recall
scales, but it is a redesign of the memory model, whereas L4 digests are purely
additive. Note it; do not start there. Their own status line reads *"NOT YET
IMPLEMENTED — design only"*.

---

## 5. Borrowing #3 — blob spilling instead of fingerprint-and-discard

`db/src/audit.rs`'s `truncate_payload` caps a serialised payload at 4 KiB and
replaces the oversize value with a SHA-256 fingerprint envelope. The bytes are
gone. HANDOVER records the live cost:

> *"Dropping it was measured live: on 2026-08-23 two 85 KB `web.fetch` rows took
> the guard tier's score down with them."*

`PRESERVED_KEYS` patches the specific loss; the general one stands — an operator
cannot recover *what the guard actually judged*.

Headlong spills instead of dropping (`design/trajectory_spec.md`, "Blob
spilling"): over-limit `stdout`/`stderr` go to `blobs/<step_id>-<blob_id>.stdout`
and the step keeps `stdout_ref` + `stdout_bytes` + `stdout_truncated`, so
`traj show <id> --full` restores the original.

We already have the analogous machinery one layer up — the reserved
`handoff`/`fetch` built-in that stashes an oversized *tool result* — it simply is
not wired to the audit path.

### Reconciling this with [#617](https://github.com/hherb/kastellan/issues/617)

#617 covers the same loss for `shell.exec` (*"`req.argv` **is** the act being
audited"*) and deliberately rejects preserving the body:

> *"`req` fails `PRESERVED_KEYS`' first admission criterion outright: it is
> unbounded by construction, and preserving a body under another name is exactly
> what the cap exists to prevent."*

That objection is about what rides **inside the JSONB payload**, and it is
correct there. A spill is not that: the bytes leave the payload entirely and the
row keeps only a bounded reference (`ref`, `bytes`, `sha256`). The cap on the
row, the table and the WAL is untouched; what changes is that the bytes are
*recoverable* rather than *destroyed*.

So the two are complementary, not competing, and #617's `req_summary` should ship
first: it is bounded by construction, always present, and answers "what ran"
with no new storage surface. The spill is the optional second half that answers
"give me the exact bytes" — and being derived and prunable, it can be dropped by
retention policy without touching the audit record.

**Caveat, and it is real:** spilling relocates untrusted content to a filesystem
carrying none of `audit_log`'s append-only GRANT, and it re-creates a retention
surface the cap partly existed to avoid. Two mitigations: the SHA-256 stays in
the row, which is what makes the blob tamper-evident rather than merely large;
and the spill directory lives under the agent's own scratch FS, inside the
existing boundary. If the retention cost is judged to outweigh the forensic gain,
ship #617's summary alone and stop there — that is a legitimate outcome of this
comparison, not a failure of it.

---

## 6. Borrowing #4 — idle-backoff pacing (for whenever routines land)

Not the "never sleeps" philosophy — the **pacing policy** in
`design/monolith_backoff.md`, which we would want the day scheduled routines
start driving the loop. Two properties held simultaneously:

1. **Reactivity is never throttled.** *"A message addressed to us is answered
   immediately, on the full model, no matter how deep the idle."*
2. **Spontaneity backs off geometrically, with a dwell.**
   `delay(0) = 0`; `delay(n≥1) = min(BASE · FACTOR^(n-1), CAP)`, holding `HOLD`
   empty wakes at each level before stepping slower:
   `0,0,0 → 5,5,5 → 10,10,10 → 20,20,20 → … → cap`.

Three bugs they have already paid for, which we would otherwise pay for again:

- **The wait must not occupy the worker slot.** Their first version slept *inside*
  the step, which held the dispatch slot and pinned the cap near 60 s. *"Merely
  polling during the wait… is not the same as a free slot."*
- **The timer must be dispatcher-native.** Their `setsid` background-timer
  implementation *"silently never ran on macOS (no setsid), so spontaneity died
  until the first reactive wake."* That is our cross-platform-parity constraint
  catching a real bug in someone else's tree — and a reminder that the failure
  mode of a missing platform counterpart is *silence*, not an error.
- **Thinking is not working.** 2026-08-24 revision: a thought-only run stopped
  counting as engagement, because *"writing 'nothing changed' had counted as
  work, so a ruminating mind re-fired at full speed forever."*

---

## 7. Borrowing #5 — liveness as a *dispatcher guarantee*

`design/THINKERS_spec.md`, "Liveness Watchdog". Their loop died live on
2026-08-04: a step consumed its own trigger with a bare `exit 0`, leaving no wake
source, and *"the mind slept until manually restarted."* The fix is not "audit
every code path":

> *"Liveness is therefore a **dispatcher guarantee**, not a property thinker code
> paths must each preserve."*

The dispatcher synthesises an idle trigger for any subscriber that has been free
and quiet for a window; busy-or-queued refreshes the clock, so a long legitimate
run is never interrupted. Result: *"every liveness bug, present or future,
degrades to a ≤window wake-up delay instead of a dead mind."*

**This applies to us now, and we have already made the argument once.**
`scheduler::crash_recovery::sweep_and_audit` is **startup-only** —
`core/src/main.rs:232` is its sole non-test call site, flipping lease-elapsed
`running` rows to `crashed` at bring-up. A task wedged in-process while the daemon
is alive and healthy (a hung worker, an LLM call outliving its budget) is caught
only at the next restart. We cover *died*; we do not cover *stalled-but-alive*.

The precedent is in the same crate. `scheduler::runner`'s `ASK_SWEEP_INTERVAL`
doc-comment reasons its way to exactly Headlong's conclusion for the *ask*
deadline:

> *"Slice 1a's spec put this at daemon startup only. On a daemon that runs for
> weeks that is not a deadline: an unanswered ask holds its task in
> `awaiting_operator` until the next restart, which is the permanent wedge the
> deadline exists to prevent."*

So `sweep_loop` already exists — a lane-independent tokio task ticking every 60 s,
with a shutdown watch and a keep-going-on-error policy — and it already sweeps one
pool-wide predicate. Adding `crash_recovery::sweep_and_audit` beside
`asks::sweep_expired_and_audit` in that loop is close to free, and closes the
identical wedge for leases that the ask sweep closed for deadlines.

Two things to get right if we do it: `task.crashed`'s `audit_log.ts` already
carries the *"detection time, not crash time"* caveat documented in
`crash_recovery.rs`, and a 60 s sweep narrows that gap from hours to a minute —
which improves the data but does not remove the caveat. And the lease predicate
must stay the thing that decides, not a wall-clock heuristic; Headlong's
equivalent rule is that busy-or-queued refreshes the clock so *"a long agentic run
never triggers a spurious wake."*

---

## 8. Two cheap ones

- **`tools/pr-committee`** — multi-model PR review, run on their own repo. We do
  five-reviewer passes by hand (see the #619 and #614 retrospectives); they have
  automated the same shape. Worth a look before we build our own.
- **`<name> bugreport`** — one command bundling logs + trajectory with keys
  scrubbed. We already have `leak-scan` as a shared crate, so a
  `kastellan-cli bugreport` that runs it over an audit slice plus the supervisor
  journal is close to free — and it is exactly the tool nobody writes until they
  need it at 2 a.m.

---

## 9. Recommendation

| # | Item | Cost | Do when |
| --- | --- | --- | --- |
| §3 · [#628](https://github.com/hherb/kastellan/issues/628) | `task_id` / `caused_by` columns on `audit_log` | one migration + backfill + reader rule | **first** — generalises a fix already made twice, unblocks the Phase 5 audit UI |
| §4 · [#629](https://github.com/hherb/kastellan/issues/629) | L4 session digests + `<history>` block | a writer, a sealing rule, one assembler block | **next** — purely additive; the layer is already declared |
| §5 | audit blob spill | small; reuses the handoff-stash idea | **after [#617](https://github.com/hherb/kastellan/issues/617)** — its bounded `req_summary` is the load-bearing half; the spill is optional |
| §7 | move the crash sweep into the existing `sweep_loop` | a few lines — the loop already exists | **now** — live gap, and the precedent is in the same file |
| §6 | backoff table + dwell | small | with the routines slice |
| §8 | pr-committee / bugreport | small | whenever |

Explicitly **not** adopted: shell-as-tool, agent self-modification, the shared
multi-user mind, Docker-as-sandbox. Those are the parts of Headlong that make it
what it is, and they are the parts kastellan exists to refuse.
