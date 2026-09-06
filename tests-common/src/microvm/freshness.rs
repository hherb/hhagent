//! Is this rootfs image old enough to be testing code that no longer
//! exists? (issue #667)
//!
//! # The failure this exists to stop
//!
//! `kv_demo_firecracker_persistent_e2e` once failed with
//!
//! ```text
//! persistent ext4 store must survive a VM respawn (SIGKILL + reboot); got {"value":null}
//! ```
//!
//! which reads exactly like a regression in the persistent-store path. It
//! was not: `/var/lib/kastellan/microvm/kv-demo.ext4` had been built in June,
//! and rebuilding it made the test pass unchanged.
//!
//! Every image bakes its own copy of `kastellan-microvm-init` and of its
//! worker (see [`super::images`]), so a guest-side change is invisible to
//! the Firecracker e2es until the affected image is rebuilt. That makes a
//! stale image the micro-VM twin of the stale `target/release` launcher and
//! of the `.venv` that silently belonged to another OS: **a fixture whose
//! staleness is invisible turns a gate into a formality.** The owed gate for
//! audit item W-2 could have run start to finish against stale images and
//! reported green having tested none of it.
//!
//! # What is compared, and what that is worth
//!
//! The image's mtime against the mtimes of the `target/release/` binaries it
//! bakes. mtime is a cheap approximation and its limits are worth stating
//! plainly, because a check whose strength is overestimated is worse than
//! none:
//!
//! * it **does** catch the case that actually happens — you rebuild a
//!   binary, then run the e2e without rebuilding the image;
//! * it does **not** catch an image rebuilt from a stale checkout, nor
//!   source edits that were never compiled. Nothing here claims otherwise.
//!
//! # Why the verdict is a value and not a `bool`
//!
//! Three outcomes need three different operator responses, and collapsing
//! [`Freshness::Indeterminate`] into either neighbour is what would make
//! this module dishonest. "No reference binary is built" is not evidence of
//! freshness *or* of staleness, and the one thing it must never do is render
//! identically to `Fresh`.

use std::time::SystemTime;

/// The verdict for one rootfs image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    /// Every baked binary that exists is at least as old as the image, so
    /// the image can contain their current code.
    Fresh,
    /// At least one baked binary is **newer** than the image, so the image
    /// certainly does not contain that binary's current code. Names the
    /// newest offender — the one whose rebuild is furthest ahead of the
    /// image, and so the clearest thing to put in front of the operator.
    Stale { binary: String },
    /// None of the baked binaries is built, so nothing can be compared.
    ///
    /// Deliberately **not** folded into [`Freshness::Fresh`]: absence of a
    /// reference is absence of evidence. Callers run the test but say so
    /// out loud, and the `REQUIRE_E2E` knob turns it into a failure for an
    /// operator who is demanding a fully-gated run.
    Indeterminate,
}

/// Compare an image's mtime against the mtimes of the binaries baked into
/// it.
///
/// `baked` pairs each binary's bare name with its mtime, or `None` when that
/// binary is not built. Pure — the caller does the `stat`ting — so every
/// branch below is unit-testable on any host, including the macOS one that
/// compiles the whole Firecracker backend out.
///
/// Equal mtimes count as fresh. A build script compiles the binary and then
/// writes the image, so `image >= binary` is the normal ordering, and
/// filesystem timestamp granularity can legitimately collapse the two into
/// the same instant. Treating equality as stale would fail correct builds.
pub fn freshness(image_mtime: SystemTime, baked: &[(&str, Option<SystemTime>)]) -> Freshness {
    let newest_newer_than_image = baked
        .iter()
        .filter_map(|(name, mtime)| mtime.map(|m| (*name, m)))
        .filter(|(_, m)| *m > image_mtime)
        .max_by_key(|(_, m)| *m);

    match newest_newer_than_image {
        Some((name, _)) => Freshness::Stale { binary: name.to_string() },
        // No binary is newer. That is only meaningful if at least one
        // binary was actually there to compare against.
        None if baked.iter().any(|(_, mtime)| mtime.is_some()) => Freshness::Fresh,
        None => Freshness::Indeterminate,
    }
}

/// The operator-facing reason for a [`Freshness::Stale`] verdict.
///
/// Pure so the wording is testable without touching a filesystem. Names the
/// image, the binary that overtook it, the script that rebuilds *this* image
/// and the script that rebuilds them all — because the two build-script
/// directories are exactly what makes "rebuild everything" easy to get
/// wrong, and an operator reading this has just been told their gate is
/// invalid and wants the one command that fixes it.
pub fn stale_reason(image: &str, binary: &str, build_script: Option<&str>) -> String {
    let rebuild = match build_script {
        Some(script) => format!("bash {script}"),
        // An image outside the registry has no known script; say so rather
        // than guessing a filename (the failure mode `images.rs` exists to
        // prevent).
        None => format!("bash {} (no per-image script recorded)", super::REBUILD_ALL_SCRIPT),
    };
    format!(
        "{image} is older than {binary}, which it bakes in — this image cannot contain \
         that binary's current code, so booting it would gate nothing (#667). \
         Rebuild it: {rebuild} — or rebuild every image: bash {}",
        super::REBUILD_ALL_SCRIPT
    )
}

/// The operator-facing reason for a [`Freshness::Indeterminate`] verdict.
///
/// Pure, for the same reason as [`stale_reason`]. Says what could not be
/// established rather than implying a fault: on a host that has only ever
/// built debug binaries there is nothing wrong, there is merely nothing to
/// compare against.
pub fn indeterminate_reason(image: &str, baked: &[&str]) -> String {
    if baked.is_empty() {
        return format!(
            "{image} is not in the rootfs registry, so nothing knows which binaries it \
             bakes and its freshness cannot be established (#667)"
        );
    }
    format!(
        "cannot establish whether {image} is current: none of the binaries it bakes ({}) \
         is built in target/release, so there is nothing to compare its mtime against (#667)",
        baked.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A fixed instant to build orderings around; the absolute value is
    /// irrelevant, only the relative ordering is under test.
    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn an_image_newer_than_every_binary_is_fresh() {
        let verdict = freshness(t(1000), &[("init", Some(t(900))), ("worker", Some(t(950)))]);
        assert_eq!(verdict, Freshness::Fresh);
    }

    /// The whole point: a rebuilt binary the image predates.
    #[test]
    fn a_binary_newer_than_the_image_is_stale_and_names_itself() {
        let verdict = freshness(t(1000), &[("init", Some(t(1001)))]);
        assert_eq!(verdict, Freshness::Stale { binary: "init".to_string() });
    }

    /// One stale binary is enough, even when every other one is older —
    /// which is the realistic shape, since a guest-init change leaves each
    /// image's worker untouched.
    #[test]
    fn one_newer_binary_is_enough_even_when_the_others_are_older() {
        let verdict = freshness(t(1000), &[("worker", Some(t(10))), ("init", Some(t(1001)))]);
        assert_eq!(verdict, Freshness::Stale { binary: "init".to_string() });
    }

    /// When several have overtaken the image, report the newest: it is the
    /// one whose rebuild is furthest ahead, and reporting an arbitrary one
    /// would make the message depend on table order.
    #[test]
    fn the_newest_offender_is_the_one_reported() {
        let verdict = freshness(
            t(1000),
            &[("init", Some(t(1001))), ("worker", Some(t(2000))), ("other", Some(t(1500)))],
        );
        assert_eq!(verdict, Freshness::Stale { binary: "worker".to_string() });
    }

    /// A build compiles the binary and then writes the image, so the two can
    /// legitimately land on the same timestamp. Calling that stale would
    /// fail correct builds — the false positive that would get this check
    /// switched off.
    #[test]
    fn an_equal_mtime_is_fresh_not_stale() {
        assert_eq!(freshness(t(1000), &[("init", Some(t(1000)))]), Freshness::Fresh);
    }

    /// Absence of a reference is not evidence of freshness. If this ever
    /// returned `Fresh`, #667 would be back: a silent pass for an image
    /// nothing checked.
    #[test]
    fn no_built_binary_at_all_is_indeterminate_never_fresh() {
        let verdict = freshness(t(1000), &[("init", None), ("worker", None)]);
        assert_eq!(verdict, Freshness::Indeterminate);
    }

    /// An empty registry entry (an unknown image) must reach the same
    /// honest answer rather than passing vacuously.
    #[test]
    fn an_empty_baked_list_is_indeterminate() {
        assert_eq!(freshness(t(1000), &[]), Freshness::Indeterminate);
    }

    /// A partially-built tree still yields a real verdict from the binaries
    /// that ARE present — dropping to Indeterminate here would throw away a
    /// usable signal.
    #[test]
    fn a_missing_binary_does_not_mask_a_stale_one_that_is_present() {
        let verdict = freshness(t(1000), &[("worker", None), ("init", Some(t(1001)))]);
        assert_eq!(verdict, Freshness::Stale { binary: "init".to_string() });
    }

    #[test]
    fn a_present_older_binary_beside_a_missing_one_is_fresh() {
        let verdict = freshness(t(1000), &[("worker", None), ("init", Some(t(10)))]);
        assert_eq!(verdict, Freshness::Fresh);
    }

    #[test]
    fn the_stale_reason_names_the_image_the_binary_and_both_rebuild_routes() {
        let msg = stale_reason("kv-demo.ext4", "kastellan-microvm-init", Some("scripts/x.sh"));
        assert!(msg.contains("kv-demo.ext4"), "must name the image: {msg}");
        assert!(msg.contains("kastellan-microvm-init"), "must name the binary: {msg}");
        assert!(msg.contains("bash scripts/x.sh"), "must give the per-image command: {msg}");
        assert!(msg.contains(super::super::REBUILD_ALL_SCRIPT), "must offer rebuild-all: {msg}");
        assert!(msg.contains("#667"), "must be traceable to the issue: {msg}");
    }

    /// An unknown image must not have a build script invented for it — the
    /// hint would send the operator to a path that does not exist, which is
    /// the failure `images.rs` was written to prevent.
    #[test]
    fn the_stale_reason_invents_no_script_for_an_unregistered_image() {
        let msg = stale_reason("mystery.ext4", "some-bin", None);
        assert!(msg.contains("no per-image script recorded"), "must admit it: {msg}");
        assert!(msg.contains(super::super::REBUILD_ALL_SCRIPT), "must still offer a route: {msg}");
    }

    #[test]
    fn the_indeterminate_reason_lists_what_it_looked_for() {
        let msg = indeterminate_reason("web-fetch.ext4", &["a-bin", "b-bin"]);
        assert!(msg.contains("web-fetch.ext4"), "must name the image: {msg}");
        assert!(msg.contains("a-bin") && msg.contains("b-bin"), "must list the refs: {msg}");
        assert!(msg.contains("target/release"), "must say where it looked: {msg}");
    }

    /// The unregistered-image case has a different cause and so gets a
    /// different sentence: nothing knows what to look for, as opposed to
    /// having looked and found nothing.
    #[test]
    fn the_indeterminate_reason_distinguishes_an_unregistered_image() {
        let msg = indeterminate_reason("mystery.ext4", &[]);
        assert!(msg.contains("not in the rootfs registry"), "must name the cause: {msg}");
    }
}
