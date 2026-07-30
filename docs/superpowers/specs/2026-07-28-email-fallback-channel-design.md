# Email fallback channel — design (Phase 2, slice #5)

**Status:** designed, not implemented.
**Supersedes the inbound half of:** `2026-06-12-primary-communication-channel-design.md`
§"Fallback channel" — that document assumed a direct IMAP client; localmail did
not exist yet. The security posture it mandated (SPF/DKIM/DMARC pass + a
per-pairing in-body token) is kept in full; only the *transport* changes.
**Related:** `2026-07-22-localmail-mail-worker-integration-design.md` (the
read-only `mail.*` tool over the same localmail `/v1`),
`2026-07-27-upstream-extra-ca-operator-config-design.md` (#492 — the
single-private-origin rule that forces this design's two-worker split).

## 1. Problem

Matrix is the primary user↔kastellan channel and has **no single-user
homeserver failover**. The 2026-06-12 brainstorm chose a *cross-transport*
fallback instead of a second homeserver: if `matrix.kastellan.dev` is
unreachable, the user must still be able to reach the agent, and the agent the
user. That fallback is email.

Phase 2 is inbound. Today nothing implements it: `core/src/channel/` has the
bus, the pure security core, pairing, and exactly one transport
(`MatrixChannel`). The channel-generic `PolledWorkerDriver` was extracted in
slice 5b-4a explicitly so a second polled channel could reuse it.

Two facts that post-date the 2026-06-12 design reshape the solution:

1. **localmail is live** on the DGX and already ingests the user's mail over
   IMAP. It exposes `GET /v1/changes?since=<cursor>` — a purpose-built
   tail-subscription endpoint — plus `GET /v1/messages/{id}?headers=full`. A
   kastellan worker (`kastellan-worker-mail`) already talks to that API
   force-routed, including the self-signed-TLS path that #492 completed.
2. **#492 enforces single-private-origin trust.** `select_for_allowlist`
   returns `MixedAllowlist` when a worker's allowlist contains the keyed
   private host *plus* anything else, and a refusal **fails the spawn**.

## 2. Decisions

| # | Decision | Rationale |
|---|---|---|
| D1 | Inbound source is **localmail `/v1`**, not a direct IMAP client | localmail already holds the IMAP credentials and does the fetching. No IMAP crate to license-check and sandbox, no IDLE/reconnect state machine, no mail passwords inside a kastellan jail, and it reuses the force-routed + extra-CA egress path proven in #491/#492. |
| D2 | A gated email becomes a **normal channel task** | This is what makes it a real fallback. The 2026-06-12 "never commands" constraint is honoured as *never commands from an unauthenticated sender* — the per-pairing token is precisely the mechanism that design added to close that gap. |
| D3 | Outbound is a **separate SMTP worker** | Forced by #492: one worker cannot hold both a private self-signed origin (needs the extra-CA anchor) and a public SMTP host, because that is `MixedAllowlist` and fails the spawn. Two single-origin workers is also plain least privilege. |
| D4 | Scoping via a **dedicated localmail account + api-user grant** | `/v1/changes` is filtered server-side by localmail's existing `user_accounts` ACL. The channel reads one mailbox, not the 37k-message archive; nothing to filter, and nothing to get wrong, in kastellan. |
| D5 | The token lives in a new **`pairings.token_sha256`** column, minted by `kastellan-cli pair issue-token --channel email --peer <addr>` | Reuses the pairing lifecycle: hash-only storage, printed once, revocable, audited. Nullable, so Matrix rows keep NULL and Matrix behaviour is byte-identical. A **separate subcommand**, not flags on `pair issue`: that mints a single-use code for an in-channel handshake, and one command meaning two different things depending on flags is a footgun. |
| D6 | **Security decisions stay pure and in core**; the worker returns raw material | `channel/mod.rs` states every rejected message lands in `audit_log`. A gate inside the worker, or inside `parse_poll`, could not audit and would silently break that invariant. |
| D7 | The inbound **cursor is localmail's**, via a server-side subscription | See §5. localmail may own non-security state; it must not make security decisions. |
| D8 | Email pairing is **operator-only, out of band**, enforced by an explicit guard | Removing an unauthenticated brute-force target on a spoofable transport is the right posture. **Corrected 2026-07-29 after review:** this was originally justified as unreachable "by construction" because an unpaired sender holds no token. That reasoning was WRONG. An unpaired sender resolves to `Ok(None)` → `AuthDecision::Rejected`, which still reaches the carve-out; presenting a live single-use pairing code would then mint a `token_sha256 = NULL` row, **permanently disabling DMARC+token for that address**. The carve-out is therefore skipped explicitly whenever `msg.evidence.is_some()` — evidence being `Some` is precisely the marker for "this transport cannot authenticate its own peers", so the rule is general rather than an email special case. Matrix passes `evidence: None` and keeps the carve-out unchanged. |

## 3. Non-goals

* No change to the scheduler, runner, or task shape. A channel task from email
  is byte-identical to one from Matrix.
* No DMARC/DKIM *computation* — no DNS lookups, no signature verification. We
  consume the verdict our own MX already wrote. §4 explains why that is the
  meaningful boundary and how it is kept honest.
* No per-channel classification floor or restricted tool set. Considered and
  rejected for this slice: it would need a scheduler policy hook that does not
  exist. Revisit if the threat picture changes.
* No WebAuthn (still deferred, no client surface).
* No S/MIME or PGP. The token is the second factor.

## 4. Architecture

```
localmail /v1  ──poll──▶ kastellan-worker-email-in ──┐
(private IP literal,     Net::Allowlist = localmail  │  JSON-RPC
 self-signed TLS)        ONLY → receives the #492    │  stdio
                         extra-CA anchor             │
                                                     ▼
                                          PolledWorkerDriver ──▶ EmailChannel ──▶ ChannelBus ──▶ tasks
                                                     ▲                                              │
smtp submission ◀──send── kastellan-worker-email-out │                                    reply ◀───┘
(public host:587,         Net::Allowlist = submission
 webpki only)             host ONLY
```

### 4.1 New components

| Component | Purpose |
|---|---|
| `workers/email-in` (`kastellan-worker-email-in`) | `email.init` / `email.poll` / `email.ack`. Polls the localmail subscription, fetches each new message with `?headers=full`, returns raw material. **Makes no security decisions.** |
| `workers/email-out` (`kastellan-worker-email-out`) | `email.send` over `lettre` **0.11 (MIT — verified 2026-07-28, AGPL-compatible)** SMTP submission. Slice 2. |
| `core/src/channel/email/` | `wire.rs` (`EMAIL_POLLED_SPEC` + pure codecs), `gate.rs` (pure DMARC + token), `policy.rs` (`SandboxPolicy` builders), `config.rs` (env-gated parsing), parent (`EmailChannel`, `spawn_email_worker`). Each file under the 500-LOC cap. |
| migration `0022` | `ALTER TABLE pairings ADD COLUMN token_sha256 TEXT` (nullable). |
| `kastellan-cli pair issue-token --channel email --peer <addr>` | Creates the pairing row, mints a long-lived random token, stores only its SHA-256, prints the plaintext once. Audited as `pairing.token_issued` (hash only). The peer is lowercased at mint time to match the channel's normalization of a `From` header. |

### 4.2 Changes to shared code

All additive or parity-preserving:

* `PolledEvent` and `IncomingMessage` gain `evidence: Option<PeerEvidence>`.
  `None` means "the transport authenticates its own peers" (Matrix: E2E +
  homeserver auth) and the bus applies no extra check.
* `PeerAuthorizer::authorize` gains the evidence parameter.
* `AuthDecision` gains `RejectedUnauthentic(UnauthenticReason)`, distinct from
  `Rejected`. The payload is a fixed, non-secret classification label
  (`dmarc_fail` / `no_evidence` / `no_token` / `token_mismatch` /
  `pairing_has_no_token`) so §4.3's promised reason code actually reaches
  `audit_log` instead of being discarded at the authorizer.
* `channel::actions` gains `REJECTED_UNAUTHENTIC = "channel.rejected_unauthentic"`.
* `PolledWorkerSpec` gains `ack_method: Option<&'static str>` plus an
  `EncodeAck` fn, symmetric with the existing init/poll/send fields.

`DbPeerAuthorizer` fetches the active pairing row. If `token_sha256` is
**NULL** *and* the transport supplied no evidence (`evidence == None`), the
peer is `Recognised` — that is exactly the Matrix shape, so **Matrix is
byte-identical**. **Corrected 2026-07-29 after review:** a NULL token with
evidence **present** is now `RejectedUnauthentic(PairingHasNoToken)`, not
`Recognised`. `evidence.is_some()` is the same "this transport cannot vouch
for its sender" marker D8 uses, and such a peer is admitted purely on the
strength of its per-pairing token — so a row without one is *misconfigured*
for that transport, not permissive. Admitting would have collapsed the entire
email gate (no DMARC check, no token) for that address; nothing creates such a
row today, but no DB `CHECK` or code guard prevented one either.

### 4.3 Inbound data flow, one message

1. `email-in` polls the subscription, then `GET /v1/messages/{id}?headers=full`
   per new id, and returns `message_id`, `from`, `subject`, `date`, the
   `Authentication-Results` headers, `Message-ID`, and the body — verbatim.
   It fetches the body of **every** new message, including ones the gate will
   later reject: the gate lives in core (D6), so the worker cannot know the
   outcome. D4 is what makes that acceptable — the subscription covers one
   dedicated mailbox, not the archive, so the volume is a handful of messages a
   day rather than the user's whole mail stream.
2. Core's pure `parse_email_poll` builds a `PolledEvent`:
   * `peer` = the `From` address, lowercased and normalized. **Never
     `Reply-To`** — honouring that would let a sender who passes the gate
     redirect the agent's reply to a third party.
   * `conversation` = the original `Message-ID`, so the reply can set
     `In-Reply-To`/`References` and thread.
   * `evidence` = `PeerEvidence { dmarc_pass, presented_token }`, from two pure
     fns in `gate.rs`:
     * `trusted_dmarc_pass(headers, authserv_id)` — considers **only the very
       first `Authentication-Results` header in wire order**, and requires its
       authserv-id to equal the configured value. A sender can write arbitrary
       `Authentication-Results` lines into the message they send; our own MX
       prepends its header on receipt, so the genuine verdict is always the
       topmost one. **Corrected 2026-07-29 after review:** the original rule
       was "topmost *matching* header", which let a typo'd configured
       authserv-id skip the genuine header and hand the decision to a forged
       one below it. Anything other than a match on the first header now
       **fails closed**, so a misconfiguration is loud rather than silently
       trusting a forgery. Parsing is quote- and comment-aware (`;` is legal
       inside an RFC 5321 quoted local-part and inside comments), and the
       first `dmarc=` segment wins — `dmarc=fail; dmarc=pass` is a fail.
     * `extract_token(body)` → `(Option<token>, stripped_body)`. **Every**
       case-insensitive occurrence of the token prefix, anywhere in the body,
       is removed from the prefix through end-of-line (a leading BOM is
       stripped first); the first occurrence supplies the presented token. The
       secret is removed **before** the body becomes the instruction, so it
       never reaches a task payload, an LLM prompt, or a quoted reply.
       **Corrected 2026-07-29 after review:** the original rule was
       line-anchored and enumerated `>` as the quote marker, which leaked the
       secret via `|`, `}`, `:`, an inline `you wrote: kastellan-token: …`, and
       a UTF-8 BOM (U+FEFF is not `White_Space`, so `trim_start` misses it).
       Matching anywhere subsumes every marker in one rule. Stated limitation:
       a token split across lines by transport folding cannot be detected.
3. `handle_inbound` authorizes with the evidence. On `RejectedUnauthentic` it
   audits `channel.rejected_unauthentic` (peer + the `UnauthenticReason` label
   only — never the body, never the token) and **skips the pairing carve-out**,
   so a spoofed email cannot even attempt a code claim. **Corrected 2026-07-29
   after review:** this is *not* belt-and-braces — it is the mechanism. The
   original text said D8 already made the carve-out unreachable over email
   because an unpaired sender has no token; that reasoning was refuted (see
   D8). The carve-out is reachable, and the `msg.evidence.is_none()` gate here
   is what closes it.
4. Passing messages continue down the existing path unchanged: Strict injection
   screen → `tasks` payload → agent → `route::reply_body` → `Channel::send`.

### 4.4 Why the gate is where it is

The two halves do different work and neither is sufficient alone:

* **DMARC pass + an exactly-matching paired `From`** is the primary gate. A
  DMARC pass means the sending domain authenticated the message; requiring the
  address to equal the paired one closes the "any user at that domain" hole.
* **The token** is defence in depth against a misconfigured or compromised MX,
  or a domain that permits internal `From` spoofing. It is cheap and it was an
  explicit 2026-06-12 decision.

The trust root is our MX's `Authentication-Results` header, which is why the
configured authserv-id is mandatory and unmatched headers fail closed. This is
the standard posture; the alternative (verifying DKIM in-worker) means DNS
lookups and a crypto dependency inside the jail for a marginal gain over
trusting the MX we already trust to deliver the mail at all.

## 5. localmail-side change (separate PR in that repo)

`/v1/changes` is a stateless tail: **with no `since` it returns the 200 most
recent messages**, so a worker respawn without a cursor would re-deliver up to
200 emails *as tasks*. Rather than compensate in kastellan with a durable
cursor table, a `PolledWorkerDriver` `init_params` extension, and core-side
de-duplication, localmail gains a named subscription per api-user:

* `GET /v1/changes?subscription=<name>` — returns only messages after that
  subscription's stored cursor.
* `POST /v1/changes/ack {cursor}` — advances it.

This is compatible with the existing tail-only contract in `changes.py` (it
adds no `min_id`/`before` backfill parameter). kastellan then holds **no
inbound position state at all**: poll, process, ack.

Delivery is at-least-once: a crash between enqueue and ack redelivers one
message. That is unavoidable in any two-party design and is stated plainly
rather than engineered around.

## 6. Ack semantics and failure behaviour

The driver calls `ack_method` once an event is accepted by the bus's inbound
channel. **Known residual:** if the bus then fails to *enqueue*, the message is
acked but lost. This is not new — `handle_inbound` already logs `channel
enqueue failed; message dropped` and Matrix has the identical property. Matching
existing semantics is preferred over inventing a receipt mechanism for one
channel.

| Failure | Behaviour |
|---|---|
| Worker dies | `PersistentWorker` respawns with backoff; **nothing lost** — the unacked cursor is localmail's |
| localmail down | Poll errors; driver logs the down/up transition once and retries; no loss |
| One message's `message_detail` fails **transiently** (5xx, 408, 429, transport) | Omitted from **both** `events` and `skipped`, so *nothing in that message's own path* acks it — redelivered next poll. The rest of the batch still processes (`workers/email-in`'s `is_permanent`). **Known residual, accepted:** the cursor is shared and monotonic (`GREATEST`), so a *later* message in the same poll that succeeds can still drag it past this hole. Closing that needs a per-message ack contract with localmail; it is strictly better than the previous behaviour, where every failure was acked away |
| One message's `message_detail` fails **permanently** (4xx other than 408/429) | Recorded in `skipped`, acked + audited `channel.skipped_ack_only`, so one poisoned message cannot wedge the channel forever |
| Malformed poll result | Batch skipped + logged (existing driver behaviour) and left *unacked*, so it retries |
| Gate fails | Dropped, audited `channel.rejected_unauthentic` with the `UnauthenticReason` label (`dmarc_fail` / `no_evidence` / `no_token` / `token_mismatch` / `pairing_has_no_token`) — reason code only, never the body, never the token |
| SMTP send fails (slice 2) | The driver's existing `pending` retention holds the reply across a respawn |
| Config incomplete | The daemon refuses to start **the email channel** — a loud `error!` naming every missing variable, then the daemon comes up **without** it. Deliberately *not* an abort: this is the fallback channel (it exists because Matrix has no homeserver failover), so its misconfiguration must not remove the primary one, and a half-configured channel already fails closed anyway (a blank authserv-id rejects every message). Unset config ⇒ channel absent ⇒ byte-identical to today |

## 7. Testing

* **`gate.rs` units** — forged extra `Authentication-Results` headers,
  authserv-id mismatch, no matching header, multiple headers; token
  absent/duplicated/trailing; body verifiably stripped of the secret.
* **`DbPeerAuthorizer` (PG)** — NULL `token_sha256` **with no evidence** ⇒
  `Recognised`, pinning **Matrix parity**; NULL `token_sha256` **with**
  evidence ⇒ `RejectedUnauthentic(PairingHasNoToken)`, asserted for both a
  hostile and a good-looking evidence value so the guard cannot be weakened to
  "bad evidence only"; non-NULL ⇒ DMARC *and* token both required; wrong token,
  missing token, and missing evidence each rejected with their own reason.
* **Bus** — `RejectedUnauthentic` audits the new action **with the right reason
  code for each arm**, never carries the body or token, **and never reaches the
  pairing carve-out**.
* **Worker units** over the `web-common` fake-HTTP seam, as `mail` does.
* **Hermetic e2e** `core/tests/email_channel_e2e.rs` — fake worker process +
  the existing `tests-common::mock_localmail`, mirroring `matrix_channel_e2e`.
* **Negative controls** — each gate assertion must fail against deliberately
  weakened code (the pattern #492 established).
* **`#[ignore]` live DGX tier** (slice 3).

## 8. Slices

**Slice 1 — gated inbound (one branch).** localmail subscription endpoints
(separate PR in that repo); `workers/email-in`; `core/src/channel/email/`;
migration 0022; `pair issue --channel email`; the authorizer evidence parameter
+ `AuthDecision::RejectedUnauthentic` + bus audit; the driver `ack_method`
extension; hermetic e2e; config-gated daemon wiring. `Channel::send` returns
"not configured" — no replies yet. The security gate is never the deferred half.

**Slice 2 — outbound.** `workers/email-out` (`lettre`), driver routing `send` to
a second worker handle, full round trip.

**Slice 3 — live.** DGX deployment (dedicated mail account, channel api-user +
grant, `kastellan.env`), plus the `#[ignore]` live tier.

## 9. Operator setup (slice 3)

1. A dedicated mail account in localmail for the agent's address, and a channel
   api-user granted **only** that account.
2. `kastellan-cli pair issue-token --channel email --peer <your-address>` —
   record the printed token; it is shown once.
3. `kastellan.env`: localmail endpoint, subscription name, the agent address,
   the **authserv-id of your MX**, and (slice 2) the SMTP submission host and
   credentials.
4. Because the localmail origin is a private IP literal with a self-signed cert,
   `KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA` must key that literal — and per #492 the
   worker's allowlist must resolve to that **single** private origin, which the
   two-worker split guarantees.
