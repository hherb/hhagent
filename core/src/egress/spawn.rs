//! Spawn the sandboxed egress-proxy sidecar on a per-worker UDS and wait for it
//! to be ready. Reusable host-side API; slice #2 calls this from the net-worker
//! bring-up path and ties `SidecarHandle::shutdown` to worker-terminal teardown.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use kastellan_sandbox::{Net, Profile, SandboxBackend, SandboxPolicy};

/// Env keys the sidecar binary reads (must match `egress-proxy::main`).
const ENV_UDS: &str = "KASTELLAN_EGRESS_PROXY_UDS";
const ENV_ALLOWLIST: &str = "KASTELLAN_EGRESS_PROXY_ALLOWLIST";
const ENV_WORKER: &str = "KASTELLAN_EGRESS_PROXY_WORKER";
const ENV_PINS: &str = "KASTELLAN_EGRESS_PROXY_PINS";
/// Env key that puts the sidecar into no-MITM (transparent-tunnel) mode for
/// workers that do their own end-to-end TLS (the browser). Must match the read
/// in `egress-proxy::main`.
const ENV_DISABLE_MITM: &str = "KASTELLAN_EGRESS_PROXY_DISABLE_MITM";
/// Env key pointing the sidecar at an operator-provided extra CA to trust on the
/// re-origination (upstream) leg — for a self-signed private origin (localmail,
/// #491). Must match the read in `egress-proxy::main`.
const ENV_UPSTREAM_EXTRA_CA: &str = "KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA";

/// Basename of the per-worker sidecar UDS under the scratch dir. Shared so the
/// force-routing scratch-dir guard (`net_worker::make_worker_scratch_dir`) can
/// project the exact socket path the sidecar will `bind()`.
pub(crate) const UDS_FILE_NAME: &str = "egress.sock";

/// Basename of the per-worker CA cert the sidecar exports for the host to inject
/// into the worker's trust store (slice #3a). Lives beside the UDS in scratch.
pub(crate) const CA_FILE_NAME: &str = "ca.pem";

/// How long `spawn_sidecar` waits for the proxy to `bind()` its UDS.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const READY_POLL: Duration = Duration::from_millis(25);

/// How long a bring-up failure waits for the stderr drain thread to flush before
/// reporting. `try_wait` can observe the exit before the drain thread has read
/// the bytes the proxy wrote on its way out, so snapshotting the tail
/// immediately would drop the very message we want (the fail-closed reason).
/// Only ever paid on a path that is already failing.
const STDERR_SETTLE: Duration = Duration::from_millis(250);

/// Cumulative CPU budget (ms → ceil-div RLIMIT_CPU seconds) for a **short-lived**
/// per-tool-call sidecar. Matches the web-fetch worker's own `cpu_ms` (the
/// sidecar lives 1:1 with that single dispatch), and restores the CPU
/// defense-in-depth that `e70174b` had to drop blanket-wide (issue #395). A
/// long-lived channel sidecar (matrix) gets `0` instead — see [`proxy_policy`].
const SHORT_LIVED_SIDECAR_CPU_MS: u64 = 10_000;

/// A running sidecar. Drop or `shutdown()` kills it.
#[derive(Debug)]
pub struct SidecarHandle {
    child: Child,
    pub uds_path: PathBuf,
}

impl SidecarHandle {
    /// Kill the sidecar and reap it. Idempotent-ish (errors ignored).
    pub fn shutdown(mut self) {
        self.terminate();
    }

    /// Kill + reap the sidecar and remove its UDS, in place. Idempotent-ish
    /// (errors ignored). Shared by [`shutdown`](Self::shutdown) and by the
    /// coupled-teardown `Drop` of `egress::net_worker::EgressSidecar`, which
    /// holds the handle by value and cannot consume `self`.
    pub fn terminate(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.uds_path);
    }

    /// Borrow the child's stdout for the caller's decision-ingest loop.
    pub fn stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.stdout.take()
    }
}

/// Build the sandbox policy for the proxy: `Net::ProxyEgress` (real outbound +
/// DNS, self-enforcing), `WorkerNetClient` (permits `socket(2)`), fs_read for
/// the DNS resolver files + the binary, fs_write for the scratch dir (to create
/// the UDS), and the env contract.
///
/// `long_lived` selects the CPU governance (issue #395). A channel sidecar
/// (matrix) lives 1:1 with a worker that runs for weeks, so a cumulative
/// `RLIMIT_CPU` would eventually SIGKILL it mid-flight → `cpu_ms: 0` (no cap;
/// bounded instead by the cgroup `CPUQuota` on Linux / the mem cap). A
/// short-lived per-tool-call sidecar (web-fetch) lives only for the one
/// dispatch, so it gets a bounded [`SHORT_LIVED_SIDECAR_CPU_MS`] cap back —
/// restoring the defense-in-depth that only mattered on macOS, where
/// `RLIMIT_CPU` is the sole per-process CPU-governance primitive.
///
/// This function is a pure descriptor builder and validates nothing: the
/// preconditions on `upstream_extra_ca` (absolute, and not paired with
/// `disable_mitm`) are enforced by [`check_upstream_extra_ca`], which
/// [`spawn_sidecar`] calls before it builds the policy. Callers reaching for
/// `proxy_policy` directly must uphold them themselves.
#[allow(clippy::too_many_arguments)] // descriptor args for the sidecar's SandboxPolicy
pub fn proxy_policy(
    binary: &Path,
    allowlist: &[String],
    scratch: &Path,
    worker: &str,
    cert_pins_json: Option<&str>,
    disable_mitm: bool,
    long_lived: bool,
    upstream_extra_ca: Option<&Path>,
) -> SandboxPolicy {
    let uds = scratch.join(UDS_FILE_NAME);
    let allow_json = serde_json::to_string(allowlist).expect("Vec<String> serializes");
    let mut env = vec![
        (ENV_UDS.to_string(), uds.to_string_lossy().into_owned()),
        (ENV_ALLOWLIST.to_string(), allow_json),
        (ENV_WORKER.to_string(), worker.to_string()),
    ];
    // Pins are static operator config (slice #4). Omit the key entirely when
    // absent so the no-pin path is byte-identical to slice #3b.
    if let Some(pins) = cert_pins_json.filter(|s| !s.trim().is_empty()) {
        env.push((ENV_PINS.to_string(), pins.to_string()));
    }
    // Omit the disable-MITM key entirely when false so the no-flag path is
    // byte-identical to the default MITM path (mirrors the pins pattern).
    if disable_mitm {
        env.push((ENV_DISABLE_MITM.to_string(), "1".to_string()));
    }
    // Operator-provided extra CA for the re-origination leg (#491). Omit the key
    // entirely when absent so the no-extra-CA path is byte-identical. The env
    // value is `to_string_lossy` (as ENV_UDS above is) while the fs_read bind
    // below keeps the exact bytes, so a non-UTF-8 path would disagree — the
    // proxy then can't open the mangled path and startup fails closed.
    if let Some(ca) = upstream_extra_ca {
        env.push((ENV_UPSTREAM_EXTRA_CA.to_string(), ca.to_string_lossy().into_owned()));
    }
    let mut fs_read = vec![
        binary.to_path_buf(),
        PathBuf::from("/etc/resolv.conf"),
        PathBuf::from("/etc/hosts"),
        PathBuf::from("/etc/nsswitch.conf"),
    ];
    // The proxy reads the extra CA at startup (before lock_down); it must be
    // bound into the jail's fs_read to be openable.
    if let Some(ca) = upstream_extra_ca {
        fs_read.push(ca.to_path_buf());
    }
    SandboxPolicy {
        fs_read,
        fs_write: vec![scratch.to_path_buf()],
        net: Net::ProxyEgress,
        // CPU governance is lifetime-scoped (issue #395). A long-lived channel
        // sidecar (matrix, weeks) gets no cumulative RLIMIT_CPU — same
        // convention as `build_matrix_policy` — because the historical `10_000`
        // WOULD have SIGKILLed it mid-flight once the spawn fix below made the
        // lockdown env actually reach the proxy (it never did before `e70174b`).
        // A short-lived per-tool-call sidecar lives only for its one dispatch,
        // so it keeps the bounded cap as defense-in-depth (the only CPU primitive
        // on macOS, where there is no cgroup quota).
        cpu_ms: if long_lived { 0 } else { SHORT_LIVED_SIDECAR_CPU_MS },
        mem_mb: 256,
        profile: Profile::WorkerNetClient,
        cpu_quota_pct: None,
        tasks_max: None,
        env,
        proxy_uds: None,
        broker_uds: None,
        persistent_store: None,
    }
}

/// Reject an `upstream_extra_ca` that cannot do what the caller intends, before
/// anything is spawned:
///
/// * A **relative** path. The CA is bound into the proxy jail via
///   `SandboxPolicy.fs_read`, and both backends reject relative `fs_read`
///   entries — so the failure would name the sandbox rather than the
///   misconfigured field. (A *nonexistent* absolute path is deliberately NOT
///   rejected here: `canonicalize_one` tolerates `NotFound` and the Linux bind is
///   `--ro-bind-try`, leaving the proxy — the authority on the PEM's content —
///   to fail closed on it at startup.)
/// * A path paired with `disable_mitm`. A transparent tunnel never re-originates
///   TLS, so it never consults the upstream root store. Accepting the pair would
///   leave an operator believing a private self-signed origin is reachable when
///   in fact the sidecar validates no upstream certificate at all — exactly the
///   false "the force-routed path reaches it" belief #491 was opened to correct.
///   Fail loud instead.
///
/// Split out as a pure function so both preconditions are unit-testable without
/// a sandbox or a built proxy binary.
fn check_upstream_extra_ca(
    disable_mitm: bool,
    upstream_extra_ca: Option<&Path>,
) -> anyhow::Result<()> {
    let Some(ca) = upstream_extra_ca else {
        return Ok(());
    };
    if !ca.is_absolute() {
        anyhow::bail!(
            "upstream extra CA path must be absolute (it is bound into the proxy jail via \
             fs_read, which rejects relative paths): {ca:?}"
        );
    }
    if disable_mitm {
        anyhow::bail!(
            "upstream extra CA {ca:?} was given to a transparent-tunnel (disable_mitm) sidecar, \
             which never re-originates TLS and so would never use it"
        );
    }
    Ok(())
}

/// One-line stderr summary for a sidecar bring-up failure, waiting up to
/// [`STDERR_SETTLE`] for the drain thread to flush.
///
/// The proxy's fail-closed startup aborts (a malformed pin set, an unreadable
/// upstream extra CA) are reported only as `Err` out of `main`, i.e. on its
/// stderr — a pipe the host drains to `debug` and nothing else reads. Without
/// folding it into the error, a mistyped operator CA path surfaces as a bare
/// readiness timeout that blames the UDS bind.
fn stderr_note(tail: Option<&crate::worker_stderr::StderrTail>) -> String {
    let Some(tail) = tail else {
        return "no stderr captured".to_string();
    };
    let deadline = Instant::now() + STDERR_SETTLE;
    let mut lines = tail.snapshot();
    while lines.is_empty() && Instant::now() < deadline {
        std::thread::sleep(READY_POLL);
        lines = tail.snapshot();
    }
    if lines.is_empty() {
        "no stderr captured".to_string()
    } else {
        format!("recent stderr: {}", lines.join(" | "))
    }
}

/// Spawn the proxy under `backend` and wait (bounded) for its UDS to appear.
/// Fail-closed: returns `Err` on a failed precondition
/// ([`check_upstream_extra_ca`]), on spawn failure, on the proxy exiting before
/// it is ready, or on the readiness timeout.
///
/// `long_lived` scopes the sidecar's CPU cap — see [`proxy_policy`]. Pass `true`
/// for a channel sidecar that outlives many dispatches (matrix), `false` for a
/// per-tool-call sidecar (web-fetch) so it gets a bounded `RLIMIT_CPU` back.
#[allow(clippy::too_many_arguments)] // mirrors `proxy_policy`'s descriptor args + `backend`
pub fn spawn_sidecar(
    backend: &dyn SandboxBackend,
    binary: &Path,
    allowlist: &[String],
    scratch: &Path,
    worker: &str,
    cert_pins_json: Option<&str>,
    disable_mitm: bool,
    long_lived: bool,
    upstream_extra_ca: Option<&Path>,
) -> anyhow::Result<SidecarHandle> {
    check_upstream_extra_ca(disable_mitm, upstream_extra_ca)?;
    if let Some(ca) = upstream_extra_ca {
        // Operator-visible record that this sidecar's upstream trust is wider
        // than webpki. The proxy logs its own WARN, but only to its stderr —
        // drained to `debug` below — so without this the daemon log would never
        // carry the fact at a level an operator reads.
        tracing::warn!(
            worker,
            extra_ca = %ca.display(),
            "egress sidecar trusts an operator-provided upstream extra CA on its re-origination \
             leg (widens trust beyond webpki for EVERY host this sidecar may reach)"
        );
    }
    let policy = proxy_policy(
        binary,
        allowlist,
        scratch,
        worker,
        cert_pins_json,
        disable_mitm,
        long_lived,
        upstream_extra_ca,
    );
    let uds_path = scratch.join(UDS_FILE_NAME);
    let _ = std::fs::remove_file(&uds_path);

    // Derive the worker-side lockdown env (KASTELLAN_SECCOMP_PROFILE +
    // KASTELLAN_LANDLOCK_RW/RO) exactly like every other spawn path. Without
    // it the proxy's in-process lock_down ran with NO seccomp and — worse — a
    // Landlock ruleset missing the fs_read grants, so post-lockdown glibc
    // could not open /etc/resolv.conf|hosts|nsswitch.conf and EVERY
    // DNS-needing CONNECT failed EAI_AGAIN ("Temporary failure in name
    // resolution") on Linux. Literal-IP tunnels never resolve, which is why
    // the hermetic suites stayed green while real-hostname egress was broken.
    let derived = crate::tool_host::derive_lockdown_env(&policy);
    let program = binary.to_string_lossy();
    let mut child = backend
        .spawn_under_policy(&derived, &program, &[])
        .map_err(|e| anyhow::anyhow!("spawn egress-proxy sidecar: {e}"))?;

    // Drain the sidecar's piped stderr on a detached thread — same reason
    // `tool_host::spawn_worker` drains a worker's: the backends pipe stderr but
    // nothing here reads it, so a proxy writing past the ~64 KiB pipe buffer
    // would block on write. The tail-retaining variant additionally lets a
    // bring-up failure below report the proxy's OWN account of what went wrong.
    let pid = child.id();
    let stderr_tail = child
        .stderr
        .take()
        .map(|stderr| crate::worker_stderr::spawn_drain_with_tail(pid, stderr));

    // Slice #3a: the sidecar also exports its per-instance MITM CA next to the
    // UDS. Wait for BOTH so the host never binds a worker before the CA it must
    // trust exists on disk.
    let ca_path = scratch.join(CA_FILE_NAME);
    let deadline = Instant::now() + READY_TIMEOUT;
    while !(uds_path.exists() && ca_path.exists()) {
        // The proxy fails CLOSED on bad operator config (a malformed pin set, an
        // unreadable/certless upstream extra CA) by aborting at startup. Catch
        // that exit as soon as it happens: otherwise it only ever surfaces as the
        // READY_TIMEOUT below — a five-second wait whose message blames the UDS
        // bind rather than the config that actually failed.
        if let Ok(Some(status)) = child.try_wait() {
            anyhow::bail!(
                "egress-proxy sidecar exited before becoming ready ({status}); {}",
                stderr_note(stderr_tail.as_ref())
            );
        }
        if Instant::now() >= deadline {
            let mut handle = SidecarHandle { child, uds_path: uds_path.clone() };
            handle.child.kill().ok();
            handle.child.wait().ok();
            anyhow::bail!(
                "egress-proxy sidecar did not bind {uds_path:?} + write {ca_path:?} within \
                 {READY_TIMEOUT:?}; {}",
                stderr_note(stderr_tail.as_ref())
            );
        }
        std::thread::sleep(READY_POLL);
    }
    Ok(SidecarHandle { child, uds_path })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_uses_proxy_egress_and_net_client() {
        let p = proxy_policy(Path::new("/opt/proxy"), &["example.com".into()], Path::new("/scratch"), "web-fetch", None, false, false, None);
        assert!(matches!(p.net, Net::ProxyEgress));
        assert!(matches!(p.profile, Profile::WorkerNetClient));
        assert!(p.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
        assert!(p.fs_write.contains(&PathBuf::from("/scratch")));
        // env carries the UDS path + allowlist + worker name.
        let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
        assert_eq!(env[ENV_UDS], "/scratch/egress.sock");
        assert_eq!(env[ENV_ALLOWLIST], r#"["example.com"]"#);
        assert_eq!(env[ENV_WORKER], "web-fetch");
    }

    /// Regression pin for the live-gate bug (5b-4a): the sidecar spawn must
    /// derive the worker-prelude lockdown env from the policy. Without it the
    /// proxy self-applied Landlock WITHOUT the fs_read grants, so post-lockdown
    /// glibc could not read /etc/resolv.conf|hosts|nsswitch.conf and every
    /// DNS-needing CONNECT failed EAI_AGAIN on Linux (hermetic literal-IP
    /// suites stayed green, hiding it) — and ran with no seccomp at all.
    /// `spawn_sidecar` feeds `proxy_policy` through `derive_lockdown_env`;
    /// this pins what that derivation must yield for the proxy's policy.
    #[test]
    fn derived_proxy_policy_carries_lockdown_env_for_dns() {
        let p = proxy_policy(Path::new("/opt/proxy"), &["matrix.example.org:443".into()], Path::new("/scratch"), "matrix", None, true, true, None);
        let d = crate::tool_host::derive_lockdown_env(&p);
        let env: std::collections::HashMap<_, _> = d.env.into_iter().collect();
        assert_eq!(env["KASTELLAN_SECCOMP_PROFILE"], "net_client");
        let ro: Vec<String> = serde_json::from_str(&env["KASTELLAN_LANDLOCK_RO"]).unwrap();
        for path in ["/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf"] {
            assert!(ro.iter().any(|r| r == path), "Landlock RO must grant {path}");
        }
        let rw: Vec<String> = serde_json::from_str(&env["KASTELLAN_LANDLOCK_RW"]).unwrap();
        assert!(rw.iter().any(|r| r == "/scratch"), "Landlock RW must grant the scratch dir");
        // Long-lived sidecar: no cumulative RLIMIT_CPU (cpu_ms == 0 ⇒ env omitted).
        assert!(!env.contains_key("KASTELLAN_CPU_MS"), "no CPU rlimit for a long-lived sidecar");
    }

    /// Issue #395: the CPU cap is lifetime-scoped. A long-lived channel sidecar
    /// (matrix, weeks) must carry NO cumulative RLIMIT_CPU — a bounded cap would
    /// eventually SIGKILL it mid-flight now that the lockdown env actually
    /// reaches the proxy (post `e70174b`).
    #[test]
    fn proxy_policy_long_lived_has_no_cpu_cap() {
        let p = proxy_policy(
            Path::new("/opt/proxy"), &["matrix.example.org:443".into()],
            Path::new("/scratch"), "matrix", None, true, true, None,
        );
        assert_eq!(p.cpu_ms, 0, "long-lived sidecar must have no cumulative CPU cap");
    }

    /// Issue #395: a short-lived per-tool-call sidecar (web-fetch) lives 1:1 with
    /// its single dispatch, so it keeps a bounded RLIMIT_CPU as defense-in-depth
    /// — the only per-process CPU-governance primitive on macOS. This is the
    /// path `e70174b` had regressed to `0` blanket-wide.
    #[test]
    fn proxy_policy_short_lived_keeps_bounded_cpu_cap() {
        let p = proxy_policy(
            Path::new("/opt/proxy"), &["example.com".into()],
            Path::new("/scratch"), "web-fetch", None, false, false, None,
        );
        assert_eq!(
            p.cpu_ms, SHORT_LIVED_SIDECAR_CPU_MS,
            "short-lived sidecar must keep a bounded CPU cap",
        );
        assert!(p.cpu_ms > 0);
    }

    /// The short-lived cap must survive lockdown-env derivation as
    /// `KASTELLAN_CPU_MS` (the wire form the worker prelude reads for
    /// `setrlimit(RLIMIT_CPU)`) — the long-lived case omits it entirely (pinned
    /// by `derived_proxy_policy_carries_lockdown_env_for_dns`).
    #[test]
    fn derived_short_lived_policy_carries_cpu_ms_env() {
        let p = proxy_policy(
            Path::new("/opt/proxy"), &["example.com".into()],
            Path::new("/scratch"), "web-fetch", None, false, false, None,
        );
        let d = crate::tool_host::derive_lockdown_env(&p);
        let env: std::collections::HashMap<_, _> = d.env.into_iter().collect();
        assert_eq!(
            env["KASTELLAN_CPU_MS"],
            SHORT_LIVED_SIDECAR_CPU_MS.to_string(),
            "short-lived sidecar must derive a CPU rlimit env",
        );
    }

    #[test]
    fn proxy_policy_omits_pins_env_when_none() {
        let p = proxy_policy(Path::new("/bin/proxy"), &["example.com".into()], Path::new("/scratch"), "web-fetch", None, false, false, None);
        let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
        assert!(!env.contains_key(ENV_PINS));
    }

    #[test]
    fn proxy_policy_includes_pins_env_when_set() {
        let pins = r#"{"api.anthropic.com":["sha256/AAAA"]}"#;
        let p = proxy_policy(Path::new("/bin/proxy"), &["example.com".into()], Path::new("/scratch"), "web-fetch", Some(pins), false, false, None);
        let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
        assert_eq!(env[ENV_PINS], pins);
    }

    #[test]
    fn proxy_policy_sets_disable_mitm_env_when_requested() {
        let p = proxy_policy(
            Path::new("/bin/proxy"), &["example.com:443".into()],
            Path::new("/scratch"), "browser-driver", None, true, false, None,
        );
        let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
        assert_eq!(env[ENV_DISABLE_MITM], "1");
    }

    #[test]
    fn proxy_policy_omits_disable_mitm_env_when_false() {
        let p = proxy_policy(
            Path::new("/bin/proxy"), &["example.com:443".into()],
            Path::new("/scratch"), "web-fetch", None, false, false, None,
        );
        let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
        assert!(!env.contains_key(ENV_DISABLE_MITM));
    }

    #[test]
    fn proxy_policy_includes_upstream_extra_ca_env_and_fs_read_when_set() {
        let ca = PathBuf::from("/etc/localmail/ca.pem");
        let p = proxy_policy(
            Path::new("/bin/proxy"), &["127.0.0.1:8443".into()],
            Path::new("/scratch"), "mail", None, false, false, Some(&ca),
        );
        let env: std::collections::HashMap<_, _> = p.env.iter().cloned().collect();
        assert_eq!(env[ENV_UPSTREAM_EXTRA_CA], "/etc/localmail/ca.pem");
        assert!(p.fs_read.contains(&ca), "the extra CA must be bound into the proxy jail");
    }

    #[test]
    fn proxy_policy_omits_upstream_extra_ca_when_none() {
        let p = proxy_policy(
            Path::new("/bin/proxy"), &["example.com".into()],
            Path::new("/scratch"), "web-fetch", None, false, false, None,
        );
        let env: std::collections::HashMap<_, _> = p.env.iter().cloned().collect();
        assert!(!env.contains_key(ENV_UPSTREAM_EXTRA_CA));
        assert!(!p.fs_read.contains(&PathBuf::from("/etc/localmail/ca.pem")));
    }

    /// The default (no extra CA) must stay unconditionally spawnable, in either
    /// MITM posture — the precondition check may not gate the existing paths.
    #[test]
    fn check_upstream_extra_ca_accepts_absent_ca_in_either_posture() {
        assert!(check_upstream_extra_ca(false, None).is_ok());
        assert!(check_upstream_extra_ca(true, None).is_ok());
    }

    #[test]
    fn check_upstream_extra_ca_accepts_absolute_path_under_mitm() {
        let ca = PathBuf::from("/etc/localmail/ca.pem");
        assert!(check_upstream_extra_ca(false, Some(&ca)).is_ok());
    }

    /// A relative path would be rejected far downstream by the Linux backend's
    /// `fs_read` validation, naming the sandbox instead of the field. Catch it
    /// here, before anything is spawned.
    #[test]
    fn check_upstream_extra_ca_rejects_relative_path() {
        let ca = PathBuf::from("certs/ca.pem");
        let err = check_upstream_extra_ca(false, Some(&ca)).expect_err("relative path must fail");
        assert!(err.to_string().contains("absolute"), "unhelpful error: {err}");
    }

    /// A transparent tunnel never re-originates TLS, so an extra upstream anchor
    /// can do nothing. Silently ignoring it would leave the operator believing a
    /// self-signed private origin is reachable — fail loud instead.
    #[test]
    fn check_upstream_extra_ca_rejects_pairing_with_disable_mitm() {
        let ca = PathBuf::from("/etc/localmail/ca.pem");
        let err =
            check_upstream_extra_ca(true, Some(&ca)).expect_err("disable_mitm pairing must fail");
        assert!(err.to_string().contains("disable_mitm"), "unhelpful error: {err}");
    }

    /// With no drain thread there is nothing to report, and the note must say so
    /// rather than claim a clean startup. (The populated case is covered by the
    /// sandbox-gated `sidecar_with_unreadable_extra_ca_fails_fast_with_reason`
    /// in `core/tests/egress_proxy_e2e.rs`, which needs a real proxy binary.)
    #[test]
    fn stderr_note_without_a_tail_says_nothing_was_captured() {
        assert_eq!(stderr_note(None), "no stderr captured");
    }

    /// A tail that already has lines is reported immediately — the settle poll
    /// must not delay the common case where the drain already flushed.
    #[test]
    fn stderr_note_reports_captured_lines() {
        let tail = crate::worker_stderr::StderrTail::new(4);
        crate::worker_stderr::drain_reader(
            0,
            std::io::Cursor::new(b"Error: build upstream TLS config: upstream extra CA: read\n"),
            Some(&tail),
        );
        let note = stderr_note(Some(&tail));
        assert!(note.contains("upstream extra CA"), "note lost the reason: {note}");
    }
}
