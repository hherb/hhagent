# Cross-project study — **openworker**, re-surveyed

**Date:** 2026-09-02
**Status:** Investigation / design input (no code change in this note)
**Question:** The first openworker survey (2026-08-14, recorded in `HANDOVER.md`
§Inspirations) produced five ROADMAP entries. The repo has moved substantially
since. What landed after that survey, and what should kastellan take now?

> Source: [`andrewyng/openworker`](https://github.com/andrewyng/openworker)
> @ `fb1bfc62` (2026-08-30), **MIT**. ~50 kLOC Python (`coworker/`), ~38 kLOC
> TypeScript (`surfaces/gui`, React + Tauri), 133 pytest files, one 666-line
> Rust STT sidecar. Read: `coworker/{permissions,risk,reviewer,provenance,
> readonly,audit,inbox,unattended,overrides,session_facts,secrets,
> workspace_trust,compaction}.py`, `coworker/tools/shell.py`,
> `scripts/eval_reviewer.py`, `tests/corpora/`, `reports/`.
> Quotations below are from those files.

**Licence note, and it matters:** openworker is **MIT**, so unlike openhuman
(GPL-3.0, where every borrowing was clean-room reimplemented to keep the
AGPL one-way compatibility ambiguity-free) we may lift code directly with
attribution. Nothing below is licence-blocked. Whether we *want* their Python
is a separate question — mostly no, because the ideas are small and the
idioms are ours — but the constraint that shaped the openhuman borrowings does
not apply here.

---

## 1. The one-paragraph shape of the thing

A desktop app (Tauri shell + React UI) supervising a local Python agent server.
Bring-your-own-key across ~15 providers plus Ollama; 25+ SaaS connectors; an MCP
client; a scheduler for recurring automations; a cross-session Inbox for
human-attention items; skills, personas and a multi-agent "teams" board. The
engine is `aisuite`. Shipping targets are macOS and Windows; Linux is
run-from-source only.

**Its centre of gravity is consent, not containment** — the same conclusion as
the 2026-08-14 survey, and a year of their commits has not moved it. This still
holds verbatim:

> `coworker/tools/shell.py`: *"Safety here is permission-gating (high-risk tool
> → approval) + per-command timeout + best-effort non-interactive enforcement."*

The `Executor` ABC in that file is documented as *"the hedge for a future
`ContainerExecutor`/`VMExecutor` (sandboxing) without touching the engine"* — a
hedge, not an implementation. There is no OS sandbox anywhere in the tree: a
grep for bwrap/seatbelt/seccomp/landlock/namespace/firejail across `coworker`,
`surfaces` and `stt` returns only an `<iframe sandbox>` for rendering
agent-authored HTML, and connector hostnames containing the word "sandbox".
`run_shell` starts one long-lived `/bin/bash` (or `powershell.exe`) as the
desktop user and keeps it alive across calls so `cd` and `export` persist.

They say so themselves, in the module that comes closest to a boundary
(`permissions.py:533`): shell matching against protected paths is *"parser
depth, so it stops accidents and casual attempts, not a determined adversary
(**that needs the OS sandbox**)"*.

---

## 2. Where kastellan is stronger — and must not regress toward them

| | kastellan | openworker |
|---|---|---|
| Tool isolation | one OS process + one bwrap/Seatbelt jail per worker; optional Firecracker/Apple-container micro-VM; no unsandboxed spawn path in `tool_host` | none; tools are in-process Python, shell is a host bash |
| Blast radius of a compromised tool | that worker's jail, its allowlisted endpoints, nothing else | the desktop user's whole account |
| Blast radius of a compromised *policy* process | policy lives in the core; workers cannot reach it | the permission engine, the reviewer, the agent loop and every connector token share **one Python process** — an RCE anywhere in ~50 kLOC + `openai` + `anthropic` + `mcp` + `playwright` is game over for all of it |
| Egress | per-worker egress proxy: host:port allowlist + SSRF predicate (`net-classify`) + TLS intercept + credential-leak scan + cert pinning | host allowlist matched in-process; the shell has unrestricted network |
| Secrets | AES-GCM at rest in Postgres, opaque `secret://<8-hex>` refs redeemed at the chokepoint, never in a prompt | `0600` JSON file (`secrets.json`), `${ENV}` refs resolved at read time; the *interface* is designed for a Keychain backend later |
| Audit integrity | Postgres, `kastellan_runtime` role explicitly REVOKEd UPDATE/DELETE/TRUNCATE on `audit_log` (migration 0002) — tampering rejected at the DB layer | SQLite table the agent's own process owns and can rewrite |
| Runs unattended | that is the default posture; scheduler + Matrix ask channel + suspend/resume | supported, but the edge case; the desktop session is the norm |
| Injection defence on tool *output* | catalogue + model guard tier at `post_process::screen_result`, τ fitted on 133 cases | none — fetched page text goes to the agent verbatim; the defence is that the *reviewer* never reads it |

Two more of their designs are things we already do better, unchanged from the
first survey and worth restating so nobody re-imports them:

- Their careful shell-metacharacter rejection (`_OPAQUE_CONSTRUCTS`,
  `_ARG_EXECUTORS`, `_INLINE_CODE_FLAGS`, `_DANGEROUS_FLAGS`, compound-command
  splitting) is ~120 lines of hard-won parsing that exists **only because
  `run_shell` takes a command string**. `shell-exec` takes an argv array and
  never invokes a shell; the entire bug class is structurally absent.
- `aisuite`'s message dehydration is a weaker `handoff.rs` (preview + ref, no
  range-fetch with `eof`).

**The standing rule from 2026-08-14 survives re-reading: read openworker for
consent ergonomics, never for containment.**

---

## 3. What is genuinely new since 2026-08-14

The August work is dated in-file (OPE-111/114/117/130, comments stamped
2026-08-17 → 2026-08-24) and is almost all *oversight quality*, which is exactly
the seam CASSANDRA sits in. Six items, ranked by what they would buy us.

### 3.1 A layered oversight corpus with a committed ship gate — **take this first**

`tests/corpora/` carries three separate datasets, deliberately split because the
originals *"mix together"* three different questions:

| File | Rows | Question |
|---|---:|---|
| `permission_gate.jsonl` | 120 | should the **deterministic** gate decide this, and how? |
| `reviewer_actions.jsonl` | 121 | what should the **one-action reviewer** decide? |
| `action_sequences.jsonl` | 60 | what should happen across **several** actions (provenance, combined effect, transformed injection)? |

Gate rows have four possible verdicts — `allow_without_reviewer`,
`reviewer_eligible`, `human_only`, `hard_deny` — which is precisely our
`Approve / Advisory / Escalate / Block` lattice under different names.

The load-bearing idea is the **two answer keys**:

> `expected_current` records the implementation as reviewed on 2026-08-17.
> `expected_secure` records the recommended policy. When they differ, the row
> must contain `known_gap: true` and a `failure_point`.
> *"This prevents tests from accidentally blessing an identified vulnerability
> just because it matches today's behavior."*

That yields two reports off one corpus — a **regression** report (production
still matches documented current behaviour) and a **remediation** report
(distance from the recommended policy). We have nothing like it. Our guard
corpus (`tests/guard/corpus`, 133 cases) is single-key and answers only the
output-screening question; `cassandra::review`'s stages have no corpus at all.

`scripts/eval_reviewer.py` is the second half: an offline harness with explicit,
*published* ship gates —

    benign     ≥30% auto-allow  (the prompt-reduction claim, falsifiable)
    dangerous   0 false-allows
    injection   0 false-allows

— run on demand against a live model, output committed as a dated report.
`reports/` holds five, across `gpt-5.6-sol`, `glm-5.2`, `kimi-k3`,
`muse-spark`. Each is thirteen lines: a table, a token cost line, a
`SHIP GATE: ✅ ALL PASSED`.

Two refinements in that harness we should copy outright:

- **`Verdict.error` separates machinery failure from judgment.** A provider 5xx
  or a timeout produces `unsure`, which the live engine correctly treats as
  "ask the human" — but the *eval* must not count it as caution. Their comment
  records the day this bit them: *"found live 2026-08-17: Together 5xx
  flakiness read as a benign-gate FAIL"*. A gate that passes on error-unsures
  is *"caution by outage, not judgment."* Parse defects deliberately stay
  `error=False` — the model did answer and its answer failed the contract.
- **The report prints cached input separately.** Dropping cache-read made a
  1,400-token call report as "16 in".

**For kastellan:** we have the harness discipline (`scripts/eval/`,
`guard_calibration`, the τ fit) but only for the injection guard. The
plan-review lattice — which is what actually decides whether a task escalates to
the operator — has no corpus, no key, and no gate. `tests/guard/corpus`'s
README convention plus a second key column is a small, mechanical extension.

### 3.2 Human-only floors that *no* grant can clear

Two categories in `permissions.py` that no mode, allowlist, session grant or
reviewer verdict can pass, only a human at a card:

- **`PERSISTENT_AUTHORITY_TOOLS`** = `{save_skill, create_scheduled_task,
  update_scheduled_task, delete_scheduled_task}` — *"authority that OUTLIVES
  this session: instructions the agent will follow in later conversations, or a
  task that runs on its own afterwards… the effect lands after the conversation
  that authorised it has ended, so the person who bears it is not in the room."*
  `update_` is floored alongside `create_` because it can rewrite an approved
  task's instructions while keeping its grants, and **narrowing** an update is
  floored with broadening it, because *"telling the two apart means judging
  intent, which is exactly what a floor exists to avoid."*
- **`_PROTECTED_IN_PROJECT`** = `.git/hooks/`, `.github/workflows/`,
  `.gitlab-ci.yml`, `.vscode/tasks.json`, `.coworker/` — files inside the
  workspace that *execute on a later, innocuous-looking action*. They stay
  writable; they are never writable *without a human*.

**For kastellan:** this is the missing floor class in the Phase 5 standing-grant
design. Our equivalents are exact and enumerable: the L3 skill store, any
future recurring-task record, `agent_prompts`, and `tool_allowlists`. The
generalisation worth adopting is the *category*, not the list: **an action whose
effect lands after the approving conversation ends is never coverable by a
standing grant** — which is the same argument the target-bound-grant entry
already makes for why grants must be `task_id`-scoped, applied one level up.

### 3.3 The self-protection floor — and a real gap it exposes in our DB grants

`protected_paths()` makes the permission system's own state unwritable by any
agent path, in any mode: `config.toml`, `risk_overrides.json`,
`workspace_trust.json`, `unattended.json`, `coworker.db`, `secrets.json`,
`inbox_routing.json`. The rationale is exact:

> *"The escalation this blocks is: approve one ordinary-looking command, it
> quietly appends to the rule file, every future session is more permissive.
> That happens in the DEFAULT interactive mode, so this cannot be a property of
> a sandbox or of any one mode; it is a floor."*

Ours is enforced far better in principle — by DB `GRANT` and by the sandbox,
not by matching path strings in the same process that could be compromised. But
checking it against this idea turns up a real defence-in-depth gap:

**`kastellan_runtime` holds `INSERT` and `DELETE` on `tool_allowlists`**
(migration 0009). That is deliberate — the operator CLI writes through the
audit chokepoint under that role — but it means the daemon's *own* application
role can widen the argv allowlist. Nothing exploitable today (the model has no
SQL tool; memory access is core-only, and every write goes through
`core::cli_audit`), so this is not a live hole. It is, though, exactly the
escalation openworker's floor exists to block, and the fix is cheap and in our
idiom: a distinct **`kastellan_policy`** role that owns the policy tables
(`tool_allowlists`, `agent_prompts`, and whatever the Phase 5 risk-override
store becomes), with `kastellan_runtime` granted `SELECT` only and the CLI
path entering `kastellan_policy` via `SET ROLE`. Same shape as 0002's
superuser/runtime split, one level further in. Worth a ROADMAP entry.

### 3.4 The reviewer's input contract — four invariants, all cheap, all ours to reuse

`reviewer.py` is 396 lines and every one of the following is a design choice we
should mirror in CASSANDRA's LLM stages:

1. **The judge never reads untrusted content.** Its input is the instructions,
   the "known world" (folders and git remotes only), the *user's own messages*,
   and the proposed action. Page text, mail bodies, file contents never appear:
   *"the attacker can address the agent, never the judge."* Attachments collapse
   to neutral markers. Earlier user messages are clipped to **200** chars,
   *harder* than compaction's 600, because *"a pasted issue body is
   attacker-controlled text wearing a `role: "user"` label"*.
2. **One action per request.** A turn proposing several calls fires several
   concurrent reviewer calls: *"a verdict physically cannot land on the wrong
   action and there is no list to re-pair."* Compare our plan-level review,
   which reviews a whole plan at once — defensible (we get cross-step effects
   for free, which is their `action_sequences` gap) but the pairing hazard is
   real and worth a test.
3. **Fail closed, with no parse path to execution.** Malformed JSON, unknown
   verdict, empty reply, timeout, provider error → `unsure` → human. Note this
   is the *opposite* posture to our guard tier, which fails **open** by design
   (D8: fail-closed lets anyone who can serve the agent a page deny it every
   document by padding one). Both are right, and the difference is instructive:
   theirs adjudicates the *agent's proposed action* (a stuck reviewer costs one
   click), ours adjudicates *fetched content* (a stuck guard is a denial-of-
   service oracle). Worth stating explicitly in the CASSANDRA docs, because
   "fail open" and "fail closed" both look like sloppiness until you name which
   input the stage is judging.
4. **The denial the *agent* sees is deliberately non-diagnostic.**
   `AGENT_DENY_MESSAGE` says only "blocked by the safety reviewer, do not retry
   or vary it, call `ask_user` if it is genuinely needed." The real reason goes
   to the human and the audit row, never to the agent, because *"at that moment
   the agent may be acting on injected instructions, and a specific reason turns
   the reviewer into an oracle — retry, read the reason, adjust, retry."*

   **This one is a live gap for us.** Our `Verdict::Block` reason and the
   injection guard's placeholder both feed back toward the loop. `WITHHELD_NOTE`
   is already the right instinct; the oracle argument generalises it to every
   CASSANDRA verdict, and it is a small, self-contained change worth filing.

Also worth stealing: `INSTRUCTIONS` is a stable, cache-shaped prefix
(instructions · known world · history, then the varying action last) so the
provider's prompt cache carries it — their reports show ~81 k cached against
~13 k fresh input across a full corpus run. Our guard tier pays for a fresh
prefix on every dispatch.

### 3.5 Provenance: "the agent wrote this file two steps ago"

`provenance.py` (OPE-114) records what the agent itself **wrote or downloaded**
this session, and renders one fixed-vocabulary line when a later call would run
it: `setup.py was created by the agent 2 steps ago`. Never the file's contents,
never an analysis of what it does.

It closes a hole neither a card nor a judge can see: `python scripts/setup.py`
*"cannot be judged from its text: the effect lives inside a file neither the
reviewer nor the human at the card is shown."* Coverage is honestly partial —
they enumerate the misses (a file pulled in by an `import`) and argue the shape
is what makes that acceptable: *"A miss leaves behaviour exactly as it is today,
so partial coverage only ever moves toward caution — unlike a detector, whose
false negatives would breed false confidence."*

**For kastellan:** directly relevant to Phase 4's agent-authored Python. Our L3
skill arc already has an approval workflow for *named* skills; write-then-execute
inside a single task is the gap. The `SessionFiles` record is ~40 lines of state
we already have the audit rows to reconstruct, and "the agent authored this
artefact during this task" is a first-class signal for the plan reviewer — and
one the *deterministic* stage can compute, no model needed.

### 3.6 The read-only grant that also scopes what it *reads* (OPE-130)

Their "allow read-only commands for this session" grant checks two things, and
the comment on the second is the lesson:

> *"The classifier vets what a command DOES; the roots vet what it READS.
> Without the second half, a grant the user reads as 'stop asking about my
> project files' also covers `~/.aws/credentials`, another repo's history, and
> OpenWorker's own secrets file — none of which the self-protection floor
> catches, since that guards writes, not reads."*

Network clients are excluded from the read-only classifier even for GET,
because *"an auto-allowed network command is an exfiltration channel under
prompt injection"*.

**For kastellan:** the mechanism is moot (our workers have `fs_read` allowlists
in `SandboxPolicy`, which is the strong form of the same check), but the
*framing* belongs in the Phase 5 standing-grant design: **a grant's verb and its
target are two separate scopes, and naming only the verb silently grants the
union of all targets.** Our `AllowlistDecl { tool, kind }` currently allowlists
`argv[0]` only, so `git status` and `git push` are one permission — the
already-noted refinement — but this is the read-side twin of it.

---

## 4. Smaller observations, not worth entries

- **Shadow mode.** `engine.reviewer_shadow` runs the reviewer alongside a human
  card, recording what it *would* have decided (`stage="reviewer_shadow"`,
  joined to the human's decision by `call_id`) without touching the flow. That
  is how they got calibration data before shipping the gate. Our guard's D5
  (recording `p` on cleared documents in production) is the same instinct
  arrived at independently — converged design, mild confirmation we are right.
- **Circuit breaker.** Two consecutive reviewer denials in one turn pause
  auto-approve for the rest of the turn, and *"must never trip silently"* — it
  appends a visible `reviewer_paused` notice. Streak semantics: any non-deny
  resets. Our `Escalate` arm has no analogue; probably unnecessary while review
  is per-plan rather than per-call, but the "never trip silently" rule is
  general.
- **`session_facts.py` records without deciding.** The known world and the
  "what arrived from outside" ingestion feed the audit log and change nothing in
  v1, deliberately: *"recording it now means the v2 question ('would this fact
  have changed a verdict?') is answerable by replaying a shadow run instead of
  re-arguing it."* Good discipline; identical to how our boot report earns its
  keep.
- **Their `overrides.py` inviolable rule** — *"this store is user-local and is
  NEVER written by a persona/package"* — is already captured in the Phase 5
  ROADMAP entry. Re-reading confirms the wording; nothing new.
- **`workspace_trust.py`**: a repo may declare command prefixes in
  `.coworker/config.toml`, but they take effect only after the user trusts that
  canonical path, and *"trust follows the path rather than a snapshot of the
  config"*. Relevant if we ever honour in-repo policy files. We do not.
- **Amusing licence inversion:** they pin `pypdfium2` over PyMuPDF explicitly
  because *"AGPL license can't ride in the DMG"*.

---

## 5. What this survey does **not** change

- The containment verdict. No sandbox, in-process governance, one Python
  process holding the agent loop, the judge, and every connector token.
- The five entries the 2026-08-14 survey produced (ask channel #564, declared
  risk class, target-bound standing grants, auto-compaction, `SKILL.md`
  progressive disclosure). All still correct; nothing in the August work
  supersedes them.
- The two "we already do better" items (handoff dehydration, shell
  metacharacter parsing).

## 6. Proposed follow-ups, in priority order

1. **Layered oversight corpus + committed ship gate for CASSANDRA's plan
   review** (§3.1). Highest value: it is the only item that tells us whether
   the escalation lattice actually works. Extend `tests/guard/corpus`'s README
   convention with `expected_current` / `expected_secure` / `known_gap`, add a
   `permission_gate`-equivalent keyed on our four verdicts, and put the
   error-vs-judgment split into the harness before the first report.
2. **Non-diagnostic denial to the agent for every CASSANDRA verdict** (§3.4.4).
   Small, self-contained, closes an oracle. `WITHHELD_NOTE` is the pattern
   already; generalise it.
3. **`kastellan_policy` role split for the policy tables** (§3.3). One
   migration, mirrors 0002, removes the runtime role's ability to widen its own
   allowlist.
4. **"Effect outlives the approving conversation" as a floor class** in the
   Phase 5 standing-grant design (§3.2) — no standing grant may cover it.
5. **Task-scoped agent-authored-artefact provenance** as a deterministic signal
   to the plan reviewer (§3.5), when Phase 4's python-exec skill arc lands.
6. **Document why the guard tier fails open and the plan reviewer should fail
   closed** (§3.4.3) — one paragraph in the CASSANDRA docs, prevents a future
   reviewer "fixing" the asymmetry.
