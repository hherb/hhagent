# Ask recognition — exact containment, honest UX, diagnosable rejections

Closes [#582](https://github.com/hherb/kastellan/issues/582),
[#583](https://github.com/hherb/kastellan/issues/583),
[#584](https://github.com/hherb/kastellan/issues/584).

Follows [#564](https://github.com/hherb/kastellan/issues/564) slice 2
([#579](https://github.com/hherb/kastellan/pull/579), `bb937df7`); spec
`2026-08-19-ask-channel-slice-2-design.md`, whose decisions D7, D9 and D16 this
one builds on and does not revisit.

---

## Why this exists

Three findings against the merged ask channel, two of them from a live test
against the deployed DGX bot on 2026-08-20 (recorded in #581's comment).

**One predicate is doing two unrelated jobs.** `channel::ask_message::looks_like_ask_command`
answers *"this body mentions an ask verb, so it might carry a live approval
token"*, and `handle_inbound` uses that single guess both to keep a live token
out of `tasks.payload` **and** to tell a fumbling operator their syntax was
wrong. Those are different questions with different right answers, and
conflating them has made the predicate wrong in both directions:

- **Too narrow, twice.** v1 checked only the first whitespace token — shaped to
  the reported instance (`/approve tok9 thanks!`) rather than to the property.
  A quoted reply, a leading mention pill and prose around the command each
  walked past it into the task queue carrying a live token. #579 widened it to
  scan every token.
- **Now too broad.** An ordinary message containing a bare `/approve` is refused
  instead of enqueued. On Matrix that costs a rephrase; on email the refusal
  cannot even be sent (`EmailChannel::send` bails unconditionally until outbound
  SMTP lands), so the message is dropped silently.

**The usage hint teaches a shape whose literal transcription fails invisibly.**
`ACK_MALFORMED_COMMAND` says ``Usage: /approve <token>``. Element parses
`<token>` as an unknown HTML tag and **drops it from the sender's own
timeline**, so an operator who transcribes the hint literally sends a
well-formed-but-unresolvable two-token command, receives the deliberately vague
`ACK_NOT_ANSWERABLE`, and cannot see what they actually sent. The reply
contradicts the message they can read back, and the cause is invisible on both
ends.

**The audit trail cannot say which arm refused.** All three producers of
`channel.ask_answer_rejected` write an identical payload. Diagnosing the above
needed `strings` on the deployed binary plus a second hand-run experiment in
Element, because the row that was supposed to explain it said nothing.

---

## What this slice delivers

1. **Containment becomes exact** — a peer-scoped live-nonce lookup replaces the
   shape guess as the thing that decides whether a body may be enqueued.
2. **The shape check narrows back to first-token-only** and is re-aimed at the
   job it is actually good at: recognising a *typed command*.
3. **`ACK_MALFORMED_COMMAND` drops the `<token>` metasyntax.**
4. **`channel.ask_answer_rejected` gains a `reason` field** with a closed
   four-value vocabulary.

It changes no wire vocabulary (`/approve`, `/deny` stand — see D1), no schema,
and no migration.

---

## Design decisions

### D1 — The offered vocabulary does not change

#581 asked whether Element swallows a leading `/`. Answered live on 2026-08-20:
**it does not.** Two messages from a real Element client reached the bot and
were answered correctly, corroborated by a `channel.ask_answer_rejected` row.

So D17's `/approve <token>` vocabulary stands, and **none of #581's three
candidate mitigations are built** — not a bare no-slash form, not a different
rendering, not reply-quote guidance. In particular, do **not** widen recognition
to accept a bare `approve <token>`: the reason to do so does not exist, and it
would enlarge exactly the false-positive surface D3 shrinks.

### D2 — Containment and UX are split, because they are different questions

The load-bearing decision of this slice.

| job | the question | right instrument | why the other one fails |
| --- | --- | --- | --- |
| **Containment** | may this body be enqueued? | exact peer-scoped nonce lookup | shape cannot answer it — the leaking shapes were ones nobody predicted, and the next one will be too |
| **UX** | did a human just fumble a command? | shape, first token only | a live token is irrelevant; `/deny` carries none and still needs the hint |

**A pure replacement, as #582 is literally written, would reintroduce the
failure #579 fixed.** `looks_like_ask_command("/deny")` is true today, so that
body gets the usage hint. Under an exact-nonce check alone it carries no live
token, falls through to `screen_and_classify`, and is **enqueued as a task with
no acknowledgement** — verbatim the failure #579's own doc describes: *"an
enqueued 'answer' got no acknowledgement at all, so the operator believed they
had approved while the task sat suspended until it expired."* `/deny` alone is
not hypothetical; it is the second message of the 2026-08-20 live test.

Splitting the jobs makes each instrument better and dissolves #582's complaint
as a side effect, because once the shape check no longer carries containment
load it is free to be *narrow* — and first-token-only is precisely the shape a
person **typing** a command produces. A quoted reply or a mention pill is not
someone typing a command; it is someone quoting, and containment now catches the
token in it regardless of shape.

### D3 — The broad guess survives, demoted to a cheap gate

`looks_like_ask_command` is **not deleted**. It becomes the cheap, DB-free
precondition on the containment arm, which is what #582 asks for: *"run only
when the existing cheap predicate has already fired — so ordinary traffic pays
nothing."*

It no longer decides anything by itself. Its false positive therefore stops
costing anything: `should I /approve the PR?` fires the gate, hashes its tokens,
matches no live nonce, is not verb-first, and is **enqueued normally**.

Its doc comment must lose the "#582 replaces this predicate" note and gain the
demotion, or the next reader will delete a function that is still load-bearing.

### D4 — Four arms, in this order

```
authorize -> Recognised
  |
  1. parse_ask_command -> Some(cmd)
     |  resolve(nonce, choice, claimant)
     |    Ok(Some) -> ack_resolved                       [ask.resolved]
     |    Ok(None) | Err -> ACK_NOT_ANSWERABLE           [reason: unresolvable]
     |
  2. looks_like_ask_command  AND  a token hashes to a live peer-scoped nonce
     |  -> ACK_MALFORMED_COMMAND, never enqueued         [reason: carries_live_token]
     |     cannot answer (DB error / over cap)           [reason: unscannable]
     |     no wiring: looks_like_ask_command alone decides  (D7 fallback)
     |
  3. first token is /approve or /deny
     |  -> ACK_MALFORMED_COMMAND                         [reason: malformed]
     |
  4. otherwise -> screen_and_classify -> enqueue
```

**Containment precedes the usage hint** so that `/approve tok9 thanks!` with a
*live* `tok9` audits `carries_live_token`, not `malformed`. Both give the same
ack; the rows differ, and the security-relevant one is strictly more specific.
It is the only row that ever shows the containment guard doing its job.

**A dead token is not a capability**, so arm 1's `Ok(None)` does not fall
through to arm 2, and a quoted reply whose token is already spent reaches arm 4
and is enqueued. Deliberate, and #582 says so in as many words.

### D5 — Peer-scoped, not global, and the residual is stated

An **unscoped** existence check is exactly the oracle D9 and D16 refuse to be: a
paired peer could probe token guesses and read the answer off refuse-vs-enqueue.
The nonce is five bytes.

So the check reuses D16's scoping — the nonce must belong to an ask whose task
is this claimant's own `(channel, peer)`.

**The residual, accepted:** another peer's token pasted into a body is not
caught and lands in `tasks.payload`. It is a leaked-but-inert secret, because
D16 means a token without its owning peer confers no authority — no one,
including the paster, can resolve with it. Recorded here rather than left
implicit, which was the previous narrowing's sin.

### D6 — One SQL predicate, bound twice, never hand-copied

`any_live_nonce_for_claimant`'s `WHERE` must agree with `resolve_with_nonce`'s
**exactly**. If it drifts narrower, containment misses a token resolution would
have accepted — the fail-open this whole arm exists to prevent, reached through
a copy-paste.

Both therefore bind one shared `const` SQL fragment rather than two hand-typed
predicates. This is the same drift the guard slice already paid for twice:
`Confusion::is_valid` vs `invalidity`, and `confusion_at` re-writing `p >= tau`
instead of calling `decide`. A test asserts a live nonce that `resolve_with_nonce`
accepts is also seen by `any_live_nonce_for_claimant`, and that one scoped to a
different peer is seen by neither.

### D7 — Three fail-closed edges

- **DB error** on the containment check → refuse. Never enqueue on an
  unanswered question.
- **No wiring** (`asks: None`) → fall back to today's broad predicate as the
  decider, preserving the current containment property exactly. Both production
  buses wire it (`matrix_boot.rs`, `email_boot.rs`), so this is a test-only
  state — but #579 deliberately hoisted containment out of `if let Some(wiring)`
  so it would not depend on one bus's configuration, and that intent survives.
- **Candidate explosion** → the pure `candidate_tokens` dedups over a bounded
  prefix and caps the distinct count, returning `None` over cap → refuse,
  audited `unscannable`. Inbound bodies are **not** bounded before enqueue
  (`build_channel_task_payload` stores `msg.body` whole; `SCAN_BYTE_CAP` bounds
  only screening), so without a cap a large body would hash unboundedly and ship
  a huge array to Postgres.

  Two new `ask_message` consts, deliberately **not** reusing `SCAN_BYTE_CAP`
  (that is the injection guard's document budget and answers a different
  question; sharing it would couple two caps that should move independently):
  `CANDIDATE_BYTE_CAP = 65_536` and `CANDIDATE_TOKEN_CAP = 1024`. The prefix is
  cut on a **char boundary**, since a body is arbitrary UTF-8 and both
  `String::truncate` and `&s[..n]` panic on a non-boundary index — the same
  trap `corpus::scannable_prefix` hit in the guard slice.

**No shape filter on candidates.** D7-of-slice-2 bans coupling to the nonce
encoding, and here the failure mode is worse than there: a wrong filter yields a
false *negative* — an uncaught live token — which is the dangerous direction.
Hash every token; let the index decide.

### D8 — `reason` is four values, and the deliberate collapse is preserved

```
unresolvable       arm 1: parse succeeded, resolve returned Ok(None) or Err
carries_live_token arm 2: a token in the body is a live nonce of this peer's
unscannable        arm 2: the containment question could NOT be answered —
                          over the candidate cap, a DB error, or no wiring
malformed          arm 3: verb-first but did not parse
```

`unscannable` covers all three of D7's fail-closed edges, including the
no-wiring fallback. Reporting that one as `malformed` would be a lie: the body
need not be verb-first to reach it, and the row would claim a syntax judgement
nothing made. "We refused because we could not answer" is one honest cause with
three triggers, and the daemon log carries which.

`Ok(None)` and `Err` stay collapsed into `unresolvable`, exactly as today. That
collapse is load-bearing — a DB error and a refused answer must look identical
to the peer or the error path becomes the existence oracle the refusal path
refuses to be — and this slice does not touch it.

**The field leaks nothing.** It is written to `audit_log`, which is
operator-queried and role-gated; the peer sees only the ack body, and arms 2 and
3 share one ack precisely so the peer cannot tell them apart.

One action with a field, **not** three actions: observation SQL grouping on
`action` must keep seeing one population by default (#584 asks for this
explicitly).

### D9 — `<token>` becomes `TOKEN`

`ACK_MALFORMED_COMMAND` becomes:

```
Usage: /approve TOKEN or /deny TOKEN — exactly the verb and the token, nothing else.
```

A plain uppercase word survives HTML rendering intact and still reads as a
placeholder. Pinned by a test asserting the constant contains no `<`.

**#583's fix 2 is explicitly rejected** — rejecting an obviously-placeholder
token would put a shape check back into `parse_ask_command`, which slice 2's D7
removed on purpose. **Fix 3 is the wrong layer** — `body` is already the
plain-text field; the tag survives in `body` and is dropped only at Element's
render time.

`render_ask` is untouched: it prints the real token on both command lines, so
answering a genuine ask is a copy, not a transcription. Verified by reading it;
matches #583's own scope note.

---

## Components

| unit | purpose | depends on |
| --- | --- | --- |
| `ask_message::candidate_tokens(&str) -> Option<Vec<String>>` | pure: dedup'd whitespace tokens of a bounded prefix, `None` over cap | nothing |
| `ask_message::is_command_shaped(&str) -> bool` | pure: first token is a verb (the narrowed check) | nothing |
| `ask_message::looks_like_ask_command(&str) -> bool` | pure: unchanged body, demoted to the arm-2 gate | nothing |
| `db::asks::LIVE_ASK_FOR_CLAIMANT_PREDICATE` | the one `WHERE` fragment both queries bind | nothing |
| `db::asks::any_live_nonce_for_claimant(pool, &[Nonce], &Claimant) -> Result<bool>` | one indexed `SELECT EXISTS` | the const above |
| `AskResolver::any_live_nonce(&[Nonce], &Claimant) -> Result<bool>` | the trait seam, so bus tests stay PG-free (slice-2 D12) | — |
| `bus::handle_inbound` arms 2 and 3 | the ordering in D4 | all of the above |

`candidate_tokens` and `is_command_shaped` are pure and live beside the existing
pure vocabulary in `ask_message.rs`, which is where everything decidable without
the bus or the DB already lives.

---

## Testing

**Pure, no DB** (`ask_message.rs` unit tests):

- `candidate_tokens` dedups, bounds the prefix, and returns `None` over cap.
- `is_command_shaped` is true for `/approve x` / `/DENY` and false for
  `> /approve x`, `should I /approve the PR?`, and a leading mention pill.
- `ACK_MALFORMED_COMMAND` contains no `<` (#583's drift guard).
- `looks_like_ask_command` keeps its existing table green — it is demoted, not
  changed.

**Bus, PG-free via a fake `AskResolver`** (`bus/tests.rs`):

- Each of the four arms is reached by exactly one representative body, asserting
  **both** the ack body and the audit `reason`. Nothing pins these payloads
  today, which is #584's own note.
- `should I /approve the PR?` with **no** live nonce → **enqueued**. This is the
  test that fails today and is the point of #582.
- A quoted reply carrying a live nonce → refused, `carries_live_token`, and the
  enqueue seam is never called.
- A resolver returning `Err` on the containment check → refused, `unscannable`.
- `asks: None` → the broad predicate still refuses (D7's fallback).

**PG-backed** (`channel_bus_pg_e2e.rs` / a `db::asks` e2e):

- A live nonce accepted by `resolve_with_nonce` is also seen by
  `any_live_nonce_for_claimant`; one scoped to a different peer by neither
  (D6's agreement test).
- A **spent** and an **expired** nonce are both invisible to the containment
  check, so their bodies enqueue.

**Mutations to run, each of which must fail a test:**

- drop the peer scope from `any_live_nonce_for_claimant` → the different-peer
  test must fail (otherwise D5's oracle is back).
- invert arm 2 and arm 3's order → the `carries_live_token` reason test fails.
- make the DB-error edge enqueue instead of refuse → the `Err` test fails.
- widen `is_command_shaped` back to any-token → the enqueue test for
  `should I /approve the PR?` fails.
- restore `<token>` in the ack → the no-`<` test fails.

---

## What this deliberately excludes

- **Any change to the offered vocabulary** (D1).
- **An escape hatch** (`\/approve …` to force enqueue). #582 rejects it, and
  correctly: it lets the operator override precisely the protection that exists
  *because* they cannot see that a quoted reply carries a token.
- **Splitting `ask_answer_rejected` into three actions** (D8).
- **Un-collapsing `Ok(None)` from `Err`** (D8).
- **Making the email refusal visible.** `EmailChannel::send` still bails; that
  is outbound SMTP, email-channel slice 2. D3 reduces how often it matters — an
  ordinary email mentioning `/approve` is now enqueued rather than silently
  dropped — but a genuine malformed attempt over email is still dropped.

---

## Open risks

1. **Another peer's pasted token is not contained** (D5). Inert under D16, but
   still a live secret written to a DELETE-less column.
2. **A body over the candidate cap is refused**, so a very large paste that
   happens to mention `/approve` costs a rephrase. Chosen over the alternative,
   which is a silent false negative on a token past the cap.
3. **`is_command_shaped` is still a guess**, just a much cheaper one to be wrong
   about: being wrong now costs a missing usage hint, never a leaked token. If a
   new *typed* shape turns up, widening it is safe in a way widening the old
   predicate was not.
4. **The two SQL predicates could still drift** if someone edits the const's
   consumer rather than the const. D6's agreement test is the guard; it must be
   PG-backed to mean anything.
