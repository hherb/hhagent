//! Pure [`SandboxPolicy`] builder for the email-in worker, mirroring
//! `matrix/policy.rs`'s `build_matrix_policy` in shape (`Net::Allowlist`
//! scoped to the one endpoint, `Profile::WorkerNetClient`, the resolver files
//! RO-shared for in-jail DNS) but simpler in two ways that both follow from
//! the design (`docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`):
//!
//! * `fs_write` is always empty — localmail owns the polling cursor
//!   server-side (design decision D7), so this worker persists nothing
//!   across restarts, unlike Matrix's E2E crypto/session store.
//! * No system CA trust store is bound in: email-in reuses
//!   `workers/web-common`'s `make_get` transport, the same one
//!   `workers/mail` already uses against the identical localmail `/v1` API
//!   (`core/src/workers/mail.rs::mail_entry`'s policy has the identical
//!   `fs_read` set for exactly this reason), unlike matrix-rust-sdk which
//!   validates the homeserver's TLS against the native system trust store.

use std::path::PathBuf;

use kastellan_sandbox::{Net, Profile, SandboxPolicy};

/// Build the [`SandboxPolicy`] for the long-lived email-in worker.
///
/// - `Net::Allowlist([endpoint_host:endpoint_port])` — the worker reaches
///   only localmail (via the egress proxy when `proxy_uds` is set at spawn).
/// - `Profile::WorkerNetClient` — the plain outbound-HTTPS syscall set, same
///   as `workers/mail`'s policy.
/// - `fs_read`: the worker binary, the bearer-token file, and the resolver
///   config files (in-jail DNS) — exactly `mail_entry`'s set.
/// - `fs_write`: always empty (see the module docs).
/// - `proxy_uds`: always `None` here; force-routing (if the daemon opts in)
///   sets it at spawn, mirroring every other net worker.
///
/// [`SandboxPolicy`]: kastellan_sandbox::SandboxPolicy
pub fn build_email_policy(
    worker_bin: PathBuf,
    endpoint_host: &str,
    endpoint_port: u16,
    token_file: PathBuf,
) -> SandboxPolicy {
    SandboxPolicy {
        fs_read: vec![
            worker_bin,
            token_file,
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        net: Net::Allowlist(vec![format!("{endpoint_host}:{endpoint_port}")]),
        cpu_ms: 0, // long-lived; no per-process CPU cap (bounded by cgroup/quota)
        mem_mb: 256,
        profile: Profile::WorkerNetClient,
        cpu_quota_pct: None,
        tasks_max: None,
        env: Vec::new(), // spawn fills env (endpoint/subscription/address/token-file path)
        proxy_uds: None,
        broker_uds: None,
        persistent_store: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_shape() {
        let p = build_email_policy(
            PathBuf::from("/opt/kastellan/kastellan-worker-email-in"),
            "127.0.0.1",
            8443,
            PathBuf::from("/var/lib/kastellan/email/token"),
        );
        assert!(matches!(p.net, Net::Allowlist(ref v) if v == &["127.0.0.1:8443"]));
        assert!(matches!(p.profile, Profile::WorkerNetClient));
        assert!(p.fs_write.is_empty(), "localmail owns the cursor; nothing persisted locally");
        assert!(p.fs_read.contains(&PathBuf::from("/var/lib/kastellan/email/token")));
        assert!(p.fs_read.contains(&PathBuf::from("/opt/kastellan/kastellan-worker-email-in")));
        assert!(p.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
        assert!(p.proxy_uds.is_none());
        assert_eq!(p.cpu_ms, 0, "long-lived worker: no per-process CPU rlimit");
    }

    #[test]
    fn policy_scopes_the_allowlist_to_exactly_one_host_port() {
        let p = build_email_policy(
            PathBuf::from("/bin/kastellan-worker-email-in"),
            "mail.example.org",
            443,
            PathBuf::from("/etc/kastellan/email.token"),
        );
        assert!(matches!(p.net, Net::Allowlist(ref v) if v.len() == 1 && v[0] == "mail.example.org:443"));
    }
}
