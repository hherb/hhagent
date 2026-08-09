# Make `mail.get_message` accept the ids localmail emits (#527) + fix `full_headers` (#500)

**Date:** 2026-08-09 · **Branch:** `fix/527-500-mail-id-contract` · **Closes:** [#527](https://github.com/hherb/kastellan/issues/527), [#500](https://github.com/hherb/kastellan/issues/500)

Both defects live in the same `get_message` params struct
(`workers/mail/src/handler.rs:58-68`), so they are one sitting. Both were
filed from reading code; both were re-measured against the live DGX before
this design was written, and **the measurement changed the fix for #527**.

---

## The measurement

Taken 2026-08-09 against the deployed DGX (`main` = `6e22a470`), the real
localmail at `10.0.0.3:8443`, and the live `audit_log`.

### 1. `mail.get_message` fails 54% of the time

```
mail.get_message   14 failed / 26 dispatched      <- all time
mail.search         0 failed / 37
mail.list_messages  0 failed /  7
mail.list_accounts  0 failed /  4
```

The whole mail problem is one parameter of one method. Every other mail tool
is clean.

### 2. localmail serialises every id as a JSON string

```
GET  /v1/messages      -> {"messages":[{"message_id":"37477", … ,"account":{"id":"1"}}], "next_cursor":"ZHwy…"}
POST /v1/search        -> {"results":[{"message_id":"20973", …}], "next_cursor":"6f6dd7a731…"}
GET  /v1/accounts      -> [{"id":"1","name":"horst-gmail", …}]
GET  /v1/messages/{id} -> {"id":"37477", …}
```

The worker demands `message_id: i64`. So the planner is **not** inferring a
type from prose, which is what #527 assumed — it is faithfully echoing the
shape the previous tool call returned, into a parameter that refuses it.

### 3. The failures split three ways

Every failing dispatch, from `audit_log`:

| Sent | n | Class |
| --- | --- | --- |
| `"17817"` ×2, `"15408"`, `"20070"`, `"23022"`, `"37242"`, `"2562"` | **7** | echo of localmail's string id |
| `"ZHwyMDI2LTA4LTA4VDIyOjAxOjU4KzAwOjAwfDM3NDc0"` ×2, `"3db5c6e23812425c"` | **3** | the adjacent `next_cursor` pasted in as the id |
| `"{{message_id}}"` ×4 | **4** | an assumed template engine |

The base64 value decodes to `d|2026-08-08T22:01:58+00:00|37474` — a paging
cursor that *ends in* a message id, sitting next to `message_id` in the same
response. The hex one is a `/v1/search` cursor, which is hex on that route.

**There is no `{{…}}` substitution mechanism anywhere in the tree.** The
architecture is one step at a time with output fed back and a replan
(`inner_loop`), so the model must emit a literal. And that habit appears on
`mail.get_message` and **on no other tool in the entire audit log** — it is
provoked by this parameter's own description, *"message id from a search/list
hit"*, which says the value comes from another step without naming the field
or saying to inline it.

### 4. `full_headers` confirmed broken against the live service

Not just against localmail's source, which is how #500 was filed:

| Query sent | `headers` key | n headers |
| --- | --- | --- |
| *(none)* | absent | 0 |
| `?headers=full` | **present** | **19** |
| `?full_headers=true` ← what we send | absent | 0 |
| `?headers=compact` | absent | 0 |

### 5. Why no test caught any of this

`tests-common/src/mock_localmail.rs` disagrees with the live service on
exactly the three routes the mail tool uses:

| Route | Mock | Live |
| --- | --- | --- |
| `/v1/search` | `"message_id": 7` (number) | `"20973"` (string) |
| `/v1/messages` (list) | `{"results":[…]}`, number id | `{"messages":[…]}`, **string** id |
| `/v1/accounts` | `"id": 1` | `"1"` |
| `/v1/changes` | `"message_id": "7"` ✓ | ✓ |
| `/v1/messages/{id}` | `"id": "7"` ✓, `headers` gated on `headers=full` ✓ | ✓ |

The two correct routes carry a comment explaining the numeric-vs-string trap
in detail. A previous author hit it, documented it, and fixed the two routes
`email-in` consumes — leaving the three the **mail tool** consumes lying. A
hermetic `search → get_message` chain therefore passes in tests and fails in
production. This is the "sweeping one file is not sweeping the class" finding
from #458's review wave, at a second site.

---

## Part 1 — #527: widen the contract to the shape upstream emits

### `LocalmailId`, a pure newtype in its own module

New file `workers/mail/src/ids.rs`. `handler.rs` is already 450 lines, so the
type does not go there.

- Deserializes from a JSON **number** or a **digit-string** to `i64`.
  Everything else is refused.
- **The accepted grammar, stated exactly** — this is the part that gets
  transcribed into code, so it does not get to be approximate:

  | Input | Verdict |
  | --- | --- |
  | `37477` (JSON number, non-negative integer) | accept |
  | `"37477"` | accept |
  | `"0037"` | accept → `37` (localmail never emits this, but it is unambiguous) |
  | `"-1"`, `"+1"` | **reject** — sign characters are not digits, and no id is negative |
  | `" 37477"`, `"37477 "` | **reject** — no trimming; whitespace means the value came from somewhere it should not have |
  | `""` | reject |
  | `37.0`, `3.5` (JSON float) | reject |
  | `-1` (negative JSON number) | reject |
  | value `> i64::MAX` | reject |
  | `null`, `true`, array, object | reject |

  In short: `[0-9]+` for strings, non-negative integer for numbers. Rejecting
  rather than trimming keeps the newtype a *validator*, not a repair layer —
  the widening is for localmail's own format and nothing else.
- **Preserves today's injection guard exactly.** `i64` is not decoration —
  `get_message` interpolates the id straight into a URL path, so a free
  string would admit `../` and `?`-injection. Widening the *accepted JSON
  types* while keeping the *validated output* is the whole trick: the set of
  values that can reach a URL is unchanged.
- Applied to `message_id`, and to `account_ids` / `folder_ids`, which carry
  the identical latent mismatch (`/v1/accounts` returns `"id":"1"`). They
  have zero observed failures only because the planner has not yet chained
  `list_accounts` into `list_messages`; fixing one and not the others repeats
  the mock's own mistake.

**Accepting is silent** — this is not a repair, it is localmail's own wire
format, so an audit row per call would be noise on what is now the success
path.

### The error is the repair mechanism

`inner_loop` feeds the prior failure back on the next iteration ("the agent
sees the prior failure on the next iteration, bounded by `max_plans`"), so
error *text* is functional, not cosmetic. Today the planner receives a raw
serde string: `bad params: invalid type: string "ZHwy…", expected i64`.

A second pure function classifies the rejected value and says the specific
thing:

| Rejected value | Message |
| --- | --- |
| matches `{{…}}` | there is no template substitution — use the literal value from the previous step's output |
| non-numeric string | this looks like a paging token, not a message id — re-read the `message_id` field of the search/list hit |
| any other JSON type | expected the numeric `message_id`, e.g. `37477` |

Pure over the raw `serde_json::Value`, so each arm is a unit test. This is
what addresses the 7 failures the newtype cannot.

### The description rewrite

Current — and the direct cause of the `{{message_id}}` class:

> `message id from a search/list hit`

Replacement:

> `numeric message_id of a mail.search / mail.list_messages hit, e.g. 37477 (a number or a digit-string). NOT next_cursor — that is a paging token. Use the literal value from the previous step's output, not a placeholder.`

It names the field, gives a concrete example, rules out the adjacent field
that was actually pasted three times, and rules out the placeholder. A narrow
test pins that the rendered `<tools>` block still carries the field name and
the `next_cursor` warning, so a later edit that drops either fails rather
than silently regressing — without pinning the whole prose string, which
would be brittle.

---

## Part 2 — #500: translate at the HTTP boundary

The tool's public JSON parameter stays `full_headers: bool` — that is the
advertised contract and the tool schema. Only the URL changes:

```
full_headers: true  ->  /v1/messages/{id}?headers=full
full_headers: false ->  /v1/messages/{id}                (omit; compact is the default)
```

As a pure `detail_path(id, full_headers) -> String`, unit-testable without a
server. Per the issue's own ask, the worker-side URL pin is paired with a
mock that serves `headers` **only** on the real spelling, so the round trip
asserts against the service's grammar rather than agreeing with the worker.

---

## Part 3 — mock fidelity, and the test that would have caught this

Correct the three lying routes in `tests-common/src/mock_localmail.rs` to the
measured shapes above: string ids on `/v1/search`, `/v1/messages` and
`/v1/accounts`, and the list route's `messages` key in place of `results`.
The detail and `/v1/changes` routes are already right and are left alone.

Then the durable half — **a chained `search → get_message` test that feeds the
id back exactly as the search response emitted it.** It fails today with
`expected i64` and passes after. Plus shape pins for the three corrected
routes, in the style of the existing
`changes_returns_message_id_and_next_cursor_as_strings`.

**The pins live in `tests-common` deliberately.** `linux-check.yml` is
compile-only apart from `cargo test -p kastellan-tests-common`, so that crate
is the only place a guard runs on **every PR** — the same argument that put
#504's installer-coverage guard there. A defect whose character is "the mock
agrees with the worker instead of the service" is exactly the kind that
survives every check not specifically looking for it.

### Expected breakage — each one is a real finding

- `workers/mail/tests/mail_e2e.rs:43,118` — the inline fixture
  `{"message_id":7}` and the assertion `message_id == 7`, which currently
  encode the fiction.
- `workers/mail/src/handler.rs:299-308` — inline mock in the unit tests.
- Any list-shape assertion keyed on `results` in `core/tests/mail_e2e.rs` or
  `core/tests/mail_daemon_e2e.rs`.

---

## Testing

TDD, in this order:

1. **RED** — the chained `search → get_message` test against the corrected
   mock. It must fail with today's `expected i64`; that failure is the
   in-repo reproduction of the live 54%.
2. `LocalmailId` + one unit test per row of the grammar table above.
   **GREEN.**
3. **RED** — `detail_path` expecting `?headers=full`; mock serves `headers`
   only on that spelling. **GREEN.**
4. Error-classifier unit tests, one per arm.
5. Route shape pins in `tests-common`.
6. The narrow tools-block pin for the description.

## Verification

- **Two-host gate** with a stated test-count prediction up front. If the
  landed count differs, investigate rather than accept it — the #458 gate
  came in +2 over prediction and the +2 was the point of the change.
- **Live acceptance on the DGX** against the real 37k-message archive.
  Baseline to beat: **14/26 (54%)** failing `mail.get_message` dispatches.
  Re-measure from `audit_log` after deploy, and confirm the three classes
  individually — a numeric-string id now succeeds, and a cursor or
  placeholder now produces the actionable error rather than the serde one.
- Run the live tier on a **quiet** DGX. A full-workspace `cargo test` on the
  same box makes a live task look like the runaway-thinking bug.

## Declined, with evidence

**A planner-prompt line about template substitution.** The `{{…}}` habit
appears on `mail.get_message` and nowhere else in the audit log, so it is
provoked by one description; a base-prompt change is a far wider blast radius
than the evidence supports. If the pattern appears on a second tool after
this ships, that is the signal to revisit.

**Normalising ids to integers on the way *out* of `mail.search`.** It would
make the planner see and copy numbers, but it means the worker rewriting
upstream response bodies — diverging the tool's output from the documented
service, and taking on a transformation that must then track every localmail
shape change. Widening the input is the smaller and more robust half of
Postel's law here.

## Out of scope, filed

- [#534](https://github.com/hherb/kastellan/issues/534) — `ToolParam` carries
  no type. #527 proposed this as its systemic fix; the measurement shows it
  would have prevented at most 7 of the 14 failures, and only by asking the
  model to contradict its own input, so it is split out to be judged on its
  own merits.
- [#533](https://github.com/hherb/kastellan/issues/533) — `shell.exec` refuses
  **40 of 81** live dispatches, because the planner is never shown the argv0
  allowlist; 11 of those are bare names for binaries that *are* allowlisted.
  Same root class as #527: the advertised contract is narrower than the
  enforced one.
