//! Micro-VM (Firecracker) test preflight: image discovery, launcher
//! discovery, and the `[SKIP]` early-returns that guard every
//! `*_firecracker_*_e2e.rs` integration test.
//!
//! # Why this module exists (issue #475)
//!
//! `skip_if_no_microvm` / `locate_microvm_run` / `image_dir` /
//! `firecracker_backend` were byte-copied into **15** integration-test
//! files. Only the rootfs filename genuinely differed between them.
//!
//! That is worse than ordinary duplication because these are the
//! **`[SKIP]` helpers**. A test that skips prints a `[SKIP]` line and
//! then *passes*, so a copy that skips for the wrong reason — or prints
//! a hint that sends the operator down the wrong path — is
//! indistinguishable from a genuinely green run. `CLAUDE.md` calls this
//! the false-green pattern; 15 copies is that pattern multiplied.
//!
//! It was not hypothetical. By the time this module was written the
//! copies had already diverged: two of the 15 told the operator to run
//! `cargo build -p kastellan-microvm-run` **without `--release`**, while
//! [`launcher_candidates`] probes `target/release` **first**. Following
//! that hint rebuilds the debug binary, leaves a stale release binary in
//! place, and silently runs *old* launcher code — the exact failure
//! recorded in the `firecracker-e2e-stale-release-launcher` note, which
//! had already cost one false bug report (#362).
//!
//! # Layout
//!
//! The pure, host-independent parts ([`build_script_for`],
//! [`launcher_candidates`], the message builders) are **not** cfg-gated,
//! so they compile and unit-test on macOS as well as Linux. Only the
//! functions that name a Firecracker type are `#[cfg(target_os =
//! "linux")]`. That split is deliberate: macOS compiles `cfg(linux)`
//! code *out*, so anything behind the gate is verified only by the DGX
//! run, and the two facts most worth protecting — the build-hint table
//! and the release-before-debug ordering — are exactly the ones that
//! need no VM to check.

use std::path::{Path, PathBuf};

/// Where the guest kernel and rootfs images live when the operator has
/// not overridden `KASTELLAN_MICROVM_DIR`.
///
/// Provisioned root-owned, group-writable (1775), with a root-owned
/// vmlinux the agent cannot replace (#479), by the one-time
/// `sudo scripts/linux/install-firecracker-vsock.sh`.
pub const DEFAULT_IMAGE_DIR: &str = "/var/lib/kastellan/microvm";

/// The VMM launcher binary. The Firecracker backend spawns this by
/// **bare name** via a `PATH` lookup, which is why
/// [`skip_if_no_microvm`] prepends its build directory to `PATH`.
///
/// [`launcher_skip_message`] also uses this as the `cargo build -p`
/// **package** name — true today because the crate and its binary share
/// the name. If they ever diverge, split this into two consts.
pub const LAUNCHER_BIN: &str = "kastellan-microvm-run";

/// Build profiles probed for [`LAUNCHER_BIN`], **most-preferred first**.
///
/// `release` precedes `debug` and that order is load-bearing, not
/// stylistic: if both exist the release binary wins, so a contributor
/// who rebuilds only the debug binary keeps running whatever stale code
/// is sitting in `target/release`. Every operator-facing hint in this
/// module therefore says `--release`; see the module docs.
const LAUNCHER_PROFILES: [&str; 2] = ["release", "debug"];

mod images;
pub use images::{build_script_for, GUEST_KERNEL_LIB};

#[cfg(test)]
mod kernel_pin_tests;

/// The repository root, derived from this crate's manifest dir.
///
/// Test-only, and shared by both child test modules — hoisted here in the
/// #667 split rather than copied, since two copies of "where is the repo"
/// is exactly the duplication this module was created to end.
#[cfg(test)]
pub(crate) fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests-common has a workspace parent")
        .to_path_buf()
}


/// Candidate [`LAUNCHER_BIN`] paths under a workspace `target/`
/// directory, in probe order (see [`LAUNCHER_PROFILES`]).
///
/// Pure: it does not touch the filesystem, so a test can assert the
/// ordering without building anything.
pub fn launcher_candidates(target_dir: &Path) -> Vec<PathBuf> {
    LAUNCHER_PROFILES.iter().map(|profile| target_dir.join(profile).join(LAUNCHER_BIN)).collect()
}

/// The `[SKIP]` line printed when the Firecracker probe fails.
///
/// Pure so the wording is testable. Names the rootfs (probe failures are
/// usually a missing image, and "which image?" is the operator's first
/// question) and, when known, the script that builds it.
pub fn probe_skip_message(rootfs: &str, err: &str) -> String {
    let mut msg = crate::skip::skip_line(&format!(
        "firecracker probe failed (need {rootfs} + KVM + vsock): {err}"
    ));
    if let Some(script) = build_script_for(rootfs) {
        msg.push_str(&format!("       build the rootfs with: bash {script}\n"));
    }
    msg
}

/// The `[SKIP]` line printed when the VMM launcher has not been built.
///
/// Says `--release` deliberately: see [`LAUNCHER_PROFILES`].
pub fn launcher_skip_message() -> String {
    crate::skip::skip_line(&format!(
        "{LAUNCHER_BIN} not built; run `cargo build --release -p {LAUNCHER_BIN}`"
    ))
}

/// The directory holding `vmlinux` + the rootfs images, honouring the
/// `KASTELLAN_MICROVM_DIR` override.
///
/// An empty or whitespace-only override falls back to
/// [`DEFAULT_IMAGE_DIR`] rather than resolving paths against `""`.
///
/// Returns `String` because several call sites hand it straight to
/// policy builders that take one.
pub fn image_dir() -> String {
    std::env::var("KASTELLAN_MICROVM_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_IMAGE_DIR.to_string())
}

/// The built [`LAUNCHER_BIN`], or `None` if neither profile has one.
///
/// Resolves the workspace `target/` from this crate's manifest dir, so
/// it is correct regardless of the caller's working directory.
pub fn locate_microvm_run() -> Option<PathBuf> {
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests-common has a workspace parent")
        .join("target");
    launcher_candidates(&target).into_iter().find(|p| p.is_file())
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::Arc;

    use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
    use kastellan_sandbox::{SandboxBackend, SandboxBackendKind, SandboxBackends};

    use super::{image_dir, launcher_skip_message, locate_microvm_run, probe_skip_message};

    /// The kernel + rootfs pair for `rootfs` (a bare filename such as
    /// `"web-fetch.ext4"`) inside [`image_dir`].
    pub fn firecracker_image_for(rootfs: &str) -> FirecrackerImage {
        let dir = std::path::PathBuf::from(image_dir());
        FirecrackerImage { kernel_path: dir.join("vmlinux"), rootfs_path: dir.join(rootfs) }
    }

    /// Returns `true` if this host cannot boot `rootfs`, after printing a
    /// `[SKIP]` line saying which prerequisite is missing. Callers
    /// `return` immediately.
    ///
    /// Two gates, in the order an operator can act on them:
    ///
    /// 1. the Firecracker probe (`/dev/kvm`, `/dev/vhost-vsock`, and the
    ///    rootfs + kernel actually present), and
    /// 2. the VMM launcher being built.
    ///
    /// With VMM confinement on (`KASTELLAN_MICROVM_CONFINE_VMM` unset — the
    /// default), the probe *also* fails closed on a missing bwrap or user
    /// cgroup (the slice-5a gate), so a host without the AppArmor profile or
    /// a systemd user session `[SKIP]`s here too — read the probe error
    /// before assuming a KVM/vsock problem.
    ///
    /// On success it prepends the launcher's directory to `PATH`,
    /// because the backend spawns it by bare name. That is a
    /// process-global mutation, but each integration-test binary is its
    /// own process and the `Once` makes repeated calls idempotent.
    /// Hoisting these 15 copies into one shared `Once` is strictly
    /// better than 15 independent ones.
    pub fn skip_if_no_microvm(rootfs: &str) -> bool {
        if let Err(e) = LinuxFirecracker::probe(&firecracker_image_for(rootfs)) {
            eprint!("{}", probe_skip_message(rootfs, &e.to_string()));
            return true;
        }
        match locate_microvm_run() {
            Some(bin) => {
                use std::sync::Once;
                static PATH_ONCE: Once = Once::new();
                PATH_ONCE.call_once(|| {
                    let dir = bin.parent().expect("launcher path has a parent").to_path_buf();
                    let cur = std::env::var_os("PATH").unwrap_or_default();
                    let mut paths = vec![dir];
                    paths.extend(std::env::split_paths(&cur));
                    let joined = std::env::join_paths(paths).expect("join PATH");
                    std::env::set_var("PATH", joined);
                });
                false
            }
            None => {
                eprint!("{}", launcher_skip_message());
                true
            }
        }
    }

    /// The Firecracker micro-VM backend, resolved through the same
    /// registry production uses.
    pub fn firecracker_backend() -> Arc<dyn SandboxBackend> {
        SandboxBackends::default_for_current_os()
            .resolve(Some(SandboxBackendKind::FirecrackerVm), None)
    }
}

#[cfg(target_os = "linux")]
pub use linux::{firecracker_backend, firecracker_image_for, skip_if_no_microvm};

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression this module was filed for: the launcher is probed
    /// release-first, so every operator-facing hint must say `--release`.
    /// Two of the 15 original copies said plain `cargo build -p …`,
    /// which rebuilds debug and leaves a stale release binary running.
    #[test]
    fn release_is_probed_before_debug() {
        let candidates = launcher_candidates(Path::new("/ws/target"));
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/ws/target/release").join(LAUNCHER_BIN),
                PathBuf::from("/ws/target/debug").join(LAUNCHER_BIN),
            ]
        );
    }

    #[test]
    fn launcher_hint_says_release() {
        let msg = launcher_skip_message();
        assert!(msg.contains("--release"), "hint must say --release, got: {msg}");
        assert!(msg.contains("[SKIP]"), "must be greppable as a skip: {msg}");
    }

    #[test]
    fn probe_message_names_the_rootfs_and_its_build_script() {
        let msg = probe_skip_message("web-fetch.ext4", "no /dev/kvm");
        assert!(msg.contains("web-fetch.ext4"), "must name the image: {msg}");
        assert!(msg.contains("no /dev/kvm"), "must carry the cause: {msg}");
        assert!(msg.contains("build-web-fetch-rootfs.sh"), "must point at the builder: {msg}");
    }

    #[test]
    fn probe_message_omits_the_build_hint_for_an_unknown_rootfs() {
        let msg = probe_skip_message("mystery.ext4", "boom");
        assert!(msg.contains("mystery.ext4"));
        assert!(!msg.contains("build the rootfs with"), "must not invent a script: {msg}");
    }
}
