//! `kastellan-microvm-run`: the process the sandbox backend spawns as the
//! worker `Child`. Boots a Firecracker micro-VM and bridges the worker's
//! JSON-RPC stdio over hybrid vsock. Kernel logs go to `--log`, never stdout.

mod boot;
mod bridge;
mod console;
mod persistent_lock;
mod reverse_relay;

use std::process::{Command, Stdio};
use std::time::Duration;

fn arg(flag: &str) -> Option<String> {
    let mut it = std::env::args();
    while let Some(a) = it.next() {
        if a == flag { return it.next(); }
    }
    None
}

fn main() -> std::io::Result<()> {
    let config = arg("--config-file").expect("--config-file required");
    let vsock_uds = arg("--vsock-uds").expect("--vsock-uds required");
    let port: u32 = arg("--vsock-port")
        .expect("--vsock-port required")
        .parse()
        .expect("--vsock-port must be a u32");
    let log = arg("--log").unwrap_or_else(|| "/dev/null".into());

    // Per-spawn run-dir to remove on exit (#362). Optional for backward
    // compatibility with callers that don't pass it; when absent we fall back
    // to removing just the base vsock UDS, as before.
    let run_dir = arg("--run-dir");

    // Confined path (slice 5a): the bwrap jail has no $PATH, so the backend
    // resolves + binds firecracker and passes its absolute path here. Absent →
    // the bare name (resolved via $PATH), byte-identical to the pre-5a launcher.
    let firecracker_bin = arg("--firecracker-bin").unwrap_or_else(|| "firecracker".to_string());

    // Slice 5b-2: take an exclusive flock on the persistent-store image BEFORE
    // booting firecracker so two concurrent launchers can never mount the same
    // RW ext4 (page-cache corruption). Fail-closed: lock busy → log + exit non-zero.
    let _persistent_lock = if let Some(img) = arg("--persistent-image") {
        let path = std::path::Path::new(&img);
        let lock = persistent_lock::acquire(path).map_err(|e| {
            eprintln!(
                "kastellan-microvm-run: --persistent-image {img:?} is already locked \
                 by another launcher (flock failed: {e}); aborting boot (fail-closed)"
            );
            e
        })?;
        Some(lock)
    } else {
        None
    };

    // Slice 4a: when force-routed, start the egress reverse-relay BEFORE booting
    // firecracker so the host listener at `<vsock_uds>_<port>` exists before the
    // guest can dial it (firecracker connects there for a guest-initiated vsock
    // connection on that port). The detached accept loop relays each connection
    // to the host egress-proxy UDS.
    if let Some((proxy_uds, egress_port)) =
        reverse_relay::parse_reverse_relay_args(arg("--egress-uds"), arg("--egress-vsock-port"))
    {
        reverse_relay::spawn_reverse_relay(&vsock_uds, egress_port, proxy_uds)?;
    }

    // VM × broker: start a SECOND reverse-relay for the embed-broker channel
    // (port 1026), forwarding guest-initiated connections to the host broker UDS.
    // Same generic relay as egress; started before boot so its listener exists
    // before the guest dials. Independent of egress (different port + target).
    if let Some((broker_uds, broker_port)) =
        reverse_relay::parse_reverse_relay_args(arg("--broker-uds"), arg("--broker-vsock-port"))
    {
        reverse_relay::spawn_reverse_relay(&vsock_uds, broker_port, broker_uds)?;
    }

    // Boot firecracker as our child; it creates the base vsock UDS once it is
    // up. We keep our own stdout pristine for JSON-RPC.
    //
    // The guest's ttyS0 IS firecracker's stdout, so both of its streams go to
    // `console.log` inside the per-spawn run dir (#666). `--log-path` covers
    // firecracker's OWN logs and nothing that happens before it opens that
    // file; discarding these two is what made three separate production
    // defects indistinguishable. Our own stdout stays untouched — it is the
    // JSON-RPC channel.
    let console_path = console::console_log_path(run_dir.as_deref());
    let console_sink = console_path.as_deref().and_then(open_console_log);
    let fc_argv = boot::firecracker_argv(&firecracker_bin, &config, &log);
    let mut cmd = Command::new(&fc_argv[0]);
    cmd.args(&fc_argv[1..]).stdin(Stdio::null());
    match &console_sink {
        // Two independent handles onto the same file: firecracker writes the
        // guest console on stdout and its own early messages on stderr, and we
        // want them interleaved in one chronological transcript.
        Some(f) => match (f.try_clone(), f.try_clone()) {
            (Ok(o), Ok(e)) => {
                cmd.stdout(Stdio::from(o)).stderr(Stdio::from(e));
            }
            _ => {
                cmd.stdout(Stdio::null()).stderr(Stdio::null());
            }
        },
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    let mut fc = cmd.spawn()?;

    // Build the teardown guard BEFORE connecting so a panic (or early return) in
    // the connect unwinds through it and kills the already-spawned firecracker
    // child instead of orphaning it (holding KVM/vsock). The run-dir disposition
    // (remove vs keep-for-diagnostics) is decided by `teardown_run_dir`. The
    // guard owns a clone; the outer scope keeps `vsock_uds` for the connect
    // borrow below.
    let uds_for_guard = vsock_uds.clone();
    let run_dir_for_guard = run_dir.clone();
    // Operator opt-in for the "it booted but the worker misbehaved" case: a
    // boot FAILURE already keeps the run dir, but a graceful exit removes the
    // console with it, which is exactly the run you want to read when the VM
    // came up and then did the wrong thing.
    let keep_run_dir = console::flag_enabled(
        std::env::var(console::KEEP_RUN_DIR_ENV).ok().as_deref(),
    );
    let teardown = scopeguard(move || {
        let _ = fc.kill();
        // `fc.kill()` always runs (never orphan firecracker holding KVM/vsock);
        // the run-dir disposition depends on whether we are unwinding a panic.
        teardown_run_dir(
            run_dir_for_guard.as_deref(),
            &uds_for_guard,
            std::thread::panicking(),
            keep_run_dir,
        );
    });

    // Host-initiated hybrid-vsock connect: dial the base UDS and `CONNECT` to
    // the guest's listening port (DGX-verified direction). Retries while the
    // guest boots and binds its listener.
    let stream = match bridge::connect_hybrid_vsock(&vsock_uds, port, Duration::from_secs(20)) {
        Some(s) => s,
        None => {
            // Report BEFORE panicking. The panic is what makes the teardown
            // guard keep the run dir, but a panic message alone is what the
            // caller has been getting for the whole life of this launcher: the
            // core sees `Protocol(EarlyExit)` either way, and the only thing
            // that ever carries content is what we write to our own stderr —
            // which the sandbox backend pipes and the daemon drains.
            eprint!(
                "{}",
                console::boot_failure_report(
                    "the guest vsock listener did not come up within 20s",
                    console_path.as_deref(),
                    read_console_lossy(console_path.as_deref()).as_deref(),
                )
            );
            panic!("guest vsock did not come up within 20s");
        }
    };
    bridge::pump(stream);
    if keep_run_dir {
        if let Some(p) = console_path.as_deref() {
            eprintln!(
                "kastellan-microvm-run: {} set; guest console kept at {}",
                console::KEEP_RUN_DIR_ENV,
                p.display()
            );
        }
    }
    drop(teardown);
    Ok(())
}

/// Read the guest console as text, replacing invalid UTF-8 rather than failing.
///
/// `read_to_string` would return `Err` on the first invalid byte and the caller
/// would then report "the console could not be read" — the wrong diagnosis, and
/// a likely one: this is a **serial console**, and a kernel that panics or a
/// guest that dies mid-write routinely leaves partial or non-UTF-8 bytes on it.
/// Losing the whole transcript to one bad byte would recreate #666 in miniature.
fn read_console_lossy(path: Option<&std::path::Path>) -> Option<String> {
    let bytes = std::fs::read(path?).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Open the per-spawn guest-console file, or `None` if it cannot be created.
///
/// `create_new` on purpose: the run dir is minted exclusively and owner-private
/// (audit H3), so the name cannot already exist in a healthy spawn, and
/// refusing to append to a pre-existing one keeps a planted symlink from
/// redirecting the guest's output somewhere else. Mode 0600 matches the secret
/// files staged in the same dir.
///
/// A failure here is **never fatal**: the console is a diagnostic, and refusing
/// to boot a VM because its log file could not be opened would trade a
/// debugging aid for an outage. The caller falls back to discarding the
/// streams, exactly as before #666, and says so.
fn open_console_log(path: &std::path::Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
    {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!(
                "kastellan-microvm-run: could not open the guest console log at {}: {e} \
                 (booting anyway; the guest's diagnostics will be discarded)",
                path.display()
            );
            None
        }
    }
}

/// Minimal RAII guard (avoid a dep; teardown must run on every exit path).
fn scopeguard<F: FnOnce()>(f: F) -> impl Drop {
    struct G<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for G<F> { fn drop(&mut self) { if let Some(f) = self.0.take() { f(); } } }
    G(Some(f))
}

/// Decide what to clean up on launcher exit, given whether we are unwinding a
/// panic. Separated from the teardown closure so it is unit-testable without
/// booting a VM.
///
/// - Graceful exit, `--run-dir` known: remove the whole run-dir (#362). This
///   subsumes the base-UDS removal since the UDS lives inside it.
/// - **Panic** (firecracker/connect boot failure), `--run-dir` known: KEEP the
///   run-dir so firecracker's `fc.log` and the guest `console.log` survive for
///   post-mortem (#367 review, #666). The orphan sweep in the next
///   `spawn_under_policy` reclaims it once this launcher's now-dead pid is
///   observed — so this is a deferred clean, not a leak.
/// - Graceful exit with `keep` (the [`console::KEEP_RUN_DIR_ENV`] opt-in):
///   keep it too, for the "it booted but then misbehaved" case, which the panic
///   rule alone cannot reach. Same deferred reclaim.
/// - No `--run-dir` (legacy caller / direct test): fall back to removing just
///   the base vsock UDS, as before — on every path. `keep` is deliberately
///   ignored here rather than made to preserve the UDS: there is no dir to
///   keep, and leaving a stale socket behind would break the NEXT spawn.
fn teardown_run_dir(run_dir: Option<&str>, base_uds: &str, panicking: bool, keep: bool) {
    match run_dir {
        Some(dir) if !panicking && !keep => remove_run_dir(dir),
        Some(_) => {} // panic or opt-in: keep the run-dir for diagnostics.
        None => {
            let _ = std::fs::remove_file(base_uds);
        }
    }
}

/// Marker dropped when teardown removed the VM's files but could not remove the
/// run-dir itself (confined mode, slice 5a). MUST match
/// `kastellan_sandbox::linux_firecracker::cleanup::TEARDOWN_MARKER_FILE`
/// (the launcher has no dep on the sandbox crate — pinned literal in both).
const TEARDOWN_MARKER_FILE: &str = "teardown.done";

/// Best-effort removal of the per-spawn run-dir on launcher exit. Removing the
/// whole dir subsumes removing the base vsock UDS (which lives inside it).
///
/// Bare path: `remove_dir_all` removes the whole tree (immediate self-clean,
/// #362). Confined path (slice 5a): the run-dir is a `bwrap` bind-mount point,
/// so `remove_dir_all` unlinks the contents (incl. `launcher.pid`) but
/// `rmdir(2)` of the mount point returns `EBUSY` and the dir survives as an
/// empty husk with no pidfile. We then drop a teardown marker so the host-side
/// orphan sweep reclaims the husk — without it the sweep keeps every
/// pidfile-less dir (assuming mid-spawn) and the husk would leak forever. The
/// launcher runs as jail PID 1 here and cannot rewrite its host pidfile, so a
/// marker (not a pidfile) is the signal.
fn remove_run_dir(run_dir: &str) {
    if std::fs::remove_dir_all(run_dir).is_ok() {
        return;
    }
    // Couldn't remove the dir itself (confined bind-mount, or a partial failure):
    // leave the marker for the host-side sweep, which sees a plain dir and can
    // remove it. Best-effort — a failed marker write only defers reclaim.
    let _ = std::fs::write(
        std::path::Path::new(run_dir).join(TEARDOWN_MARKER_FILE),
        b"",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_run_dir_deletes_the_directory_tree() {
        let dir = std::env::temp_dir().join(format!(
            "kastellan-microvm-runtest-{}-{}",
            std::process::id(),
            "a"
        ));
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("fc.json"), "{}").unwrap();
        assert!(dir.exists());

        remove_run_dir(&dir.to_string_lossy());

        assert!(!dir.exists(), "remove_run_dir must delete the whole tree");
    }

    #[test]
    fn remove_run_dir_is_noop_on_missing_dir() {
        // Must not panic when the dir is already gone.
        remove_run_dir("/tmp/kastellan-microvm-runtest-definitely-absent-zzz");
    }

    #[cfg(unix)]
    #[test]
    fn remove_run_dir_drops_marker_when_dir_cannot_be_removed() {
        // Confined mode leaves the run-dir as a bind-mount point that rmdir can't
        // remove. We can't mount in a unit test, so reproduce the
        // "contents-gone-but-dir-survives" shape with a read-only parent: rmdir of
        // the (empty) run-dir fails, but the run-dir itself stays writable so the
        // teardown marker can be dropped for the host-side sweep.
        use std::os::unix::fs::PermissionsExt;
        let parent = std::env::temp_dir().join(format!(
            "kastellan-microvm-runtest-{}-marker-parent",
            std::process::id()
        ));
        let run = parent.join("run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();

        remove_run_dir(&run.to_string_lossy());

        let marker_present = run.join(TEARDOWN_MARKER_FILE).exists();
        // Restore write perms before asserting so cleanup always runs.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&parent).ok();

        assert!(
            marker_present,
            "an un-removable run-dir must get the teardown marker for the sweep"
        );
    }

    fn fresh_run_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kastellan-microvm-runtest-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fc.log"), "boot log").unwrap();
        std::fs::write(dir.join(console::CONSOLE_LOG_FILE), "guest console").unwrap();
        dir
    }

    #[test]
    fn teardown_removes_run_dir_on_graceful_exit() {
        let dir = fresh_run_dir("graceful");
        teardown_run_dir(Some(&dir.to_string_lossy()), "/unused", false, false);
        assert!(!dir.exists(), "graceful exit must remove the run-dir");
    }

    #[test]
    fn teardown_keeps_run_dir_on_panic_for_diagnostics() {
        // #367: a boot failure (panic) must KEEP the run-dir so fc.log survives;
        // the orphan sweep reclaims it later once the launcher pid is dead.
        // Since #666 the guest console.log is in there too, and it is the file
        // that actually names the cause.
        let dir = fresh_run_dir("panic");
        teardown_run_dir(Some(&dir.to_string_lossy()), "/unused", true, false);
        assert!(dir.exists(), "panic must keep the run-dir for post-mortem");
        assert!(dir.join("fc.log").exists(), "fc.log must survive a panic exit");
        assert!(
            dir.join(console::CONSOLE_LOG_FILE).exists(),
            "the guest console must survive a panic exit — it is the whole point of #666"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn teardown_keeps_run_dir_on_graceful_exit_when_opted_in() {
        // #666: the opt-in exists for the case the panic rule cannot reach — a
        // VM that booted fine and then misbehaved, whose console is the only
        // record and which a graceful exit would otherwise delete.
        let dir = fresh_run_dir("graceful-keep");
        teardown_run_dir(Some(&dir.to_string_lossy()), "/unused", false, true);
        assert!(
            dir.exists(),
            "the {} opt-in must survive a graceful exit",
            console::KEEP_RUN_DIR_ENV
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn teardown_removes_base_uds_when_no_run_dir() {
        // Legacy caller (no --run-dir): fall back to removing just the base UDS,
        // on every path. The keep opt-in is deliberately NOT honoured here —
        // there is no dir to keep, and a surviving socket breaks the next spawn.
        for panicking in [false, true] {
            for keep in [false, true] {
                let uds = std::env::temp_dir().join(format!(
                    "kastellan-microvm-runtest-{}-uds-{panicking}-{keep}.sock",
                    std::process::id()
                ));
                std::fs::write(&uds, "").unwrap();
                teardown_run_dir(None, &uds.to_string_lossy(), panicking, keep);
                assert!(!uds.exists(), "legacy path must remove the base UDS");
            }
        }
    }
}
