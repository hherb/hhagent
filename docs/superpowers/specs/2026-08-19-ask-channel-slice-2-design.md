# The ask channel — #564 slice 2

Issue: [#564](https://github.com/hherb/kastellan/issues/564). Base: `main` @ `af3e7e66`.
Predecessors: [`2026-08-16-ask-record-slice-1a-design.md`](2026-08-16-ask-record-slice-1a-design.md)
(merged as [#570](https://github.com/hherb/kastellan/pull/570)) and
[`2026-08-18-ask-path-slice-1b-design.md`](2026-08-18-ask-path-slice-1b-design.md)
(merged as [#578](https://github.com/hherb/kastellan/pull/578)).

## Why this exists

Slice 1b made `Verdict::Escalate` raise a durable question and suspend its task. The
question reaches exactly one surface: `kastellan-cli inbox`, on the host. So the daemon
now stops and waits for a human who has no way of knowing they are being waited for —
unless they happen to run a CLI command on the box. For a channel-originated task the
absurdity is sharper: the user asked over Matrix, the agent decided a human must approve
something, and the room goes quiet for 24 hours until the deadline fails the task.

The missing piece is one primitive. `channel/bus.rs` is strictly *inbound message → task
→ outbound reply on completion*, driven by the completed-task `NOTIFY` through
`route::reply_for_completed_task`. Nothing lets core **start** a conversation. Slice 2
adds that, and uses it for the one thing that needs it today.

Two seams cut in earlier slices and deliberately left unused are the whole correlation
mechanism, and slice 2 is their first caller: `db::asks::raise` returns the plaintext
nonce exactly once (1b destructures and drops it), and `db::asks::resolve_with_nonce`
matches on `nonce_sha256` with zero callers. Do not build a second mechanism.

## What slice 2 delivers

1. `core/src/channel/outbox.rs` — `ChannelOutbox`, the core-initiated-outbound primitive.
2. `core/src/channel/ask_message.rs` — pure render of the outbound ask, pure parse of the
   inbound answer, and the shared task-origin extractor.
3. One arm in `bus::handle_inbound` recognising `/approve <nonce>` / `/deny <nonce>`,
   behind a new `AskResolver` seam.
4. A best-effort delivery step in `scheduler::asks::raise_and_suspend`.
5. `db::asks::resolve_with_nonce` takes the claimant `(channel, peer)` and returns
   `Option<ResolvedAsk>` instead of `bool`; `NONCE_BYTES` drops 32 → 5.
6. `TaskContext.origin`, set once in `run_one` from the payload it already holds.
7. New audit rows: `ask.delivered`, `ask.undelivered`, `ask.delivery_failed`,
   `channel.ask_answer_rejected`; `ask.resolved` gains a channel actor.
8. An `ask_timeout`-aware arm in `route::reply_body`.
9. Step 0: a movement-only split of `core/src/scheduler/asks.rs` (801 lines, over the cap).

No migration. No new configuration.

## Design decisions

### D1 — The primitive is a shared registry, not a NOTIFY and not a durable outbox

The scheduler is spawned at `main.rs:530`; the channel supervisors start at `main.rs:547`
and `main.rs:564`, and each *restarts its bus* under supervision (#514/#517). So the
scheduler cannot hold a `Sender` the bus owns: it does not exist yet at scheduler spawn,
and the one it would hold goes stale on every respawn.

`ChannelOutbox` is the indirection both sides share — an `Arc` created in `main` before
either, holding `RwLock<HashMap<ChannelId, mpsc::Sender<OutgoingMessage>>>`.
`ChannelBus::spawn` registers each channel's *existing* per-channel sender (the same one
the outbound pump already pushes replies into, so there is one queue per channel and no
second delivery path); `ChannelBus::shutdown` deregisters. A restart re-registers, and a
stale sender's `try_send` returns `Err`, which is the failure signal rather than a silent
drop.

Two alternatives were considered and rejected:

- **`NOTIFY` carrying the nonce.** Decouples ordering completely, but `pg_notify`
  succeeds with zero listeners, so a delivery that reached nobody is indistinguishable
  from one that landed — it deletes the `ask.delivery_failed` signal D2 depends on. It
  also puts the plaintext nonce into the DB's notification stream, weakening slice 1a's
  hash-only posture for no gain.
- **A durable outbox table, retried until the deadline.** Survives any outage, but it
  persists the plaintext nonce — the exact property `db::asks` was built to avoid (no
  nonce field on `Ask`, hash-only column, no Rust-side comparison, `Nonce` zeroized on
  drop). Buying reliability by storing the capability is the wrong trade for a system
  whose CLI fallback already covers the outage case.

### D2 — Delivery is best-effort and never fails the ask

Order is load-bearing: `raise` commits (task suspended, ask durable), `ask.raised` is
written, and *only then* is delivery attempted. Every delivery failure — no origin, no
outbox, no such channel, closed queue, full queue — is audited and returns `Ok`. The task
stays suspended and `kastellan-cli inbox` still answers it.

The inverse order would be worse in both directions: delivering before the commit can
send a nonce for an ask that then fails to insert, and failing the raise on a delivery
error would turn a Matrix outage into a task failure on the one path where the reviewer
said a human must decide.

### D3 — The destination is the task's own origin, and a non-channel task is undelivered by design

An ask is delivered to the channel, peer and conversation the task came from. A task with
no channel origin — `kastellan-cli ask`, a scheduled task — is not delivered anywhere; it
audits `ask.undelivered` with reason `task_has_no_channel_origin` and the CLI remains its
surface.

A configured fallback room was considered and deferred. It is three env keys, an
install-time default, and a new silent-failure mode (a typo'd room id swallows
escalations — the [#550](https://github.com/hherb/kastellan/issues/550) shape), and it can
be added later without changing the primitive: the fallback is another way to compute
`Option<AskDestination>`, which is already the seam.

### D4 — `try_send`, not `send`

`ChannelOutbox::try_deliver` is **synchronous**. Two consequences, both wanted: the raise
path never blocks on a wedged channel whose consumer has stopped draining (a full queue is
an immediate `QueueFull`, not an await), and no lock is held across an await point, which
is the whole class of deadlock a `RwLock` + async combination invites.

### D5 — The answer is recognised after authorization and before screening

The order in `handle_inbound` is the security content, and it mirrors the pairing
carve-out's reasoning one arm above it:

- **After `authorize` returns `Recognised`.** Only a paired peer can resolve anything.
  This is the "id and authority kept separate" constraint from the ROADMAP: the nonce
  proves the caller holds the capability for *this* ask, and `channel::auth` proves the
  caller is someone the operator paired. `resolve_with_nonce`'s own doc says the second
  half is the caller's job, and this is the caller — see D16, which is the *strong* form
  of that half.
- **Before `screen_and_classify`.** An answer must never become a task. A `/deny …` that
  fell through to the enqueue path would be handed to the planner as an instruction.

### D6 — The answer is NOT injection-screened, deliberately

The body is a closed set: one of two fixed verbs plus an opaque token, parsed into
`AskCommand` and never interpolated into a plan, a prompt, or a tool argument. Running the
injection guard over it cannot prevent anything — there is no interpolation to poison —
and a false positive would block a legitimate approval, which is a fail-closed direction
that costs the operator the one action the whole slice exists to enable.

The ROADMAP's "the inbound body is untrusted like any other channel message, so it goes
through `screen_and_classify`" holds for every body that does *not* parse as a command:
those take the existing path unchanged. This decision narrows that sentence to the case it
was written about, and the narrowing is what D5's ordering makes safe.

### D7 — No syntactic pre-check on the nonce token

`parse_ask_command` requires exactly two whitespace-separated tokens and takes the second
verbatim, with no hex/length/charset check. The *verb* is checked, because it becomes
`resolution.choice` and `resolve_with_nonce` enforces that against the ask's own `options`
— so `AskCommand`'s two variants render to exactly the strings `"approve"` and `"deny"`
that `raise_and_suspend` writes into `options`, and a mismatch there is a rolled-back
transaction rather than a wrong resolution. `Nonce::from_wire`'s doc states the rule this
follows: `resolve_with_nonce`'s `WHERE` predicate is the only thing entitled to decide
whether a nonce is real. A shape check here would additionally couple the parser to the
nonce *encoding*, so changing `generate_nonce` would silently stop every answer from
parsing while every test of the resolver still passed.

### D8 — `resolve_with_nonce` returns `Option<ResolvedAsk>`, not `bool`

The ack should name the task that is resuming; a `bool` cannot. The function already has
`task_id` in the row it locks, and it has **zero callers**, so widening the return type
costs nothing and is strictly better than a second query. `ResolvedAsk { ask_id, task_id }`
mirrors the existing `ExpiredAsk`.

### D9 — The failure ack is deliberately indistinguishable

A rejected answer gets one sentence covering wrong / already-answered / expired /
cancelled, because `resolve_with_nonce` deliberately does not distinguish them: splitting
them hands a nonce-guessing peer an existence oracle over ask ids. The ack must not
reintroduce at the presentation layer what the query refuses to leak.

### D10 — The origin extractor is shared with `route.rs`

`reply_for_completed_task` already extracts `(kind == "channel", channel, peer,
conversation)` from a task payload. Slice 2 needs the same four fields for the same task
rows. Factoring one `destination_from_task_payload` out and calling it from both means
the place an ask is delivered and the place its answer is replied to **cannot disagree** —
a second copy would drift the first time either grew a field.

### D11 — The concern text is model-authored, so the render clamps it

The ask body is CASSANDRA's `reason` string, which originates in a reviewer stage and can
be model-authored. It is rendered as data with a **512-byte clamp** (`CONCERN_CAP`), in the same spirit as
`summary::ok_summary_cap`. It is going to a paired operator in their own room, so this is
a legibility bound rather than a containment one — an unbounded concern would push the two
copyable command lines off the visible message. The clamp truncates on a char boundary and
appends an ellipsis marker, so a clamped concern is visibly clamped rather than silently
cut mid-sentence.

### D12 — A trait seam for the resolver, none for the outbox

`AskResolver` is a trait because its real implementation needs a `PgPool` and the bus's
tests are deliberately PG-free. `ChannelOutbox` gets no trait: a real `ChannelOutbox` with
a registered sender and a receiver the test drains *is* the perfect fake, and dropping the
receiver is exactly how the failure path gets exercised. Adding a trait would mean the
tests stopped covering the real registry's locking and lookup.

The bus takes `Option<Arc<dyn AskResolver>>`. `None` means commands are not recognised at
all, so a bus built without one behaves byte-identically to today's.

### D13 — `origin` lives on `TaskContext`, not a second `tasks::get`

`run_one` already holds the whole `Task`, payload included. Computing the destination
there and carrying it on `TaskContext` avoids a second read of a row we just had, and
keeps `raise_and_suspend` free of a DB round trip on the escalation path. The 14 existing
`TaskContext` literals must each gain the field; that churn is the point — a new field is
a compile error rather than a silent `None`, the same argument `canonical_form`'s
no-`..` destructuring makes in slice 1a.

### D14 — `ask_timeout` gets its own reply arm

An expired ask already reaches the room, and this is worth recording because it is not
obvious: `notify_task_completed` is an `AFTER UPDATE OF state` **trigger**
(`0005_tasks_scheduler.sql`), and `fail_awaiting_operator` moves `awaiting_operator →
failed`, which crosses into the trigger's terminal set. So the completed-task pump fires
and `reply_body` renders `{"kind":"error","detail":"ask_timeout"}` as *"Sorry — that
failed: ask_timeout."* That is true but unhelpful, so `reply_body` gains an arm for the
one detail string `db::asks::ASK_TIMEOUT_DETAIL` defines.

Only the **suspend** direction (`running → awaiting_operator`) is silent, and that is
precisely what D1's push exists to cover.

### D15 — Step 0 is a movement-only split

`core/src/scheduler/asks.rs` is 801 lines, already over the 500-line guidance, and
HANDOVER names it the best first split candidate (its pure half separates cleanly from its
async half). This repo's own rule is to split *before* the change that grows a file, so the
movement diff is reviewable on its own and the test count is verifiable across it —
`boot_supervisor/tests/` is the worked example. Slice 2 grows this file, so the split
comes first, as its own commit, with the test count asserted identical before and after.

### D16 — The claimant must be the task's own peer, and the check lives in the guarded UPDATE

**The nonce is a bearer token, and in a shared room everybody can read it.** It is
delivered as a message into the conversation, so every peer with read access holds it —
which means it never protected against the case the ROADMAP reached for it to solve
("on a Matrix room any peer who can send can guess an id and resolve someone else's
approval"). Guessing was the wrong threat: *reading* is the easy path.

What actually closes it is binding the resolution to the task's own peer: ask N is
answerable only by the `(channel, peer)` recorded on the task that raised it. A co-present
room member can then read the token and still not use it.

The check is **not** a Rust-side pre-flight in the caller. It is a predicate inside
`resolve_with_nonce`'s existing guarded UPDATE:

```sql
WHERE nonce_sha256 = $1
  AND state = 'pending'
  AND deadline_at > now()
  AND EXISTS (SELECT 1 FROM tasks t
               WHERE t.id = asks.task_id
                 AND t.payload->>'kind'    = 'channel'
                 AND t.payload->>'channel' = $2
                 AND t.payload->>'peer'    = $3)
```

A caller-side check would be a TOCTOU: verify, then resolve, with the entitlement
established against a row read outside the transaction that commits the resolution. In
the guard it is atomic and fail-closed, and it inherits the no-existence-oracle property
for free — a wrong peer is indistinguishable from a wrong nonce, which is exactly what
D9 requires of the ack.

**A side effect worth having:** `resolved_by` stops being a free `&str` parameter and is
computed *inside* the function as `"<channel>/<peer>"` from the same two arguments the
guard uses. That deletes the hazard the `Nonce` newtype was created to defend against —
`resolve_with_nonce(pool, nonce, resolved_by, …)` took the secret and the attribution as
adjacent `&str`s, and transposing them compiled. There is now no attribution parameter to
transpose. Slice 1a's newtype stays: it still stops the nonce being logged or serialized,
and defence that has become redundant along one axis is not defence to delete.

A task with no channel origin therefore cannot be answered from a channel at all — the
`EXISTS` finds nothing. That is the correct pairing with D3, which does not deliver such an
ask in the first place.

### D17 — The nonce shrinks to 5 bytes

With D16 carrying the entitlement, the nonce is correlation plus defence in depth rather
than the sole barrier, and 32 bytes buys nothing but an unusable message. `NONCE_BYTES`
drops 32 → 5: **10 hex characters**, copyable from a phone, so `/approve 7f3a9c2e1b` is a
command a tired operator will actually run at 2 a.m. — which is the population this whole
feature is for.

40 bits against an attacker who must already be a *paired peer answering their own task's
ask* (D16), gets one attempt per inbound message, writes a `channel.ask_answer_rejected`
audit row on each miss, and has until the 24 h deadline. Nothing about that is close.

No migration: `asks.nonce_sha256` stores the SHA-256, which is 64 hex characters whatever
the input length. Asks raised before the change keep their long nonces and resolve
normally — the column and the predicate are unchanged.

## Control flow

**Raising and delivering** (`scheduler::asks::raise_and_suspend`):

```
plan escalates
  → plan_digest(plan)
  → db::asks::raise(...)                     [COMMITS: task suspended, ask durable]
      ↳ RaisedAsk { ask_id, nonce }
  → emit_ask_raised(...)                     [audit ask.raised]
  → deliver(outbox, ctx.origin, ask_id, &nonce, concern, deadline_at)
      ├─ origin.is_none()  → audit ask.undelivered  {reason: task_has_no_channel_origin}
      ├─ outbox.is_none()  → audit ask.undelivered  {reason: no_channel_configured}
      ├─ try_deliver → Err → audit ask.delivery_failed {reason: <fixed label>}
      └─ try_deliver → Ok  → audit ask.delivered
  → drop(nonce)                              [zeroized]
  → Outcome::AwaitingOperator { ask_id }
```

**Answering** (`bus::handle_inbound`):

```
inbound message
  → authorize
      ├─ RejectedUnauthentic → drop + audit          (unchanged)
      ├─ Rejected            → pairing carve-out     (unchanged)
      └─ Recognised
           → parse_ask_command(body)
               ├─ None    → screen_and_classify → enqueue / injection_blocked (unchanged)
               └─ Some(cmd)
                    → resolver.resolve(nonce, choice, Claimant{channel, peer})
                        │   └─ one guarded UPDATE: nonce ∧ pending ∧ deadline ∧ owning peer
                        ├─ Some(ResolvedAsk) → audit ask.resolved (actor channel)
                        │                      → ack "✓ Approved — task N is resuming."
                        └─ None              → audit channel.ask_answer_rejected
                                               → ack the indistinguishable sentence
```

The `Claimant` is built from the **inbound message's own** channel and peer — the pair
`authorize` just recognised — never from anything in the body. A body-supplied identity
would be the whole point of D16 handed back to the sender.

What the channel surface writes matches the CLI's shape exactly:
`resolution = {"choice": "approve" | "deny"}` — **no `free_text` key**, because the strict
two-token parser rejects trailing prose, so there is none to store. `resolved_by` is no
longer a parameter at all: `resolve_with_nonce` composes it as `"<channel>/<peer>"` from
the same `Claimant` its guard matched on (D16), so the attribution in the audit trail is
the identity the entitlement was checked against, by construction rather than by the
caller's good behaviour.

The ack is sent to the conversation the **command** arrived on, which is the same one the
ask was delivered to in every reachable case; deriving it from the inbound message rather
than re-reading the task keeps the reply where the operator is looking.

The resolution re-enqueues the task through slice 1a's `tasks_resumed` NOTIFY, the lane
runner claims it, and its eventual completion replies to the same conversation through the
existing outbound pump. Slice 2 adds no second completion path.

**The message on the wire:**

```
⚠️ Approval needed — task 412

An operator decision is required before I continue:
> plan writes outside the scratch directory

This expires 2026-08-20T09:14:31Z. Reply with one of:

/approve 7f3a9c2e1b
/deny 7f3a9c2e1b
```

Two details of that rendering are load-bearing rather than cosmetic.

The concern is **quoted line by line**. It is the reviewer's `reason` — model-authored
text spliced into a message whose other content is two `/`-leading command lines — so
without the fence a `reason` containing a line beginning `/approve` puts extra
command-shaped lines in the operator's room. The forged tokens resolve nothing
(`resolve_with_nonce` decides that, and only it), so what the fence buys is legibility,
not containment: "exactly two commands are offered" becomes a property production
enforces rather than one the tests merely assert.

The deadline is **RFC 3339 with the nanoseconds zeroed**. `OffsetDateTime`'s `Display` is
not a wire format — in `time` 0.3 the hour is unpadded and a subsecond fraction is always
emitted, so a deadline taken from `now_utc()` renders as
`2026-08-21 9:14:32.482913571 +00:00:00`. A 24-hour approval window has no business
claiming nanosecond precision, and the fixture that hid this was the one input
(whole-second epoch) for which the two formats agree.

## Audit rows

| actor | action | payload | when |
| --- | --- | --- | --- |
| `scheduler` | `ask.delivered` | `ask_id, task_id, channel, peer` | the message was queued to a channel |
| `scheduler` | `ask.undelivered` | `ask_id, task_id, reason` | no origin, or no channel configured |
| `scheduler` | `ask.delivery_failed` | `ask_id, task_id, channel, reason` | the queue refused it |
| `channel` | `ask.resolved` | `ask_id, task_id, choice, resolved_by, via: "channel"` | a peer's answer resolved an ask |
| `channel` | `channel.ask_answer_rejected` | `channel, peer` | an answer attempt did not stand — see below |

Never in any payload: the nonce, the rendered body, the concern text. `ask.resolved` is
deliberately the **same action** slice 1b's CLI writes, with a different actor and a `via`
field, so observation SQL grouping on `ask.resolved` sees both surfaces as one population.
The CLI writes `via: "cli"` for the same reason, so the column is total rather than NULL
for half the rows.

The channel row's identity key is **`resolved_by`**, not a `channel` + `peer` pair. That
is the composed `"<channel>/<peer>"` attribution `resolve_with_nonce` built from the
claimant its D16 guard matched on — the identity the write was *authorised* against — and
it is byte-identical to what that query stored in `asks.resolved_by`. Two loose fields
would be weaker provenance for the same information, and would not line up with the CLI
row's own `resolved_by`.

`channel.ask_answer_rejected` carries channel + peer only — a fixed-label row, no token
fragment. Repeated rejections from a paired peer are worth being able to count. **Three
producers write it**: a well-formed command whose token resolved nothing; a malformed
attempt (leading verb, body does not parse), which gets the usage ack and never reaches
enqueue; and a resolver `Err`, which is collapsed into the first arm on purpose so a DB
outage cannot become the existence oracle the refusal path refuses to be. Telling the
three apart in the payload is a durable shape change and is deferred to a later slice, so
a reader counting these rows today must not assume they are all the first case.

## Testing

Pure, no I/O:

- `parse_ask_command` — both verbs; case-insensitive verb; rejects a bare verb, three
  tokens, trailing prose, an empty token, a plain instruction, and a verb without the
  leading slash; accepts a non-hex token (D7).
- `render_ask` — contains both copyable command lines verbatim; contains the nonce exactly
  once per line; clamps an oversized concern; the clamp has both an upper *and* a lower
  bound assertion (the #572 lesson: a clamp property asserting only an upper bound
  inverted its own purpose).
- `destination_from_task_payload` — a channel payload; a non-channel `kind`; each of the
  three fields missing; and one test asserting `route::reply_for_completed_task` and this
  function agree on the same payload (D10).

`ChannelOutbox`: register → deliver → received; deliver to an unregistered channel;
deliver after deregister; deliver with the receiver dropped (`QueueClosed`); deliver to a
full queue (`QueueFull`); re-register replaces a stale sender.

`bus::handle_inbound`: a command from a **paired** peer resolves and **does not enqueue**;
a command from an **unpaired** peer never reaches the resolver (the load-bearing negative
— asserted on a resolver fake that records calls, so the assertion is "zero calls", not
"returned false"); a non-command body still enqueues unchanged; a rejected token replies
and writes the reject row; a bus with `resolver: None` treats `/approve x` as an ordinary
message.

`scheduler::asks`: delivers for a task with an origin; audits `ask.undelivered` for one
without; audits `ask.delivery_failed` when the receiver is gone; and — the one that
matters — **the ask is committed and the outcome is `AwaitingOperator` in every one of
those failure cases**.

`db::asks` (PG e2e, D16 — the entitlement guard, where the interesting cases live):

- the owning peer resolves; **a different paired peer, holding the correct nonce, does
  not** — this is the co-present-room case and the reason D16 exists, so it is the one
  test that must exist even if the others are cut;
- the right peer on the *wrong channel* does not resolve (a `matrix`/`email` collision on
  the same peer string);
- an ask whose task has no channel origin resolves through neither peer (pairs with D3);
- `resolved_by` lands as `"<channel>/<peer>"` without the caller supplying it;
- a wrong peer and a wrong nonce are **indistinguishable** in the return value (D9);
- a pre-existing 64-hex nonce still resolves after `NONCE_BYTES` drops (D17's
  no-migration claim, asserted rather than argued).

PG e2e (`core/tests/`): raise against a channel-originated task → the outbox receives a
message containing the nonce → resolve as that task's peer → the task is back in `pending`
and `resolved_for_task` carries the approval. This is the only test that proves the
delivered token is the one that resolves; every pure test above could pass with the two
halves disagreeing.

Mutation set to run, chosen where the per-module rounds would not look (the #572 lesson —
a mutation score measures the mutation set): invert the D5 ordering so the command is
screened first; make `parse_ask_command` accept three tokens; make `try_deliver` return
`Ok` on a closed queue; delete the `origin.is_none()` arm; make the rejected-answer ack
name which failure occurred (D9); return `Ok(())` from `deliver` before the audit call;
and — the two that matter most — **drop the `EXISTS` clause from the D16 guard**, and
build the `Claimant` from the ask's own task instead of the inbound message, which is the
shape that would make the check tautologically true.

## What this slice deliberately excludes

- **The `ask_user` planner tool and `propose_plan`.** Both are the same inbox item with a
  different `kind`, and the ROADMAP is explicit that they become cheap once the primitive
  exists and should not precede it.
- **A fallback destination for non-channel tasks** (D3).
- **The autonomy-ceiling axis** (`tasks.autonomy`) and the **dead-letter store**. Separate
  ROADMAP items under the same heading; neither is needed to answer an ask.
- **Email delivery in practice.** The code is transport-agnostic and an email-originated
  task will be routed to `EmailChannel`, whose `send` still refuses unconditionally
  (outbound SMTP is email slice 2). That produces an honest `ask.delivery_failed` row
  rather than a silent drop, and it is the correct behaviour until SMTP lands.
- **Answering from an unpaired peer, by any means.** D5 forecloses it.

## Open risks

**The concern text is the only thing telling the operator what they are approving.** It is
the reviewer's `reason` string, and if it turns out to be too terse to decide on, the
answer is to put the plan digest and a step summary in the message — not to let the
operator ask the agent for detail, which would be the agent explaining its own plan to its
own reviewer.

**A shared room still leaks the question, even though D16 stops it being answered.** Two
peers in one room both *see* every ask raised for tasks they did not start; what they
cannot do is resolve one. Confidentiality and authority are separate properties and slice 2
only fixes the second. If the deployment stops being single-user, the destination should
become the task's peer in a direct conversation rather than its conversation id — which is
a change to `destination_from_task_payload` alone.

**The trust root under all of this is the homeserver.** `ev.sender` on an inbound event is
what the homeserver asserts, and the bus has no device-verification gate, so a compromised
homeserver can forge a paired peer. D16 does not change that, and neither would signing
approvals: an attacker who can forge messages forges the *instruction* too, and the chain
(forged instruction → escalation → forged approval) defeats review either way. Closing it
means authenticating **every** inbound message, which is a channel-wide redesign and is
deliberately out of scope here. Recorded so the next person weighing message signing does
not have to re-derive why it is not a slice-2 problem.
