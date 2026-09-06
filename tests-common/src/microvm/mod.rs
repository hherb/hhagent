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

pub mod freshness;
mod images;
pub use freshness::{freshness, indeterminate_reason, stale_reason, BakedDigest, Freshness};
pub use images::{
    baked_for, build_script_for, image_entry, BakedBinary, RootfsImage, GUEST_INIT_BIN,
    GUEST_INIT_IN_IMAGE, GUEST_KERNEL_LIB, REBUILD_ALL_SCRIPT,
};

#[cfg(test)]
mod kernel_pin_tests;

/// The operator's demand that a micro-VM e2e actually *run* rather than
/// report green having skipped (issue #667, honouring #653's convention).
///
/// Same name shape and the same `env_flag_enabled` dialect as
/// [`crate::gliner_e2e::REQUIRE_ENV`]. Unset, every unmet precondition
/// prints `[SKIP]` and the test passes, which is what keeps a plain
/// `cargo test` green on a host with no KVM. Truthy, each one panics
/// naming itself.
///
/// This is not decoration. Two documented false greens on this exact path
/// were skips nobody could turn into failures: `firecracker` sitting off
/// the non-interactive ssh `PATH` (the whole suite skips-as-passes), and a
/// rootfs image too old to contain the code under test.
pub const REQUIRE_ENV: &str = "KASTELLAN_MICROVM_REQUIRE_E2E";

/// What an unmet micro-VM precondition means for *this* run.
///
/// Reads [`REQUIRE_ENV`] through the one project flag dialect
/// (`1|true|yes|on`, trimmed, case-insensitive) rather than a strict
/// `Some("1")` — the strict form is the operator-facing skew #654 was filed
/// about, and re-deriving it here would reintroduce it.
pub fn require_action() -> crate::gliner_e2e::UnmetAction {
    crate::gliner_e2e::unmet_action(std::env::var(REQUIRE_ENV).ok())
}

/// Report an unmet micro-VM precondition: `[SKIP]` and return `true`, or
/// panic when the operator demanded a real run via [`REQUIRE_ENV`].
///
/// Returns `true` so callers can `return` straight out of the test, which
/// keeps every call site a one-liner and the `[SKIP]`-as-pass path
/// unchanged from before #667.
///
/// # Panics
///
/// Under [`crate::gliner_e2e::UnmetAction::Fail`], naming both the knob (so
/// the operator reads it as their own demand rather than as a regression)
/// and the reason (so they know what to fix).
pub fn report_unmet_microvm(reason: &str) -> bool {
    report_unmet_microvm_to(reason, &mut std::io::stderr())
}

/// [`report_unmet_microvm`] with the skip line written to `out`.
///
/// Exists so a unit test can prove the Skip arm **emits** the line without
/// emitting a real `[SKIP]` into the run it is protecting — asserting on
/// [`crate::skip::skip_line`] alone would leave the `eprint!` deletable with
/// the suite still green, and `grep -c '^\[SKIP\]'` is how a run is audited
/// here.
///
/// # Panics
///
/// As [`report_unmet_microvm`].
pub fn report_unmet_microvm_to(reason: &str, out: &mut dyn std::io::Write) -> bool {
    match require_action() {
        crate::gliner_e2e::UnmetAction::Fail => panic!(
            "{REQUIRE_ENV} demanded a real micro-VM end-to-end run, but a precondition \
             is unmet: {}",
            crate::skip::one_line(reason)
        ),
        crate::gliner_e2e::UnmetAction::Skip => {
            let _ = write!(out, "{}", crate::skip::skip_line(reason));
            true
        }
    }
}

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

/// Why the Firecracker probe refused, **without rendering a verdict**.
///
/// The `*_or_reason` shape #653 established: one caller skips on this, another
/// must fail on it (see [`REQUIRE_ENV`]), and a helper that has already
/// decided which cannot serve both.
///
/// Pure so the wording is testable. Names the rootfs (probe failures are
/// usually a missing image, and "which image?" is the operator's first
/// question) and, when known, the script that builds it.
///
/// One line, and that is a fix rather than a style choice: the hint used to be
/// appended *after* [`crate::skip::skip_line`] had flattened the reason, so it
/// emitted an orphan continuation line that no `grep -c '^\[SKIP\]'` could
/// attribute to anything — precisely the shape `skip_line`'s own doc comment
/// warns about.
pub fn probe_reason(rootfs: &str, err: &str) -> String {
    let mut reason = format!("firecracker probe failed (need {rootfs} + KVM + vsock): {err}");
    if let Some(script) = build_script_for(rootfs) {
        reason.push_str(&format!("; build the rootfs with: bash {script}"));
    }
    reason
}

/// The `[SKIP]` line printed when the Firecracker probe fails.
pub fn probe_skip_message(rootfs: &str, err: &str) -> String {
    crate::skip::skip_line(&probe_reason(rootfs, err))
}

/// Why the VMM launcher is unusable, **without rendering a verdict** — the
/// verdict-free half, for the same reason as [`probe_reason`].
///
/// Says `--release` deliberately: see [`LAUNCHER_PROFILES`].
pub fn launcher_reason() -> String {
    format!("{LAUNCHER_BIN} not built; run `cargo build --release -p {LAUNCHER_BIN}`")
}

/// The `[SKIP]` line printed when the VMM launcher has not been built.
pub fn launcher_skip_message() -> String {
    crate::skip::skip_line(&launcher_reason())
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

/// The workspace `target/` directory, resolved from this crate's manifest
/// dir so it is correct regardless of the caller's working directory.
pub fn workspace_target_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests-common has a workspace parent")
        .join("target")
}

/// The built [`LAUNCHER_BIN`], or `None` if neither profile has one.
pub fn locate_microvm_run() -> Option<PathBuf> {
    launcher_candidates(&workspace_target_dir()).into_iter().find(|p| p.is_file())
}

/// sha256 of a file, or `None` when it cannot be read.
///
/// Collapses "absent" and "unreadable" deliberately: for freshness both mean
/// *no usable digest*, and [`freshness`] already refuses to call that
/// `Fresh`.
fn file_digest(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).ok()?;
    Some(format!("{:x}", Sha256::digest(&bytes)))
}

/// sha256 of a file *inside* an ext4 image, read without mounting it.
///
/// `debugfs -R "dump <path> <out>"` walks the inode and pulls the file's
/// blocks, so this costs a few hundred KiB of I/O against an image that may
/// be a gigabyte, and needs no root and no loop device. `debugfs` ships with
/// `e2fsprogs`, which is already a hard prerequisite for *building* any of
/// these images (`mkfs.ext4`), so a host that can produce a rootfs can read
/// one back.
///
/// Every failure — no `debugfs`, an unreadable image, a path that is not in
/// the image — returns `None`, which [`freshness`] renders as
/// `Indeterminate` rather than as a pass. `debugfs` reports a missing file
/// on *stderr* and still exits 0, so the emptiness of the output file is the
/// signal, not the exit status.
fn image_file_digest(image: &Path, in_image: &str) -> Option<String> {
    let out = std::env::temp_dir().join(format!(
        "kastellan-freshness-{}-{}",
        std::process::id(),
        crate::temp::unique_suffix()
    ));
    let status = std::process::Command::new("debugfs")
        .arg("-R")
        .arg(format!("dump {in_image} {}", out.display()))
        .arg(image)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let digest = match status {
        Ok(s) if s.success() => file_digest(&out).filter(|_| {
            // `debugfs` creates the output file even for a missing source,
            // so a zero-length result means "not found", not "empty binary".
            std::fs::metadata(&out).map(|m| m.len() > 0).unwrap_or(false)
        }),
        _ => None,
    };
    let _ = std::fs::remove_file(&out);
    digest
}

/// Read every baked binary's digest from both ends and return the verdict.
///
/// The impure half of [`freshness`]; the decision itself stays pure and
/// unit-tested. The working-tree reference is `target/release/<binary>`
/// because that is literally the path every `build-*-rootfs.sh` copies out
/// of — not `target/debug`, which no image is ever built from.
pub fn image_freshness(rootfs: &str) -> Freshness {
    let image_path = PathBuf::from(image_dir()).join(rootfs);
    let release = workspace_target_dir().join("release");
    let digests: Vec<BakedDigest> = baked_for(rootfs)
        .iter()
        .map(|b| BakedDigest {
            name: b.target_name.to_string(),
            in_image: image_file_digest(&image_path, b.in_image),
            in_target: file_digest(&release.join(b.target_name)),
        })
        .collect();
    freshness(&digests)
}

/// Refuse to boot a rootfs image whose baked binaries differ from the ones
/// this tree builds (issue #667). Returns `true` when the caller should skip.
///
/// The three verdicts get three different treatments, and the asymmetry is
/// the design:
///
/// * [`Freshness::Fresh`] — run.
/// * [`Freshness::Stale`] — **panic, unconditionally**, whatever
///   [`REQUIRE_ENV`] says. This is not an unmet precondition the operator
///   may reasonably not have; it is positive evidence that the run about to
///   happen would prove nothing about the working tree. A `[SKIP]` here
///   would be the #667 bug wearing a different hat, and these suites are
///   `#[ignore]`d, so reaching this code means an operator explicitly asked
///   for a Firecracker run.
/// * [`Freshness::Indeterminate`] — `[WARN]` and **run anyway**, because
///   absence of a comparable digest is not evidence of staleness and
///   downgrading a real VM run to a skip would lose coverage for no gain.
///   [`REQUIRE_ENV`] turns it into a failure for an operator demanding a
///   fully-gated run.
///
/// # Panics
///
/// On [`Freshness::Stale`] always, and on [`Freshness::Indeterminate`] when
/// [`REQUIRE_ENV`] is truthy.
pub fn skip_if_image_stale(rootfs: &str) -> bool {
    skip_if_image_stale_to(rootfs, image_freshness(rootfs), &mut std::io::stderr())
}

/// [`skip_if_image_stale`] with the verdict injected and the `[WARN]` line
/// written to `out`.
///
/// The seam exists so every branch is unit-testable without a rootfs image:
/// asserting on [`stale_reason`] alone would prove the wording correct while
/// leaving the panic deletable, which is the mutation that would silently
/// restore #667.
///
/// # Panics
///
/// As [`skip_if_image_stale`].
pub fn skip_if_image_stale_to(
    rootfs: &str,
    verdict: Freshness,
    out: &mut dyn std::io::Write,
) -> bool {
    match verdict {
        Freshness::Fresh => false,
        Freshness::Stale { binary } => {
            panic!("{}", crate::skip::one_line(&stale_reason(rootfs, &binary, build_script_for(rootfs))))
        }
        Freshness::Indeterminate { not_built, unreadable_in_image } => {
            let reason = indeterminate_reason(rootfs, &not_built, &unreadable_in_image);
            if require_action() == crate::gliner_e2e::UnmetAction::Fail {
                panic!(
                    "{REQUIRE_ENV} demanded a real micro-VM end-to-end run, but a \
                     precondition is unmet: {}",
                    crate::skip::one_line(&reason)
                );
            }
            let _ = write!(out, "{}", crate::skip::warn_line(&reason));
            false
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::Arc;

    use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
    use kastellan_sandbox::{SandboxBackend, SandboxBackendKind, SandboxBackends};

    use super::{
        image_dir, launcher_reason, locate_microvm_run, probe_reason, report_unmet_microvm,
        skip_if_image_stale,
    };

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
    /// Three gates, in the order an operator can act on them:
    ///
    /// 1. the Firecracker probe (`/dev/kvm`, `/dev/vhost-vsock`, and the
    ///    rootfs + kernel actually present),
    /// 2. the VMM launcher being built, and
    /// 3. the image actually containing the code this tree builds (#667).
    ///
    /// The third is last because it is the only one that can say the run
    /// would be *meaningless* rather than impossible, and because it is the
    /// only one that PANICS rather than skipping — see
    /// [`skip_if_image_stale`].
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
            return report_unmet_microvm(&probe_reason(rootfs, &e.to_string()));
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
                // Last, because it is the only gate that can say the run
                // would be MEANINGLESS rather than impossible: everything
                // above is "this host cannot boot a VM", this is "this host
                // can, but the image predates the code you changed" (#667).
                skip_if_image_stale(rootfs)
            }
            None => report_unmet_microvm(&launcher_reason()),
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
mod preflight_tests;
