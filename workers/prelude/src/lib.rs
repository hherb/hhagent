//! `kastellan-worker-prelude`: defence-in-depth primitives that every tool
//! worker calls **from inside its own process** before it serves any request.
//!
//! ## Why a separate crate
//!
//! The parent (`core::tool_host`) already wraps every worker in `bwrap` from
//! the outside (Linux) or Seatbelt (macOS, future). That gives namespace +
//! coarse FS isolation. **This crate adds a second, finer-grained layer
//! applied from inside the worker:**
//!
//!   * **Landlock** — a kernel LSM filter that the worker installs on itself,
//!     restricting which paths it may read, write, or execute. Survives even
//!     if bwrap's mount setup is somehow circumvented.
//!   * **seccomp-bpf** — a BPF syscall filter that kills the process if it
//!     attempts a catastrophically dangerous syscall (mount, unshare, bpf,
//!     ptrace, kexec, …).
//!
//! Both layers are *worker-side*: even a worker compromised by malicious
//! tool input cannot lift them — `landlock::restrict_self()` and
//! `seccomp::apply_filter()` are one-way operations enforced by the kernel.
//!
//! ## Usage
//!
//! ```ignore
//! fn main() -> anyhow::Result<()> {
//!     let mut handler = MyHandler::from_env()?;
//!     kastellan_worker_prelude::serve_stdio(&mut handler)?;
//!     Ok(())
//! }
//! ```
//!
//! `serve_stdio` calls [`lock_down`] before dispatching any JSON-RPC traffic.
//! Workers that need finer-grained control may call [`lock_down`] directly
//! and then use `kastellan_protocol::server::serve_stdio` — but they are then
//! responsible for ensuring no I/O happens between dynamic-linker resolution
//! and the lock-down call.
//!
//! ## Cross-platform contract
//!
//! On non-Linux targets, [`lock_down`] is a no-op (returns
//! [`LockdownReport::NonLinux`]). The cross-platform contract is
//! preserved because the macOS Seatbelt backend (Phase 0b) installs the
//! equivalent containment from the parent side — the worker process on
//! macOS is launched already inside the Seatbelt profile.

#![deny(missing_debug_implementations)]

#[cfg(unix)]
pub mod child_exit;
#[cfg(target_os = "linux")]
pub mod landlock_lock;
pub mod rlimit;
#[cfg(target_os = "linux")]
pub mod seccomp_lock;

use std::io;

use kastellan_protocol::server::Handler;

/// What `serve_stdio` actually managed to install. Returned so the
/// worker can log it (and tests can assert on it).
///
/// Two-layer composition: `rlimit::apply_from_env` (cross-platform,
/// POSIX `setrlimit`) plus `lock_down` (Linux Landlock + seccomp;
/// no-op on macOS, where Seatbelt enforces containment from the parent
/// side). `rlimit` runs *before* `lock_down` so the CPU ceiling is
/// armed before any seccomp restrictions on `prlimit`-family syscalls.
#[derive(Debug)]
pub enum LockdownReport {
    /// Linux: Landlock + seccomp + rlimit.
    Linux {
        landlock: LandlockReport,
        seccomp: SeccompReport,
        rlimit: rlimit::RlimitReport,
    },
    /// macOS or other non-Linux: kernel containment is the parent's
    /// job (Seatbelt), but rlimit still applies (POSIX).
    NonLinux {
        rlimit: rlimit::RlimitReport,
    },
}

/// Status of the Landlock layer after `lock_down`.
#[derive(Debug, Clone, Copy)]
pub enum LandlockReport {
    /// Filter installed; kernel reports the requested ABI is fully supported.
    FullyEnforced,
    /// Filter installed but the kernel only supports a partial subset of
    /// the access rights we asked for. Logged but not fatal.
    PartiallyEnforced,
    /// Kernel is too old (no Landlock support, < 5.13) — filter not
    /// installed. The bwrap mount layer is still in effect, so isolation is
    /// degraded but not absent.
    KernelTooOld,
    /// Deliberately not installed — the worker set
    /// `KASTELLAN_LANDLOCK_PROFILE=none`. bwrap's mount namespace remains the
    /// filesystem-containment layer. Not an error; logged via the report.
    Disabled,
}

/// Status of the seccomp-bpf layer after `lock_down`.
#[derive(Debug, Clone, Copy)]
pub enum SeccompReport {
    /// BPF filter loaded and active.
    Installed,
    /// `KASTELLAN_SECCOMP_PROFILE` env var was missing or set to `"none"`,
    /// so no filter was applied. Useful in tests; not recommended in prod.
    Disabled,
}

#[derive(Debug, thiserror::Error)]
pub enum LockdownError {
    #[error("landlock: {0}")]
    Landlock(String),
    #[error("seccomp: {0}")]
    Seccomp(String),
    #[error("env: {0}")]
    Env(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// Apply both kernel layers, in order: Landlock first (it's a one-way FS
/// restriction), then seccomp (one-way syscall restriction).
///
/// Reads its policy from environment variables set by the parent process
/// (`core::tool_host`):
///
///   * `KASTELLAN_LANDLOCK_RW`  — JSON array of absolute paths the worker may
///     write to (its scratch dir). Read-only access to `/usr`, `/lib*`,
///     `/etc/ld.so.cache` is implicit so dynamic-linker + libc still work.
///   * `KASTELLAN_SECCOMP_PROFILE` — `"strict"`, `"net_client"`, or `"none"`.
///     `"none"` disables seccomp entirely (used in tests).
///
/// The function only fails on programmer error (malformed env, kernel ABI
/// returns an error). A kernel that lacks Landlock support is reported via
/// [`LandlockReport::KernelTooOld`], not via an error — callers should still
/// proceed, since bwrap is the primary containment layer.
///
/// **Does not apply `setrlimit`.** That's [`rlimit::apply_from_env`]'s job;
/// [`serve_stdio`] composes the two. Callers using `lock_down` directly
/// (e.g. the `lockdown-probe` binary) are responsible for invoking
/// `rlimit::apply_from_env` themselves if they want CPU-time enforcement.
/// The returned `LockdownReport` carries `rlimit: RlimitReport::Disabled`
/// from this entry point.
pub fn lock_down() -> Result<LockdownReport, LockdownError> {
    // A seccomp kill (SIGSYS) has core-dump semantics, and a worker's memory
    // holds exactly what must not persist on the host: the egress proxy's CA
    // private key, a Matrix session's E2E keys, a mail bearer token. Refuse
    // core dumps and same-uid ptrace/`/proc/<pid>/mem` reads before any
    // filter can fire (security audit 2026-09-02, F7).
    forbid_core_dumps()?;
    #[cfg(target_os = "linux")]
    {
        let landlock = landlock_lock::apply_from_env()?;
        let seccomp = seccomp_lock::apply_from_env()?;
        Ok(LockdownReport::Linux {
            landlock,
            seccomp,
            rlimit: rlimit::RlimitReport::Disabled,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(LockdownReport::NonLinux {
            rlimit: rlimit::RlimitReport::Disabled,
        })
    }
}

/// Drop-in replacement for `kastellan_protocol::server::serve_stdio` that
/// applies `rlimit::apply_from_env` and [`lock_down`] before entering
/// the request loop. This is the recommended entry point for tool
/// workers.
///
/// Order matters:
///
/// 1. **`rlimit::apply_from_env` first.** Sets `RLIMIT_CPU` before any
///    syscall restrictions land — defends against future seccomp profiles
///    that ban `prlimit64`. Cross-platform (POSIX).
/// 2. **`lock_down` second.** Linux Landlock + seccomp; no-op on macOS.
///
/// Both layers fail closed: any error returns `io::Error` and the worker
/// exits before serving any request.
pub fn serve_stdio<H: Handler>(handler: &mut H) -> io::Result<()> {
    apply_lockdown_and_report()?;
    kastellan_protocol::server::serve_stdio(handler)
}

/// Like [`serve_stdio`], but the handler is **constructed after** the
/// lockdown, inside `build`.
///
/// Use this for any worker whose handler construction spawns threads — a tokio
/// runtime for the egress-proxy CONNECT client, a `reqwest::blocking::Client`
/// (which owns a runtime thread), a connection pool. Landlock restricts only
/// the *calling* thread and is inherited by threads created afterwards; there
/// is no TSYNC for it, so a runtime built in `Handler::from_env()` *before*
/// [`serve_stdio`] ran left every one of its worker threads — the threads that
/// actually parse the network — with no Landlock at all (security audit
/// 2026-09-02, F4). Seccomp was already TSYNC'd across threads; this closes
/// the same gap for the FS layer by construction rather than by ordering
/// discipline in each worker's `main`.
///
/// `build` runs with rlimit + Landlock + seccomp already applied, so it may
/// only do what the worker's profile permits: reading its fs_read paths (the
/// egress CA PEM, a token file), building runtimes and TLS configs, binding
/// sockets in its RW paths. A construction failure is returned before any
/// request is read.
pub fn serve_stdio_with<H, F, E>(build: F) -> io::Result<()>
where
    H: Handler,
    F: FnOnce() -> Result<H, E>,
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    apply_lockdown_and_report()?;
    let mut handler = build().map_err(io::Error::other)?;
    kastellan_protocol::server::serve_stdio(&mut handler)
}

/// rlimit → Landlock → seccomp, then the one-line stderr report. Shared by
/// both serve entry points so the ordering cannot drift between them.
fn apply_lockdown_and_report() -> io::Result<()> {
    let rlimit = rlimit::apply_from_env().map_err(|e| io::Error::other(e.to_string()))?;

    let report = match lock_down() {
        Ok(LockdownReport::Linux {
            landlock, seccomp, ..
        }) => LockdownReport::Linux {
            landlock,
            seccomp,
            rlimit,
        },
        Ok(LockdownReport::NonLinux { .. }) => LockdownReport::NonLinux { rlimit },
        Err(e) => {
            return Err(io::Error::other(e.to_string()));
        }
    };

    // Single, structured line on stderr so the parent can capture it
    // for the audit log without parsing JSON. Workers that want
    // richer logging can call `rlimit::apply_from_env` + `lock_down`
    // themselves and skip this.
    eprintln!("kastellan-worker-prelude: lockdown {report:?}");
    Ok(())
}

/// `RLIMIT_CORE = 0` (hard and soft, so it cannot be raised back) plus, on
/// Linux, `PR_SET_DUMPABLE = 0` — which also closes same-uid `ptrace` and
/// `/proc/<pid>/mem` against this process. Fails closed: a worker that cannot
/// refuse to dump does not serve.
fn forbid_core_dumps() -> Result<(), LockdownError> {
    let zero = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: setrlimit reads a fully-initialised struct we own; it modifies
    // only this process's limits.
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_CORE, &zero) };
    if rc != 0 {
        return Err(LockdownError::Io(io::Error::last_os_error()));
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: prctl with immediate arguments; process-wide flag only.
        let rc = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
        if rc != 0 {
            return Err(LockdownError::Io(io::Error::last_os_error()));
        }
    }
    Ok(())
}
