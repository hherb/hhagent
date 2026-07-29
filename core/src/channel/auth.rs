//! Peer authorization: decides whether an inbound message comes from a peer the
//! operator has paired. **Fail-closed.** Authorization is keyed on
//! `(channel, peer)` and is `async` because the production authorizer
//! ([`DbPeerAuthorizer`]) is a DB fact — at single-user volume a query per
//! inbound message is trivial and lets operator revocation take effect
//! immediately with no cache. [`StaticPairings`] remains for tests/legacy.

use std::collections::HashSet;

use super::{ChannelId, PeerEvidence, PeerId};

/// Outcome of authorizing one inbound peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDecision {
    /// Peer is paired; the message may proceed to screening + enqueue.
    Recognised,
    /// Peer is unknown/unpaired; the bus drops it (after the pairing carve-out).
    Rejected,
    /// Peer is paired but the transport-supplied evidence (DMARC and/or the
    /// per-pairing token) didn't check out. Distinct from [`Rejected`] because
    /// the bus must skip the pairing carve-out entirely for this outcome — see
    /// `bus::handle_inbound`.
    ///
    /// [`Rejected`]: AuthDecision::Rejected
    RejectedUnauthentic,
}

/// The authorization seam. Async + `(channel, peer)`-scoped. Dyn-safe.
///
/// `evidence` is `None` for transports that authenticate their own peers
/// (Matrix); those implementations ignore it. `Some` transports (email) hand
/// over what they could verify and the authorizer decides.
#[async_trait::async_trait]
pub trait PeerAuthorizer: Send + Sync {
    async fn authorize(
        &self,
        channel: &ChannelId,
        peer: &PeerId,
        evidence: Option<&PeerEvidence>,
    ) -> AuthDecision;
}

/// A fixed set of recognised peers (peer-only match, channel-agnostic). **Empty
/// by default → deny all.** Useful for tests + a legacy operator-config path; the
/// production authorizer is [`DbPeerAuthorizer`].
#[derive(Default, Clone)]
pub struct StaticPairings {
    recognised: HashSet<PeerId>,
}

impl StaticPairings {
    /// Empty → denies everyone (fail-closed).
    pub fn new() -> Self {
        Self { recognised: HashSet::new() }
    }

    /// Build from an iterator of recognised peer ids.
    pub fn from_peers<I: IntoIterator<Item = PeerId>>(peers: I) -> Self {
        Self { recognised: peers.into_iter().collect() }
    }
}

#[async_trait::async_trait]
impl PeerAuthorizer for StaticPairings {
    async fn authorize(
        &self,
        _channel: &ChannelId,
        peer: &PeerId,
        _evidence: Option<&PeerEvidence>,
    ) -> AuthDecision {
        // StaticPairings is peer-only (test/legacy) — evidence is a DB concept
        // (DbPeerAuthorizer), so it's deliberately ignored here.
        if self.recognised.contains(peer) {
            AuthDecision::Recognised
        } else {
            AuthDecision::Rejected
        }
    }
}

/// Production authorizer: an active (non-revoked) row in the `pairings` table for
/// `(channel, peer)` means recognised. A DB error fails **closed** (`Rejected`,
/// logged) — an authorization lookup that can't be confirmed must not admit.
pub struct DbPeerAuthorizer {
    pool: sqlx::PgPool,
}

impl DbPeerAuthorizer {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl PeerAuthorizer for DbPeerAuthorizer {
    async fn authorize(
        &self,
        channel: &ChannelId,
        peer: &PeerId,
        evidence: Option<&PeerEvidence>,
    ) -> AuthDecision {
        match kastellan_db::pairings::token_hash_for(&self.pool, &channel.0, &peer.0).await {
            // No active pairing.
            Ok(None) => AuthDecision::Rejected,
            // Paired, no token required (Matrix) — evidence is not consulted.
            Ok(Some(None)) => AuthDecision::Recognised,
            // Paired WITH a token: the transport must supply evidence, DMARC
            // must pass, and the token must match.
            Ok(Some(Some(expected))) => {
                let Some(ev) = evidence else {
                    tracing::warn!(channel = %channel.0,
                        "pairing requires a token but the transport supplied no evidence");
                    return AuthDecision::RejectedUnauthentic;
                };
                if !ev.dmarc_pass {
                    return AuthDecision::RejectedUnauthentic;
                }
                let presented = match ev.presented_token.as_deref() {
                    Some(t) => crate::channel::ingest::sha256_hex(t.as_bytes()),
                    None => return AuthDecision::RejectedUnauthentic,
                };
                if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
                    AuthDecision::Recognised
                } else {
                    AuthDecision::RejectedUnauthentic
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, channel = %channel.0,
                    "pairing lookup failed; failing closed");
                AuthDecision::Rejected
            }
        }
    }
}

/// Length-independent byte comparison, so a token check cannot be narrowed by
/// timing. Both inputs here are fixed-length hex digests, but comparing them
/// with `==` would still short-circuit on the first differing byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch() -> ChannelId {
        ChannelId("matrix".into())
    }

    #[tokio::test]
    async fn empty_pairings_deny_everyone() {
        let a = StaticPairings::new();
        assert_eq!(a.authorize(&ch(), &PeerId("@anyone:srv".into()), None).await, AuthDecision::Rejected);
    }

    #[tokio::test]
    async fn recognised_peer_is_allowed_others_denied() {
        let a = StaticPairings::from_peers([PeerId("@me:srv".into())]);
        assert_eq!(a.authorize(&ch(), &PeerId("@me:srv".into()), None).await, AuthDecision::Recognised);
        assert_eq!(a.authorize(&ch(), &PeerId("@me:other".into()), None).await, AuthDecision::Rejected);
    }

    #[tokio::test]
    async fn peer_id_match_is_exact_not_substring() {
        let a = StaticPairings::from_peers([PeerId("@me:srv".into())]);
        assert_eq!(a.authorize(&ch(), &PeerId("@me:srv.evil".into()), None).await, AuthDecision::Rejected);
        assert_eq!(a.authorize(&ch(), &PeerId("evil@me:srv".into()), None).await, AuthDecision::Rejected);
    }

    #[tokio::test]
    async fn static_pairings_ignore_evidence() {
        // StaticPairings is the test/legacy authorizer; evidence is a DB concept.
        let a = StaticPairings::from_peers([PeerId("@me:srv".into())]);
        let ev = PeerEvidence { dmarc_pass: false, presented_token: None };
        assert_eq!(a.authorize(&ch(), &PeerId("@me:srv".into()), Some(&ev)).await,
                   AuthDecision::Recognised);
    }
}
