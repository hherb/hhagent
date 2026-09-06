//! The rootfs-image registry: which images the micro-VM e2e suite boots,
//! and which script builds each one.
//!
//! Movement-only split out of `microvm.rs`, which had reached 807 lines.
//! The tree's rule is to split *before* the change that grows a file, in a
//! commit whose `#[test]` name set is verifiable either side; issue #667
//! adds the freshness check that would otherwise have grown it further.

/// Every rootfs image the e2e suite boots, paired with the script that
/// builds it (repo-relative).
///
/// This is an explicit table rather than a derived
/// `build-<stem>-rootfs.sh` convention because two entries break that
/// convention and a derived name would produce a hint pointing at a file
/// that does not exist:
///
/// * `python-exec.ext4` is built by plain `build-rootfs.sh` (it was the
///   first rootfs, before the per-worker naming settled), and
/// * `kv-demo.ext4`'s script lives under `scripts/workers/kv-demo/`,
///   not `scripts/workers/microvm/` like every other one.
///
/// `every_build_script_exists` pins the whole table against the working
/// tree, so renaming or moving a script fails the unit test instead of
/// silently misleading whoever hits the `[SKIP]`.
pub(super) const ROOTFS_BUILD_SCRIPTS: &[(&str, &str)] = &[
    ("python-exec.ext4", "scripts/workers/microvm/build-rootfs.sh"),
    ("web-fetch.ext4", "scripts/workers/microvm/build-web-fetch-rootfs.sh"),
    ("web-search.ext4", "scripts/workers/microvm/build-web-search-rootfs.sh"),
    ("web-research.ext4", "scripts/workers/microvm/build-web-research-rootfs.sh"),
    ("browser-driver.ext4", "scripts/workers/microvm/build-browser-driver-rootfs.sh"),
    ("matrix.ext4", "scripts/workers/microvm/build-matrix-rootfs.sh"),
    ("net-demo.ext4", "scripts/workers/microvm/build-net-demo-rootfs.sh"),
    ("kv-demo.ext4", "scripts/workers/kv-demo/build-kv-demo-rootfs.sh"),
];

/// The shared guest-kernel pin sourced by every `build-*-rootfs.sh`
/// (repo-relative).
///
/// All eight build scripts fetch the *same* `vmlinux`. Before issue #471
/// each one carried its own copy of the URL, the arch `case`, and an
/// unchecked `curl`. This file is now the single place any of that is
/// written down; `kernel_pin_is_the_only_place_the_kernel_url_appears`
/// keeps it that way.
pub const GUEST_KERNEL_LIB: &str = "scripts/workers/microvm/lib/guest-kernel.sh";

/// The build script for `rootfs`, or `None` for an image this table does
/// not know about.
///
/// Pure — no filesystem access, so it is unit-testable on any host.
/// Callers fold the `None` case into a generic hint rather than guessing
/// a filename; a guessed hint is the failure mode this module exists to
/// prevent.
pub fn build_script_for(rootfs: &str) -> Option<&'static str> {
    ROOTFS_BUILD_SCRIPTS.iter().find(|(name, _)| *name == rootfs).map(|(_, script)| *script)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::microvm::repo_root;

    /// Every hint must name a script that actually exists, so a rename
    /// or a move breaks this test rather than silently sending an
    /// operator to a nonexistent path. This is the pin that lets the
    /// table stay hand-written instead of derived.
    #[test]
    fn every_build_script_exists() {
        let root = repo_root();
        for (rootfs, script) in ROOTFS_BUILD_SCRIPTS {
            let path = root.join(script);
            assert!(path.is_file(), "build script for {rootfs} is missing: {}", path.display());
        }
    }

    #[test]
    fn build_script_lookup_hits_the_two_convention_breakers() {
        // Neither of these follows `build-<stem>-rootfs.sh` under
        // `scripts/workers/microvm/`, which is why the table is explicit.
        assert_eq!(
            build_script_for("python-exec.ext4"),
            Some("scripts/workers/microvm/build-rootfs.sh")
        );
        assert_eq!(
            build_script_for("kv-demo.ext4"),
            Some("scripts/workers/kv-demo/build-kv-demo-rootfs.sh")
        );
    }

    #[test]
    fn build_script_is_none_for_an_unknown_rootfs() {
        // Callers must fall back to a generic hint, never guess a name.
        assert_eq!(build_script_for("not-a-real-worker.ext4"), None);
    }
}
