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
pub const ACK_MALFORMED_COMMAND: &str =
    "Usage: /approve <token> or /deny <token> \u{2014} exactly the verb and the token, \
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
/// the record), and coupling them would make the channel module depend on
/// the scheduler for a two-variant enum. `the_wire_verbs_are_the_stored_choices`
/// is the anti-drift guard — [`Self::as_str`] must keep producing exactly the
/// strings that `raise_and_suspend` writes into `asks.options`, because
/// `db::asks::resolve_with_nonce` validates the choice against them.
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
/// else — in which case the body is an ordinary message and takes the
/// normal screen-and-enqueue path.
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

/// True when `body`'s first whitespace token is `/approve` or `/deny`
/// (case-insensitive), whether or not the whole body goes on to parse as a
/// command.
///
/// The bus uses this to tell "an attempted answer that is malformed" apart
/// from "an ordinary message" when [`parse_ask_command`] returns `None`.
/// Those two must not be treated the same: an ordinary message may safely
/// fall through to screening and enqueue, but a malformed *attempt* may
/// still carry a live approval token later in the body (`/approve tok9
/// thanks!`), and enqueueing it would write that token verbatim into
/// `tasks.payload` and hand it to the planner. Only the leading verb is
/// checked — the same reason [`parse_ask_command`] itself does no shape
/// check on the token (spec D7) applies here: this function's job is to
/// recognise an *attempt*, not to validate one.
pub fn looks_like_ask_command(body: &str) -> bool {
    matches!(
        body.split_whitespace().next().map(str::to_ascii_lowercase).as_deref(),
        Some("/approve") | Some("/deny")
    )
}

/// The message an escalated plan sends into its task's conversation.
///
/// The two command lines are printed complete so answering is a copy, not a
/// transcription. **The ask id is deliberately absent**: it is not needed to
/// answer, and putting a small sequential integer in durable room history
/// invites exactly the resolve-by-id thinking `db::asks::resolve`'s doc
/// reserves for the local CLI.
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
        concern = clamp(concern, CONCERN_CAP),
        deadline = deadline_at,
    )
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

    /// Bodies that are not an attempt at all — no leading verb, or a
    /// different leading word — must not be flagged, or an ordinary
    /// message that happens to mention approval would be blocked from
    /// ever reaching the planner.
    #[test]
    fn an_ordinary_message_does_not_look_like_a_command() {
        for body in [
            "approve 7f3a9c2e1b",
            "please /approve 7f3a9c2e1b",
            "what is my flight's GST?",
            "",
            "/approver 7f3a9c2e1b",
        ] {
            assert!(!looks_like_ask_command(body), "should not look like an attempt: {body:?}");
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
    #[test]
    fn every_command_the_message_prints_parses_back() {
        let msg = render_ask(1, "c", "tok123", deadline());
        let commands: Vec<&str> =
            msg.lines().map(str::trim).filter(|l| l.starts_with('/')).collect();
        assert_eq!(commands.len(), 2, "exactly two commands offered: {msg}");
        for line in commands {
            let cmd = parse_ask_command(line)
                .unwrap_or_else(|| panic!("rendered command does not parse: {line:?}"));
            assert_eq!(cmd.token, "tok123");
        }
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
}
