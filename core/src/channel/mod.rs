//! The channel bus: the transport-agnostic boundary between an external
//! messaging channel (Matrix primary, email fallback — see
//! `docs/superpowers/specs/2026-06-12-primary-communication-channel-design.md`)
//! and the core conversation queue (the Postgres `tasks` table).
//!
//! Security model (three separable layers — see the spec + `docs/threat-model.md`
//! "Communication channel"):
//!   1. **Peer authentication** ([`auth`]) — fail-closed: an unrecognised peer's
//!      message never becomes a task (dropped + audited). Pairing (TOTP/WebAuthn)
//!      that makes a peer *recognised* is comms slice #3; this slice ships the seam.
//!   2. **Untrusted-input screening** ([`ingest`]) — every inbound body runs
//!      through `cassandra::injection_guard` exactly like worker output. A channel
//!      peer is no more trusted than a fetched web page.
//!   3. **Audit** — every received / rejected / enqueued / replied message lands
//!      in `audit_log`.
//!
//! All security decisions are **pure** (`auth`/`ingest`/`route`); [`bus`] is a thin
//! async pump over the [`Channel`] transport seam + the DB seams, so the whole
//! inbound→enqueue→complete→reply loop is testable with fakes (no network, no PG).

pub mod ask_message;
pub mod audit_text;
pub mod auth;
pub mod boot_supervisor;
pub mod bus;
pub mod email;
pub mod ingest;
pub mod matrix;
pub mod pairing;
pub mod polled_driver;
pub mod pump_liveness;
pub mod respawn_alarm;
pub mod route;

pub use bus::ChannelBus;

use serde::{Deserialize, Serialize};

/// Stable identifier of a configured channel (e.g. `"matrix"`, `"email"`). The
/// outbound router uses it to find the `Channel` to reply through.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelId(pub String);

/// Channel-native identity of the *sender* (e.g. a Matrix `@user:server`, an
/// email `From`). Opaque to the bus; the [`auth::PeerAuthorizer`] interprets it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId(pub String);

/// Channel-native conversation/thread the message belongs to (a Matrix room id,
/// an email thread). Carried through so the reply lands in the same place.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationId(pub String);

/// A normalized inbound message handed up by a [`Channel`] transport. The
/// transport is responsible for decrypting (E2E) and flattening to this shape;
/// the bus never sees ciphertext or protocol frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncomingMessage {
    pub channel: ChannelId,
    pub peer: PeerId,
    pub conversation: ConversationId,
    /// The plaintext user message body. Treated as fully untrusted input.
    pub body: String,
    /// Transport-supplied authenticity evidence, or `None` when the transport
    /// already authenticates its own peers (Matrix). See [`PeerEvidence`].
    pub evidence: Option<PeerEvidence>,
}

/// Transport-supplied evidence that an inbound message really came from the
/// claimed peer.
///
/// `IncomingMessage.evidence` is `None` when the transport authenticates its
/// own peers (Matrix: E2E + homeserver auth) — the bus then applies no extra
/// check, which is what keeps Matrix behaviour byte-identical. `Some` means the
/// transport cannot vouch for the sender and the bus must decide.
///
/// [`Debug`] is hand-written to **redact `presented_token`**: it is the
/// plaintext per-pairing shared secret, and this struct is reachable from
/// `IncomingMessage`/`PolledEvent`, which both derive `Debug`. Nothing
/// debug-formats them today, but the whole point of a `Debug` impl is that
/// someone eventually will — a `?msg` in a `tracing` call anywhere on the
/// inbound path would otherwise write the secret to the daemon log, which is
/// exactly the leak `gate::extract_token` works to prevent one layer up.
/// Derive-by-default is the wrong side of that trade for a secret-bearing type.
#[derive(Clone, PartialEq, Eq)]
pub struct PeerEvidence {
    /// Our own MX reported `dmarc=pass` (see `email::gate::trusted_dmarc_pass`).
    pub dmarc_pass: bool,
    /// The per-pairing token the sender presented, already stripped from the body.
    pub presented_token: Option<String>,
}

impl std::fmt::Debug for PeerEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Presence is diagnostically useful ("did they send one at all?"); the
        // value never is, and must never reach a log.
        let token = if self.presented_token.is_some() { "Some(<redacted>)" } else { "None" };
        f.debug_struct("PeerEvidence")
            .field("dmarc_pass", &self.dmarc_pass)
            .field("presented_token", &token)
            .finish()
    }
}

/// A reply the bus asks a [`Channel`] to deliver back to the originating peer +
/// conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingMessage {
    pub channel: ChannelId,
    pub peer: PeerId,
    pub conversation: ConversationId,
    pub body: String,
}

/// The transport seam. One implementation per channel (slice #2: `MatrixChannel`;
/// slice #5: `EmailChannel`). Dyn-safe (no generic methods) so the bus drives a
/// `Vec<Box<dyn Channel>>`. Network I/O + E2E live behind this; the bus is pure
/// orchestration above it.
#[async_trait::async_trait]
pub trait Channel: Send + Sync {
    /// This channel's stable id (matched against `OutgoingMessage.channel`).
    fn id(&self) -> ChannelId;

    /// Block for the next inbound message. `None` means the channel closed (the
    /// bus then drops this channel's inbound pump). Cancellation-safe: the bus
    /// `select!`s this against shutdown.
    async fn recv(&mut self) -> Option<IncomingMessage>;

    /// Deliver a reply. Errors are logged + audited by the bus, never panic.
    async fn send(&self, msg: OutgoingMessage) -> anyhow::Result<()>;
}

/// Canonical audit action strings for the channel bus. Centralised so the
/// negative-test e2e and the mirror consumers key off one source of truth.
pub mod actions {
    /// A message arrived from a recognised peer and was screened.
    pub const RECEIVED: &str = "channel.received";
    /// A message from an unrecognised/unpaired peer was dropped (fail-closed).
    pub const REJECTED_UNPAIRED: &str = "channel.rejected_unpaired";
    /// An unpaired peer presented a valid pairing code and was bound (slice #3).
    pub const PAIRED: &str = "channel.paired";
    /// A recognised peer's message was blocked by the injection guard.
    pub const INJECTION_BLOCKED: &str = "channel.injection_blocked";
    /// A reply was routed to its channel for delivery.
    ///
    /// Precisely: `handle_completed` resolved a completed task to an
    /// [`super::OutgoingMessage`] and handed it to the owning channel's
    /// outbound queue. It is **not** proof the transport delivered it — the
    /// `send` happens afterwards, in the per-channel pump, and can still fail
    /// (in slice 1 `EmailChannel::send` fails *unconditionally*, since there is
    /// no outbound worker yet). A failure emits [`REPLY_UNDELIVERED`] for the
    /// same reply, so the pair is what an operator queries: a `channel.replied`
    /// with no matching `channel.reply_undelivered` is a delivered reply. The
    /// name is kept because these strings are a committed operator-facing
    /// interface (see `auth::UnauthenticReason::as_str`); the doc is what was
    /// wrong, and it claimed delivery.
    pub const REPLIED: &str = "channel.replied";
    /// A reply was routed to its channel but the transport refused to deliver
    /// it — the compensating row for a [`REPLIED`] that did not land. Carries
    /// the channel + peer only, never the reply body and never the error
    /// string (which is transport text, not a fixed label).
    pub const REPLY_UNDELIVERED: &str = "channel.reply_undelivered";
    /// A message failed transport authenticity (DMARC and/or token) — dropped
    /// before authorization, so it never reaches the pairing carve-out.
    pub const REJECTED_UNAUTHENTIC: &str = "channel.rejected_unauthentic";
    /// A raw id was acked without ever becoming a [`super::IncomingMessage`]
    /// at all — e.g. email's `skipped` list (unattributable `From`, an
    /// unfetchable detail fetch). These are messages the agent silently
    /// never saw, so they must stay traceable even though the driver that
    /// acks them (`polled_driver::run`) is DB-free by design. See
    /// [`super::polled_driver::AckOnlyAudit`].
    pub const SKIPPED_ACK_ONLY: &str = "channel.skipped_ack_only";
    /// A channel bus came up. Payload carries the channel and how many
    /// bring-up attempts it took, so "did it have to retry?" is answerable
    /// after the fact — `attempts: 1` is the healthy shape (#514).
    pub const BOOT_STARTED: &str = "channel.started";
    /// A channel bring-up attempt failed. Payload carries the channel, the
    /// attempt number, the delay before the next attempt (absent when the
    /// failure is fatal and there will be none), and the capped cause.
    ///
    /// This is the durable record of a deaf window. Before #514 the only
    /// trace was one `channel not started` line in a daemon log that rotates,
    /// and the failure was not noticed until someone messaged the bot and got
    /// silence — 12 hours later, in the case that prompted this.
    pub const BOOT_FAILED: &str = "channel.boot_failed";
    /// A channel that had come up stopped working on its own and is being
    /// restarted (#517). Payload carries the channel, how long it ran
    /// (`ran_ms`) and the delay before the restart attempt.
    ///
    /// Separate from [`BOOT_FAILED`] on purpose: that row means the channel
    /// never came up, this one means it did and then went deaf — a pump ended,
    /// which until #517 was permanent and produced no row at all. `ran_ms` is
    /// what tells a sustained outage apart from a flapping channel.
    pub const CHANNEL_DIED: &str = "channel.died";
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `PeerEvidence`'s `Debug` must never render the token. Asserted here
    /// rather than trusted, because the failure mode is silent: a `?evidence`
    /// or `?msg` added to any `tracing` call on the inbound path would start
    /// writing the per-pairing secret into the daemon log, and nothing else
    /// would complain.
    #[test]
    fn peer_evidence_debug_redacts_the_presented_token() {
        let ev = PeerEvidence {
            dmarc_pass: true,
            presented_token: Some("S3CRET-TOKEN-VALUE".to_string()),
        };
        let rendered = format!("{ev:?}");
        assert!(!rendered.contains("S3CRET-TOKEN-VALUE"), "token leaked into Debug: {rendered}");
        assert!(rendered.contains("redacted"), "must say it was redacted: {rendered}");
        // The DMARC verdict is not a secret and stays legible for diagnosis.
        assert!(rendered.contains("true"), "dmarc_pass must stay visible: {rendered}");
    }

    /// An absent token must still be distinguishable from a redacted one, or
    /// the redaction destroys the only diagnostically useful bit.
    #[test]
    fn peer_evidence_debug_distinguishes_absent_from_redacted() {
        let none = format!("{:?}", PeerEvidence { dmarc_pass: false, presented_token: None });
        let some = format!(
            "{:?}",
            PeerEvidence { dmarc_pass: false, presented_token: Some("x".into()) }
        );
        assert_ne!(none, some);
        assert!(none.contains("None"), "{none}");
    }

    /// Redacting `Debug` must not have cost the derived traits the rest of the
    /// code relies on (`PolledEvent`/`IncomingMessage` compare evidence in
    /// tests; the driver clones it).
    #[test]
    fn peer_evidence_still_clones_and_compares() {
        let a = PeerEvidence { dmarc_pass: true, presented_token: Some("t".into()) };
        assert_eq!(a.clone(), a);
        assert_ne!(a, PeerEvidence { dmarc_pass: false, presented_token: Some("t".into()) });
    }
}
