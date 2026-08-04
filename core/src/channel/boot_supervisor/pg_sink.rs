//! The production [`BootAuditSink`]: one `audit_log` row per bring-up event.
//!
//! Split out of the supervisor so the retry loop itself stays database-free,
//! and therefore unit-testable without a cluster.
//!
//! These rows are the durable half of #514's answer to "was the bot deaf, and
//! for how long?". The daemon log has the same information, but it is a
//! plaintext file that rotates and that nobody reads until they notice
//! silence; `audit_log` is queryable after the fact and is where the rest of
//! the channel's history already lives.
//!
//! A failure to *write* the row is logged and swallowed. An unavailable
//! Postgres must not stop the supervisor from retrying the channel — that
//! would trade the bug for a worse one.

use futures::future::BoxFuture;
use sqlx::PgPool;
use tracing::warn;

use super::{BootAudit, BootAuditSink};
use crate::channel::actions;

/// Build the sink for one channel. `channel` is captured, so every row carries
/// it without the supervisor having to thread it through.
///
/// Payloads carry the channel name, counters and the (already capped) cause —
/// never message content, and never anything a peer supplied.
pub fn pg_boot_audit_sink(pool: PgPool, channel: &str) -> BootAuditSink {
    let channel = channel.to_string();
    Box::new(move |event: BootAudit| {
        let pool = pool.clone();
        let channel = channel.clone();
        Box::pin(async move {
            let (action, payload) = match event {
                BootAudit::Started { attempts } => (
                    actions::BOOT_STARTED,
                    serde_json::json!({ "channel": channel, "attempts": attempts }),
                ),
                BootAudit::Failed { attempt, retry_in_ms, fatal, cause } => (
                    actions::BOOT_FAILED,
                    serde_json::json!({
                        "channel": channel,
                        "attempt": attempt,
                        "retry_in_ms": retry_in_ms,
                        "fatal": fatal,
                        "cause": cause,
                    }),
                ),
            };
            if let Err(e) = kastellan_db::audit::insert(&pool, "channel", action, payload).await {
                warn!(error = %e, action, "channel bring-up audit insert failed (non-fatal)");
            }
        }) as BoxFuture<'static, ()>
    })
}
