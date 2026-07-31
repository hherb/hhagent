# Session Handovers

This directory holds the **rolling handover document** that lets a fresh
Claude Code session pick up exactly where the previous one left off. The
user just says "read the handover" and the next session has full context.

## Convention

- **One active document**: [`HANDOVER.md`](HANDOVER.md). Always the current
  state-of-the-world.
- **At the start of every session**, read `HANDOVER.md` first. It tells you:
  what's done, what's working, what the next TODO is, and the context you
  need to start.
- **At the end of every session**, update `HANDOVER.md` in this strict order
  (load-bearing fields first; prose last):

  1. **Bump the load-bearing header fields** — *before adding any prose*.
     They are what the next session reads first and treats as authoritative,
     so they must be current even if you run out of time for the prose.
     (They are not literally the top lines of the file: the
     **Last updated:** paragraph sits below the release banner and the
     "Recently merged" list.)

     - `Last updated:` → today's date
     - **Current state / Last commit** → the hash of the most recent shipped
       commit on whichever branch you're handing over from. Confirm with
       `git log --oneline -1`.
     - Re-run `cargo test --workspace` and fold the fresh
       **passed / failed / ignored / `[SKIP]`** counts into the
       **Last updated:** paragraph (there is no separate
       "Session-end verification" field).
     - **Every test-count number embedded in the doc that changed this
       session** — search for the old count and replace with the new one.
       Stale numbers are silently misleading; a fresh agent will trust them.

  2. **Move the previous "Next TODO" into "Recently merged"** if it
     shipped — with enough detail (file paths, decisions, gotchas,
     test-count delta) that the next session can start cold.
  3. **Write a fresh "Next TODO"** for the next session.
  4. **Refresh "Working state"** — anything that became real, anything new
     under stubs.
  5. **Tick `[ ]` → `[x]` in [`../ROADMAP.md`](../ROADMAP.md)** with the
     commit hash for every item that shipped.
  6. **Commit `HANDOVER.md` + `ROADMAP.md` together** with a
     `docs(handover): ...` message.
  7. **If a milestone shipped**, check whether `site/roadmap.html` (the
     timeline + its "Last updated" stamp) and the landing-page status
     numbers need a one-line update — see `site/README.md`.

  **Why fields-first matters.** The prose is the easy part to write but
  the easy part to skip-update; if a session ends with stale load-bearing
  fields, the next session reads the wrong commit hash and the wrong
  test count, and silently drifts off-state. Updating those fields first
  guarantees they are current even if the session is cut short before
  the prose is fully written.
- **Pruning**: keep HANDOVER.md focused on what the next session needs to
  act on (current state + last 2–3 sessions in detail + next TODO). Older
  session entries get compressed into an "Earlier history" summary or
  dropped once they're no longer load-bearing. Before pruning, snapshot
  the current HANDOVER.md to [`archive/handover_<YYYYMMDD>[_<slug>].md`](archive/)
  — the archive is the audit trail and is never edited after the fact.
  See the "How to update this document at session end" section in HANDOVER.md
  for the full pruning checklist.

## Why this exists

Sessions on this project tend to span weeks. Without a deliberate handover,
context drifts: stubs get re-stubbed, decisions get re-litigated, and
threat-model details get forgotten. The handover doc is the cheapest
mechanism that fixes that.
