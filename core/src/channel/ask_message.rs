//! The operator-ask wire vocabulary: what an ask looks like when it is sent
//! into a conversation, and what an answer looks like coming back.
//!
//! **Entirely pure** — no DB, no I/O, no clock. The bus and the scheduler
//! each own one direction of the loop, and both of them are hard to test;
//! everything that can be decided without them is decided here.
//!
//! Spec: `docs/superpowers/specs/2026-08-19-ask-channel-slice-2-design.md`.

use serde_json::Value;
use time::OffsetDateTime;

use super::{ChannelId, ConversationId, PeerId};

/// Byte cap on the model-authored concern text in a rendered ask (spec D11).
///
/// A legibility bound, not a containment one: the message goes to a paired
/// operator in their own room. What it protects is the two command lines,
/// which an unbounded concern would push off the visible message — and an
/// approval nobody can see the command for is an approval nobody gives.
///
/// Bounds the concern *before* [`fence`] prefixes it, so the fenced text
/// can be up to three times this on a concern that is all newlines. That is
/// deliberate: capping the fenced form would make the amount of concern an
/// operator sees depend on how many line breaks the model happened to emit,
/// and 1.5 KiB still leaves the commands on screen.
pub const CONCERN_CAP: usize = 512;

/// The sentence sent back when an answer resolved nothing.
///
/// Deliberately one sentence for four causes — wrong token, already
/// answered, past its deadline, not this peer's ask (spec D9).
/// `resolve_with_nonce` refuses to distinguish them because splitting them
/// hands a token-guessing peer an existence oracle over ask ids; naming the
/// cause here would give back at the presentation layer exactly what the
/// query gives up.
pub const ACK_NOT_ANSWERABLE: &str =
    "\u{2717} That approval token isn't answerable. It may be mistyped, or the question \
     may no longer be open.";

/// The sentence sent back when a body looks like an attempted answer —
/// its first whitespace token is `/approve` or `/deny` — but does not
/// parse as one (extra words, a missing token, ...).
///
/// **Deliberately distinct from [`ACK_NOT_ANSWERABLE`].** That sentence is
/// vague on purpose, because a well-formed command that resolves nothing
/// must not become an existence oracle. This one is a syntax problem, not
/// a resolution outcome — the peer typed something extra (`/approve tok9
/// thanks!` is exactly what a person types), and there is nothing to leak
/// by telling them plainly what the two valid shapes are. Never echoes the
/// body or any part of a token.
///
/// Existing to close a capability leak, not just for politeness: without
/// an arm that catches this shape, a malformed command falls through to
/// `screen_and_classify` and gets enqueued — writing a **live** approval
/// token verbatim into `tasks.payload` (a durable column with no DELETE
/// grant) and handing it to the planner as an instruction.
///
/// **`TOKEN`, not `<token>`, and that is load-bearing (#583).** Element
/// parses an angle-bracketed placeholder as an unknown HTML tag and drops
/// it from the sender's own timeline. An operator who copied the old hint
/// literally sent a two-token command that parsed, resolved nothing, and
/// came back as the deliberately vague [`ACK_NOT_ANSWERABLE`] — while
/// their screen showed a one-token `/approve`, which by the documented
/// design should have produced *this* sentence instead. The reply
/// contradicted the message they could read back, and the real cause was
/// invisible on both ends. Pinned by
/// `the_usage_hint_carries_no_html_metasyntax`.
pub const ACK_MALFORMED_COMMAND: &str =
    "Usage: /approve TOKEN or /deny TOKEN \u{2014} exactly the verb and the token, \
     nothing else.";

/// Where an ask is delivered: the channel, peer and conversation of the
/// task that raised it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskDestination {
    pub channel: ChannelId,
    pub peer: PeerId,
    pub conversation: ConversationId,
}

/// The routing metadata on a channel-originated `tasks.payload`, or `None`
/// for a task that did not come from a channel (spec D3).
///
/// **Shared with [`super::route::reply_for_completed_task`] on purpose**
/// (spec D10): the place an ask is *delivered* and the place its task's
/// answer is *replied to* read the same four keys off the same row, and a
/// second copy would drift the first time either grew a field.
pub fn destination_from_task_payload(payload: &Value) -> Option<AskDestination> {
    if payload.get("kind").and_then(Value::as_str) != Some("channel") {
        return None;
    }
    Some(AskDestination {
        channel: ChannelId(payload.get("channel").and_then(Value::as_str)?.to_string()),
        peer: PeerId(payload.get("peer").and_then(Value::as_str)?.to_string()),
        conversation: ConversationId(
            payload.get("conversation").and_then(Value::as_str)?.to_string(),
        ),
    })
}

/// The two answers a `plan_approval` ask offers, in their wire spelling.
///
/// Deliberately distinct from `scheduler::asks::Choice`, which reads a
/// *stored* resolution: these are different layers (the wire vocabulary vs.
/// the record). Note the dependency only runs one way — the scheduler
/// already imports from this module (`asks::lifecycle`, `asks::pure`), so
/// what merging them would cost is the layering, not a cycle.
///
/// The agreement with `asks.options` — which `db::asks::resolve_with_nonce`
/// validates a submitted choice against — is **structural on the write
/// side**: `raise_and_suspend` builds that array by calling [`Self::as_str`],
/// so a respelling here respells both at once.
///
/// It is *not* automatically structural on the read side, and an earlier
/// version of this doc claimed it was without qualification.
/// `scheduler::asks::resolution_choice` has to map the stored string back to
/// a variant, and while it did so with hand-typed literals a respelling here
/// left it returning `None` → `NotForThisPlan` → the task re-asking a
/// question the operator had already answered. It now compares against
/// [`Self::as_str`] for that reason. `the_wire_verbs_are_the_stored_choices`
/// still pins today's two spellings, because the stored value is durable and
/// a rename would strand every ask raised before it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AskChoice {
    Approve,
    Deny,
}

impl AskChoice {
    /// The stored `resolution.choice` value. Must match `asks.options`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Deny => "deny",
        }
    }
}

/// A parsed answer: which verb, and the opaque correlation token.
///
/// `Debug` is hand-written to **redact `token`**: it is the live, plaintext
/// correlation token off the wire, one boundary before it becomes a
/// `db::asks::Nonce` — whose own hand-written `Debug` prints
/// `Nonce(<redacted>)` for the same reason. See also [`super::PeerEvidence`]
/// elsewhere in this module tree, which redacts `presented_token` the same
/// way. Derive-by-default is the wrong side of that trade for a
/// secret-bearing type: the whole point of a `Debug` impl is that someone
/// eventually reaches for it, and a `?cmd` in a `tracing` call on the
/// inbound path would otherwise write the live approval token to the
/// daemon log.
#[derive(Clone, Eq, PartialEq)]
pub struct AskCommand {
    pub choice: AskChoice,
    /// Taken verbatim off the wire. Not a `Nonce` yet — this module is pure
    /// and the `db` newtype zeroizes on drop; the conversion happens at the
    /// resolver boundary, which is the only place that should hold one.
    pub token: String,
}

impl std::fmt::Debug for AskCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskCommand")
            .field("choice", &self.choice)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Recognise `/approve <token>` or `/deny <token>`, or `None` for anything
/// else.
///
/// `None` does **not** mean "ordinary message". It splits in two, and
/// [`looks_like_ask_command`] is what tells the halves apart: a body whose
/// first token is one of the two verbs is a malformed *attempt* and gets
/// [`ACK_MALFORMED_COMMAND`] without ever reaching the enqueue path
/// (enqueueing `/approve tok9 thanks!` would write a live token into
/// `tasks.payload`); everything else is an ordinary message and takes the
/// normal screen-and-enqueue path. `handle_inbound` applies that split.
///
/// **Strict: the trimmed body must be exactly two whitespace-separated
/// tokens.** Accepting a trailing tail would let one message both resolve
/// an ask and read, to the operator scrolling past it, as if it had said
/// something else.
///
/// **No shape check on the token** (spec D7). `Nonce::from_wire`'s doc
/// states the rule: `resolve_with_nonce`'s `WHERE` predicate is the only
/// thing entitled to decide whether a token is real. A syntactic pre-check
/// would also couple this parser to the nonce *encoding*, so a change to
/// `generate_nonce` would silently stop every answer from parsing while
/// every test of the resolver still passed.
pub fn parse_ask_command(body: &str) -> Option<AskCommand> {
    let mut parts = body.split_whitespace();
    let verb = parts.next()?;
    let token = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let choice = match verb.to_ascii_lowercase().as_str() {
        "/approve" => AskChoice::Approve,
        "/deny" => AskChoice::Deny,
        _ => return None,
    };
    Some(AskCommand { choice, token: token.to_string() })
}

/// True when **any** whitespace token of `body` is `/approve` or `/deny`
/// (case-insensitive, ignoring leading quote markers), whether or not the
/// whole body goes on to parse as a command.
///
/// The bus uses this to tell "an attempted answer that is malformed" apart
/// from "an ordinary message" when [`parse_ask_command`] returns `None`.
/// Those two must not be treated the same: an ordinary message may safely
/// fall through to screening and enqueue, but a malformed *attempt* may
/// still carry a live approval token somewhere in the body, and enqueueing
/// it would write that token verbatim into `tasks.payload` — a durable
/// column with no DELETE grant — and hand it to the planner.
///
/// **Why the whole body and not just the leading verb.** The first version
/// of this checked `split_whitespace().next()`, which was shaped to the
/// instance that was reported (`/approve tok9 thanks!`) rather than to the
/// property. Three shapes an operator actually produces slipped straight
/// past it into the task queue, each carrying a live token:
///
/// - **A quoted reply.** Element's plain-text rich-reply fallback prefixes
///   every quoted line with `> `, and the message being replied to *is* the
///   rendered ask — both command lines included. `workers/matrix` forwards
///   `text.body` raw (nothing calls ruma's `remove_plain_reply_fallback`),
///   so the first token is `>`. Hitting *reply* on the ask is arguably the
///   most natural way to answer it.
/// - **A leading mention.** Element renders a start-of-message pill into
///   the plain body as the display name, so addressing the bot before the
///   command is a certain trigger.
/// - **Prose around the command**: `should I /approve 7f3a9c2e1b ?`
///
/// The cost is a false positive: an ordinary instruction that happens to
/// contain a bare `/approve` token is now refused with
/// [`ACK_MALFORMED_COMMAND`] instead of being enqueued. That is the right
/// side to err on — such a message carries no live token, so the refusal
/// costs one rephrase, while the miss costs a durable secret. The refusal
/// is visible and self-explaining, which the old fall-through was not: an
/// enqueued "answer" got no acknowledgement at all, so the operator
/// believed they had approved while the task sat suspended until it
/// expired.
///
/// On email the refusal is *not* visible, because `EmailChannel::send`
/// still bails unconditionally — so a quoting thread reply that mentions
/// `/approve` is dropped silently. Accepted, and recorded as a residual in
/// the slice-2 spec's "Open risks" rather than left implicit.
///
/// **This whole predicate is a guess, and #582 replaces it with the exact
/// question** — does any token in the body hash to a live nonce? Do not
/// widen this function a third time; that only trades a false negative for
/// a bigger false positive. If a new leaking shape turns up, add it to
/// #582's motivation rather than to the match arm here.
///
/// Still no *shape* check on the token — the same reason
/// [`parse_ask_command`] does none (spec D7) applies here: this function's
/// job is to recognise an *attempt*, not to validate one.
pub fn looks_like_ask_command(body: &str) -> bool {
    body.split_whitespace().any(|tok| {
        // `>` is the reply-fallback / email-quote marker, `|` a diff or
        // table gutter. Stripped so a quoted command is still recognised
        // when the client emits no space after the marker.
        let tok = tok.trim_start_matches(['>', '|']);
        tok.eq_ignore_ascii_case("/approve") || tok.eq_ignore_ascii_case("/deny")
    })
}

/// The message an escalated plan sends into its task's conversation.
///
/// The two command lines are printed complete so answering is a copy, not a
/// transcription. **The ask id is deliberately absent**: it is not needed to
/// answer, and putting a small sequential integer in durable room history
/// invites exactly the resolve-by-id thinking `db::asks::resolve`'s doc
/// reserves for the local CLI.
///
/// The concern is model-authored, so it is clamped ([`CONCERN_CAP`]) and
/// then [`fence`]d — see that function for why splicing it verbatim let a
/// reviewer's `reason` print its own `/approve` lines.
pub fn render_ask(
    task_id: i64,
    concern: &str,
    token: &str,
    deadline_at: OffsetDateTime,
) -> String {
    format!(
        "\u{26a0}\u{fe0f} Approval needed \u{2014} task {task_id}\n\
         \n\
         An operator decision is required before I continue:\n\
         {concern}\n\
         \n\
         This expires {deadline}. Reply with one of:\n\
         \n\
         /approve {token}\n\
         /deny {token}",
        concern = fence(&clamp(concern, CONCERN_CAP)),
        deadline = render_deadline(deadline_at),
    )
}

/// The deadline as an operator should read it: RFC 3339, whole seconds.
///
/// **`OffsetDateTime`'s `Display` is not a wire format.** In `time` 0.3.49
/// the hour is unpadded and a subsecond fraction is *always* emitted, so a
/// deadline taken from `now_utc()` renders as
/// `2026-08-21 9:14:32.482913571 +00:00:00`. It went unnoticed for the
/// dull reason: **nothing asserted the deadline at all**, so no fixture
/// could have caught it. (`Display` writes the `.` unconditionally — a
/// whole-second instant still renders `…20.0 +00:00:00`, so the old
/// fixture did not hide this by agreeing with RFC 3339; it merely looked
/// unremarkable to a human reading the test.)
///
/// The nanoseconds are zeroed rather than formatted away: a 24-hour
/// approval deadline has no business claiming nanosecond precision, and
/// RFC 3339 would otherwise print all nine digits.
///
/// Falls back to `to_string()` instead of unwrapping, on both steps. This
/// runs on the delivery path of the one message a suspended task is waiting
/// for; an ugly deadline is recoverable, a panicking renderer is a task
/// nobody can approve.
fn render_deadline(at: OffsetDateTime) -> String {
    at.replace_nanosecond(0)
        .ok()
        .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_else(|| at.to_string())
}

/// Quote every line of the concern so none of them can read — or parse — as
/// a command.
///
/// The concern is the reviewer's `reason`: model-authored text, spliced into
/// a message whose other content is two `/`-leading command lines. Without
/// this, a `reason` containing a line that starts with `/approve` puts extra
/// command-shaped lines in the operator's room. The forged tokens resolve
/// nothing (`resolve_with_nonce` is the only thing that decides that), so
/// this is operator confusion rather than compromise — but "exactly two
/// commands are offered" is an invariant the tests assert and production
/// should therefore actually hold, not merely happen to.
///
/// **Splits on more than `str::lines` does, deliberately.** `lines()` breaks
/// on `\n` only (stripping a trailing `\r`), so a `reason` carrying a bare
/// `\r`, U+2028 LINE SEPARATOR or U+2029 PARAGRAPH SEPARATOR was one "line"
/// here and got a single `> ` prefix — while a client that treats those code
/// points as breaks rendered the text after them as its own unfenced,
/// command-shaped line. The obvious test could not catch it either, because
/// it measured the result with `lines()` too and so shared the assumption it
/// was supposed to be checking.
///
/// Costs at most two bytes per line; see [`CONCERN_CAP`] for why the cap is
/// applied before this rather than after.
fn fence(concern: &str) -> String {
    // CRLF collapsed first so a `\r\n` break yields one segment, not an
    // empty one between the two characters.
    concern
        .replace("\r\n", "\n")
        .split(['\n', '\r', '\u{2028}', '\u{2029}'])
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The acknowledgement for an answer that resolved an ask.
pub fn ack_resolved(choice: AskChoice, task_id: i64) -> String {
    match choice {
        AskChoice::Approve => format!("\u{2713} Approved \u{2014} task {task_id} is resuming."),
        AskChoice::Deny => format!("\u{2713} Denied \u{2014} task {task_id} will not proceed."),
    }
}

/// Truncate to at most `cap` bytes on a char boundary, marking the cut.
///
/// Char-boundary aware because the concern is free text in any language and
/// slicing a `String` mid-character panics. The marker is what keeps a
/// clamped concern visibly clamped rather than a sentence that just stops.
fn clamp(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\u{2026} (clamped)", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload() -> serde_json::Value {
        json!({
            "kind": "channel", "instruction": "hi",
            "channel": "matrix", "peer": "@horst:srv", "conversation": "!room:srv",
        })
    }

    #[test]
    fn a_channel_payload_yields_its_destination() {
        let d = destination_from_task_payload(&payload()).expect("destination");
        assert_eq!(d.channel.0, "matrix");
        assert_eq!(d.peer.0, "@horst:srv");
        assert_eq!(d.conversation.0, "!room:srv");
    }

    #[test]
    fn a_non_channel_task_has_no_destination() {
        assert!(destination_from_task_payload(&json!({"kind": "ask", "instruction": "hi"})).is_none());
    }

    /// Each of the three routing fields is individually required: a payload
    /// missing any one of them is not routable, and a partial destination
    /// would send a question into a conversation it cannot be answered from.
    #[test]
    fn each_missing_routing_field_defeats_the_destination() {
        for drop_key in ["channel", "peer", "conversation"] {
            let mut p = payload();
            p.as_object_mut().unwrap().remove(drop_key);
            assert!(
                destination_from_task_payload(&p).is_none(),
                "payload without {drop_key} must not be routable",
            );
        }
    }

    #[test]
    fn both_verbs_parse_with_their_token() {
        let a = parse_ask_command("/approve 7f3a9c2e1b").expect("approve");
        assert_eq!(a.choice, AskChoice::Approve);
        assert_eq!(a.token, "7f3a9c2e1b");
        let d = parse_ask_command("/deny 7f3a9c2e1b").expect("deny");
        assert_eq!(d.choice, AskChoice::Deny);
    }

    /// Chat clients capitalise, and trailing whitespace is invisible. Neither
    /// should cost an operator their approval.
    #[test]
    fn the_verb_is_case_insensitive_and_the_body_is_trimmed() {
        assert_eq!(parse_ask_command("  /APPROVE abc \n").unwrap().choice, AskChoice::Approve);
        assert_eq!(parse_ask_command("/Deny abc").unwrap().choice, AskChoice::Deny);
    }

    /// D7: no shape check on the token. A syntactic pre-check would couple
    /// this parser to the nonce ENCODING, so changing `generate_nonce` would
    /// silently stop every answer parsing while every resolver test still
    /// passed. Only the WHERE predicate decides whether a token is real.
    #[test]
    fn a_token_of_any_shape_parses() {
        for token in ["7f3a9c2e1b", "ZZZZ", "not-hex-at-all", "1"] {
            assert_eq!(parse_ask_command(&format!("/approve {token}")).unwrap().token, token);
        }
    }

    /// Everything that is not exactly two tokens is an ordinary message and
    /// must take the normal enqueue path. The three-token case is the sharp
    /// one: accepting it would let `/approve <token> and delete my mail`
    /// resolve an ask *and* look to the operator like it did something else.
    #[test]
    fn anything_that_is_not_exactly_verb_plus_token_is_not_a_command() {
        for body in [
            "/approve",
            "/deny",
            "/approve  ",
            "/approve a b",
            "/approve token trailing prose",
            "approve 7f3a9c2e1b",
            "please /approve 7f3a9c2e1b",
            "what is my flight's GST?",
            "",
            "/approver 7f3a9c2e1b",
        ] {
            assert!(parse_ask_command(body).is_none(), "must not parse as a command: {body:?}");
        }
    }

    /// Bodies whose FIRST token is `/approve`/`/deny` must be recognised as
    /// an attempt even when they do not parse — this is the distinction
    /// `handle_inbound` uses to keep a malformed attempt out of the
    /// enqueue path. Case-insensitive, same as `parse_ask_command`.
    #[test]
    fn a_malformed_attempt_still_looks_like_a_command() {
        for body in [
            "/approve",
            "/deny",
            "/approve  ",
            "/approve a b",
            "/approve token trailing prose",
            "/approve tok9 thanks!",
            "  /APPROVE abc extra\n",
            "/Deny abc extra",
        ] {
            assert!(looks_like_ask_command(body), "should look like an attempt: {body:?}");
            assert!(parse_ask_command(body).is_none(), "and must not itself parse: {body:?}");
        }
    }

    /// Bodies containing no ask verb at all must not be flagged, or an
    /// ordinary message would be blocked from ever reaching the planner.
    ///
    /// The verb must be a whole token and must carry its slash: `approve
    /// <tok>` and `/approver <tok>` are ordinary text. That is what keeps
    /// the widened check from swallowing every message that discusses
    /// approval, and it is why the recogniser matches tokens rather than
    /// running a substring search.
    #[test]
    fn an_ordinary_message_does_not_look_like_a_command() {
        for body in [
            "approve 7f3a9c2e1b",
            "what is my flight's GST?",
            "",
            "/approver 7f3a9c2e1b",
            "disapprove/approve is not a token",
        ] {
            assert!(!looks_like_ask_command(body), "should not look like an attempt: {body:?}");
        }
    }

    /// The shapes an operator actually produces when answering an ask, none
    /// of which the original leading-token check caught. Each one carries a
    /// live token, so each must be intercepted rather than enqueued —
    /// enqueueing writes the token into `tasks.payload` and hands it to the
    /// planner.
    ///
    /// `> …` is Element's plain-text rich-reply fallback, which quotes the
    /// rendered ask *including both command lines*; the mention form is
    /// what a start-of-message pill flattens to. Both are what a person
    /// gets by answering the message in front of them rather than by
    /// composing a bare command.
    #[test]
    fn the_shapes_a_person_actually_sends_are_all_recognised() {
        for body in [
            "> <@kastellan:srv> \u{26a0} Approval needed \u{2014} task 412\n> /approve 7f3a9c2e1b",
            ">/approve 7f3a9c2e1b",
            "@kastellan /approve 7f3a9c2e1b",
            "please /approve 7f3a9c2e1b",
            "should I /deny 7f3a9c2e1b ?",
            "/APPROVE 7f3a9c2e1b thanks!",
        ] {
            assert!(looks_like_ask_command(body), "should look like an attempt: {body:?}");
        }
    }

    /// Well-formed commands look like commands too, obviously — this pins
    /// that the two functions never disagree on the shapes both accept.
    #[test]
    fn a_well_formed_command_also_looks_like_one() {
        assert!(looks_like_ask_command("/approve tok9"));
        assert!(looks_like_ask_command("/deny tok9"));
    }

    /// The malformed-command ack must never echo the body it is refusing.
    #[test]
    fn the_malformed_ack_names_no_part_of_the_body() {
        assert!(!ACK_MALFORMED_COMMAND.contains("tok9"));
        assert!(!ACK_MALFORMED_COMMAND.to_lowercase().contains("thanks"));
    }

    /// Element parses `<token>` as an unknown HTML tag and **drops it from
    /// the sender's own timeline**, so an operator who transcribes this
    /// hint literally sends `/approve <token>` — two tokens, so it parses,
    /// so it resolves nothing, so they get the deliberately vague
    /// [`ACK_NOT_ANSWERABLE`] while their own screen shows only
    /// `/approve`. The reply contradicts the message they can read back,
    /// and the cause is invisible on both ends (#583).
    ///
    /// A plain uppercase word survives HTML rendering intact and still
    /// reads as a placeholder. Pinned here because the failure is
    /// invisible from inside the process: every test passes, and only a
    /// real client shows it.
    #[test]
    fn the_usage_hint_carries_no_html_metasyntax() {
        assert!(
            !ACK_MALFORMED_COMMAND.contains('<'),
            "a `<...>` placeholder is eaten by Matrix clients: {ACK_MALFORMED_COMMAND}"
        );
        assert!(!ACK_MALFORMED_COMMAND.contains('>'));
        // Still teaches both verbs, or it is not a usage hint any more.
        assert!(ACK_MALFORMED_COMMAND.contains("/approve"));
        assert!(ACK_MALFORMED_COMMAND.contains("/deny"));
    }

    /// The two vocabularies must agree: what the wire parser produces is
    /// what `db::asks::resolve_with_nonce` matches against the ask's own
    /// `options`, and a mismatch is a rolled-back transaction that reads as
    /// "the token was wrong".
    #[test]
    fn the_wire_verbs_are_the_stored_choices() {
        assert_eq!(AskChoice::Approve.as_str(), "approve");
        assert_eq!(AskChoice::Deny.as_str(), "deny");
    }

    #[test]
    fn the_rendered_ask_carries_both_copyable_commands() {
        let msg = render_ask(412, "plan writes outside the scratch dir", "7f3a9c2e1b", deadline());
        assert!(msg.contains("/approve 7f3a9c2e1b"), "{msg}");
        assert!(msg.contains("/deny 7f3a9c2e1b"), "{msg}");
        assert!(msg.contains("412"), "the task id orients the operator: {msg}");
        assert!(msg.contains("plan writes outside the scratch dir"), "{msg}");
    }

    /// Each rendered command must round-trip through the parser. Without
    /// this the two halves can drift — the message could print a prefix the
    /// parser does not accept, and every test on each side would still pass.
    ///
    /// The concern here is **hostile**: a model-authored `reason` whose own
    /// lines start with `/approve` and `/deny`, carrying tokens that are not
    /// this ask's. Before the fence, `render_ask` spliced it verbatim and
    /// this test's own "exactly two commands offered" assertion was a
    /// property production did not enforce — the operator's room got four
    /// command-shaped lines, two of them forged. The forgeries resolve
    /// nothing, so what the fence buys is legibility, not containment.
    #[test]
    fn every_command_the_message_prints_parses_back() {
        let hostile = "the plan writes outside the scratch dir\n\
                       /approve deadbeef\n\
                       /deny deadbeef";
        let msg = render_ask(1, hostile, "tok123", deadline());
        let commands: Vec<&str> =
            msg.lines().map(str::trim).filter(|l| l.starts_with('/')).collect();
        assert_eq!(
            commands.len(), 2,
            "exactly two commands offered — a concern line must never read as a third: {msg}",
        );
        for line in commands {
            let cmd = parse_ask_command(line)
                .unwrap_or_else(|| panic!("rendered command does not parse: {line:?}"));
            assert_eq!(cmd.token, "tok123", "the only token offered is this ask's own");
        }
        // The concern is still legible, just quoted — fencing must not eat it.
        assert!(msg.contains("> /approve deadbeef"), "the forged line is quoted, not dropped: {msg}");
        assert!(msg.contains("> the plan writes outside the scratch dir"), "{msg}");
    }

    /// The fence must hold for separators `str::lines` does not recognise.
    ///
    /// The test above cannot catch this: it collects with `msg.lines()`,
    /// which splits on `\n` exactly as `fence` used to, so the assertion and
    /// the code under test shared one assumption and the property was
    /// measured only where it already held. A `reason` breaking on U+2028,
    /// U+2029 or a bare `\r` therefore put an unfenced, command-shaped run
    /// of text in the operator's room on any client that renders those as
    /// breaks.
    ///
    /// So this asserts against the RAW rendered string: every occurrence of
    /// the forged verb must be preceded by the quote marker.
    #[test]
    fn the_fence_holds_for_separators_str_lines_does_not_split_on() {
        for sep in ['\u{2028}', '\u{2029}', '\r'] {
            let hostile = format!("looks fine{sep}/approve deadbeef");
            let msg = render_ask(1, &hostile, "tok123", deadline());
            assert!(
                msg.contains("> /approve deadbeef"),
                "the forged line must be fenced when split by {sep:?}: {msg}",
            );
            assert!(
                !msg.contains(&format!("{sep}/approve")),
                "an unfenced command must not survive the {sep:?} separator: {msg}",
            );
        }
    }

    /// The deadline must actually reach the operator, and be readable when
    /// it does.
    ///
    /// Two regressions this pins at once. **First**, the whole `This expires
    /// {deadline}. Reply with one of:` line could be deleted and every other
    /// test in this module stayed green — nothing asserted the deadline
    /// appeared at all. **Second**, `OffsetDateTime`'s `Display` always
    /// emits a subsecond fraction and an unpadded hour, so a live deadline
    /// rendered as `2026-08-21 9:14:32.482913571 +00:00:00`. The two are
    /// one story: the *first* is why the second survived to the final
    /// review. A whole-second fixture does not hide a `Display` render —
    /// `Display` writes the `.` unconditionally, so it still shows
    /// `…20.0 +00:00:00` — it just looks unremarkable to a human, and
    /// nothing was asserting on it either way. Hence [`messy_deadline`]:
    /// a fixture with nanoseconds set, so a revert is loud rather than
    /// merely ugly.
    #[test]
    fn the_deadline_is_rendered_legibly_with_no_nanosecond_noise() {
        let msg = render_ask(412, "c", "tok", messy_deadline());
        assert!(msg.contains("This expires"), "the expiry line must survive: {msg}");
        assert!(
            msg.contains("2026-08-17T20:53:20Z"),
            "the deadline itself must be in the message, RFC 3339: {msg}",
        );
        assert!(
            !has_nine_digit_fraction(&msg),
            "a nanosecond fraction is machine garbage to an operator: {msg}",
        );
    }

    /// `.` followed by nine digits — the shape `Display` produces and RFC
    /// 3339 would too if the nanoseconds were not zeroed first.
    fn has_nine_digit_fraction(s: &str) -> bool {
        s.as_bytes()
            .windows(10)
            .any(|w| w[0] == b'.' && w[1..].iter().all(u8::is_ascii_digit))
    }

    /// The concern is model-authored (it is the reviewer's `reason`), so it
    /// is clamped. Asserted in BOTH directions: a clamp test that only
    /// bounds the maximum passes when the clamp is so aggressive that
    /// nothing fits, which inverts its own purpose (the #572 lesson).
    #[test]
    fn an_oversized_concern_is_clamped_and_the_commands_survive() {
        let huge = "x".repeat(CONCERN_CAP * 4);
        let msg = render_ask(9, &huge, "tok", deadline());
        assert!(msg.len() < CONCERN_CAP * 2, "upper bound: not clamped at all? {}", msg.len());
        assert!(msg.len() > CONCERN_CAP, "lower bound: clamped so hard nothing fits? {}", msg.len());
        assert!(msg.contains("/approve tok"), "the commands must survive the clamp");
        assert!(msg.contains("/deny tok"));
    }

    /// A clamp that splits a multi-byte character panics on a String slice.
    /// The concern is free text and can be any language.
    ///
    /// Must use a character whose width does **not** evenly divide
    /// `CONCERN_CAP` (512), or the cap always lands exactly on a boundary
    /// and the boundary-walk's decrementing branch never runs. `é` is
    /// 2 bytes and 512 is even, so it doesn't exercise this at all; a 3-byte
    /// CJK character does, since 512 isn't a multiple of 3.
    #[test]
    fn clamping_a_multibyte_concern_does_not_panic() {
        let msg = render_ask(9, &"中".repeat(CONCERN_CAP), "tok", deadline());
        assert!(msg.contains("/approve tok"));
    }

    /// `Debug` must never render the live token — the same property
    /// `channel::tests::peer_evidence_debug_redacts_the_presented_token`
    /// asserts for `PeerEvidence::presented_token`, and for the same reason:
    /// nothing today debug-formats an `AskCommand`, but the whole point of a
    /// `Debug` impl is that someone eventually will.
    #[test]
    fn ask_command_debug_redacts_the_token() {
        let cmd = AskCommand { choice: AskChoice::Approve, token: "S3CRET-TOKEN-VALUE".to_string() };
        let rendered = format!("{cmd:?}");
        assert!(!rendered.contains("S3CRET-TOKEN-VALUE"), "token leaked into Debug: {rendered}");
        assert!(rendered.contains("redacted"), "must say it was redacted: {rendered}");
    }

    #[test]
    fn the_success_ack_names_the_task_and_the_decision() {
        assert!(ack_resolved(AskChoice::Approve, 412).contains("412"));
        assert!(ack_resolved(AskChoice::Approve, 412).to_lowercase().contains("approv"));
        assert!(ack_resolved(AskChoice::Deny, 412).to_lowercase().contains("den"));
    }

    /// D9: the failure ack must not distinguish wrong / expired / already
    /// answered / not-your-ask. `resolve_with_nonce` refuses to leak which,
    /// and re-leaking it in the presentation layer would hand back the
    /// existence oracle the query gives up.
    #[test]
    fn the_failure_ack_names_no_specific_cause() {
        let lowered = ACK_NOT_ANSWERABLE.to_lowercase();
        for leak in ["expired", "already", "wrong peer", "not found", "yours"] {
            assert!(!lowered.contains(leak), "the ack leaks a cause: {leak}");
        }
    }

    fn deadline() -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(1_787_000_000).unwrap()
    }

    /// The same instant as [`deadline`], plus nanoseconds — i.e. the shape a
    /// real `OffsetDateTime::now_utc() + Duration` actually has.
    ///
    /// A whole-second fixture is not a representative one here. It does not
    /// suppress the fraction — `Display` writes the `.` unconditionally, so
    /// a whole second still renders `…20.0 +00:00:00` — but a single `.0` is
    /// the kind of thing a human skims past, whereas nine digits is not.
    /// Test against the shape production actually produces.
    fn messy_deadline() -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp_nanos(1_787_000_000_482_913_571).unwrap()
    }
}
