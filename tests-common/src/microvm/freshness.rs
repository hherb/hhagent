//! Does this rootfs image actually contain the code under test? (issue #667)
//!
//! # The failure this exists to stop
//!
//! `kv_demo_firecracker_persistent_e2e` once failed with
//!
//! ```text
//! persistent ext4 store must survive a VM respawn (SIGKILL + reboot); got {"value":null}
//! ```
//!
//! which reads exactly like a regression in the persistent-store path. It was
//! not: `/var/lib/kastellan/microvm/kv-demo.ext4` had been built in June, and
//! rebuilding it made the test pass unchanged.
//!
//! Every image bakes its own copy of `kastellan-microvm-init` and of its
//! worker (see [`super::images`]), so a guest-side change is invisible to the
//! Firecracker e2es until the affected image is rebuilt. That makes a stale
//! image the micro-VM twin of the stale `target/release` launcher and of the
//! `.venv` that silently belonged to another OS: **a fixture whose staleness
//! is invisible turns a gate into a formality.** The owed gate for audit item
//! W-2 could have run start to finish against stale images and reported green
//! having tested none of it.
//!
//! # Why this compares CONTENT and not mtimes
//!
//! The issue proposed comparing the image's mtime against the binary's, and
//! that was measured on the DGX before it was written. It does not survive
//! contact:
//!
//! ```text
//! target/release/kastellan-microvm-init   2026-09-05 19:08
//! kv-demo.ext4, web-fetch.ext4, matrix.ext4,
//! net-demo.ext4, web-search.ext4, web-research.ext4   2026-09-05 14:26-14:28
//! ```
//!
//! Six images five hours "stale" — and the init binary **inside every one of
//! them is byte-identical** to the one in `target/release` (verified by
//! sha256). Cargo had relinked an unchanged binary, moving its mtime without
//! changing a byte. An mtime rule would have refused six correct images on the
//! one host this check exists for, and a check that cries wolf on the common
//! case is a check somebody switches off.
//!
//! Comparing digests removes the whole class. It is also strictly stronger:
//! it catches an image built from a *stale checkout*, which the issue
//! explicitly conceded mtime could not.
//!
//! # Why the verdict is a value and not a `bool`
//!
//! Three outcomes need three different operator responses, and collapsing
//! [`Freshness::Indeterminate`] into either neighbour is what would make this
//! module dishonest. "The digest could not be read" is not evidence of
//! freshness *or* of staleness, and the one thing it must never do is render
//! identically to `Fresh`.

/// One binary baked into an image: what the image holds, and what the
/// working tree currently builds.
///
/// `None` on either side means *that* digest could not be established — the
/// binary is not built, or the image could not be read. Both are honest
/// unknowns and neither is evidence of anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BakedDigest {
    /// The `target/release/` filename, for the operator-facing message.
    pub name: String,
    /// sha256 of the copy inside the image.
    pub in_image: Option<String>,
    /// sha256 of the copy in `target/release/`.
    pub in_target: Option<String>,
}

/// The verdict for one rootfs image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    /// Every binary that could be compared matches, so the image carries the
    /// code the working tree builds.
    Fresh,
    /// A binary in the image differs from the one the working tree builds, so
    /// booting this image would test code that no longer exists.
    Stale { binary: String },
    /// Nothing could be compared.
    ///
    /// Deliberately **not** folded into [`Freshness::Fresh`]: absence of a
    /// reference is absence of evidence. The two lists say which side was
    /// missing, because the operator's next move differs — build the binary,
    /// or install `e2fsprogs`.
    Indeterminate {
        /// Binaries absent from `target/release/`.
        not_built: Vec<String>,
        /// Binaries whose in-image copy could not be read.
        unreadable_in_image: Vec<String>,
    },
}

/// Compare what an image holds against what the working tree builds.
///
/// Pure — the caller does the hashing — so every branch is unit-testable on
/// any host, including the macOS one that compiles the whole Firecracker
/// backend out.
///
/// One mismatch is enough. A guest-init change leaves each image's worker
/// untouched, so the realistic shape is exactly "one of two differs", and
/// requiring agreement from all of them would miss it.
pub fn freshness(baked: &[BakedDigest]) -> Freshness {
    if let Some(b) = baked
        .iter()
        .find(|b| matches!((&b.in_image, &b.in_target), (Some(i), Some(t)) if i != t))
    {
        return Freshness::Stale { binary: b.name.clone() };
    }
    // No mismatch. That is only meaningful if something was actually
    // compared — otherwise the run below would report a clean bill of health
    // for an image nothing looked at, which is #667 restored.
    if baked.iter().any(|b| b.in_image.is_some() && b.in_target.is_some()) {
        return Freshness::Fresh;
    }
    Freshness::Indeterminate {
        not_built: baked
            .iter()
            .filter(|b| b.in_target.is_none())
            .map(|b| b.name.clone())
            .collect(),
        unreadable_in_image: baked
            .iter()
            .filter(|b| b.in_image.is_none())
            .map(|b| b.name.clone())
            .collect(),
    }
}

/// The operator-facing reason for a [`Freshness::Stale`] verdict.
///
/// Pure so the wording is testable without touching a filesystem. Names the
/// image, the binary that differs, the script that rebuilds *this* image and
/// the script that rebuilds them all — because the two build-script
/// directories are exactly what makes "rebuild everything" easy to get wrong,
/// and an operator reading this has just been told their gate is invalid and
/// wants the one command that fixes it.
pub fn stale_reason(image: &str, binary: &str, build_script: Option<&str>) -> String {
    let rebuild = match build_script {
        Some(script) => format!("bash {script}"),
        // An image outside the registry has no known script; say so rather
        // than guessing a filename (the failure mode `images.rs` exists to
        // prevent).
        None => format!("bash {} (no per-image script recorded)", super::REBUILD_ALL_SCRIPT),
    };
    format!(
        "{image} bakes a copy of {binary} that DIFFERS from the one this tree builds, so \
         booting it would test code that no longer exists — the gate would pass having \
         verified nothing (#667). Rebuild it: {rebuild} — or rebuild every image: bash {}",
        super::REBUILD_ALL_SCRIPT
    )
}

/// The operator-facing reason for a [`Freshness::Indeterminate`] verdict.
///
/// Pure, for the same reason as [`stale_reason`]. Says what could not be
/// established rather than implying a fault: on a host that has only ever
/// built debug binaries, or one without `e2fsprogs`, there is nothing wrong —
/// there is merely nothing to compare.
pub fn indeterminate_reason(
    image: &str,
    not_built: &[String],
    unreadable_in_image: &[String],
) -> String {
    if not_built.is_empty() && unreadable_in_image.is_empty() {
        return format!(
            "{image} is not in the rootfs registry, so nothing knows which binaries it \
             bakes and its freshness cannot be established (#667)"
        );
    }
    let mut parts = Vec::new();
    if !not_built.is_empty() {
        parts.push(format!(
            "not built in target/release: {} (cargo build --release)",
            not_built.join(", ")
        ));
    }
    if !unreadable_in_image.is_empty() {
        parts.push(format!(
            "could not be read out of the image: {} (needs debugfs from e2fsprogs)",
            unreadable_in_image.join(", ")
        ));
    }
    format!(
        "cannot establish whether {image} is current, so this run is NOT gated on it — {} (#667)",
        parts.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two digests that are definitely different, without pretending to be
    /// real sha256 output: only equality is under test.
    const A: &str = "aaaa";
    const B: &str = "bbbb";

    fn d(name: &str, in_image: Option<&str>, in_target: Option<&str>) -> BakedDigest {
        BakedDigest {
            name: name.to_string(),
            in_image: in_image.map(str::to_string),
            in_target: in_target.map(str::to_string),
        }
    }

    #[test]
    fn matching_digests_are_fresh() {
        let verdict = freshness(&[d("init", Some(A), Some(A)), d("worker", Some(B), Some(B))]);
        assert_eq!(verdict, Freshness::Fresh);
    }

    /// The whole point: the image holds something other than what we build.
    #[test]
    fn a_differing_digest_is_stale_and_names_the_binary() {
        let verdict = freshness(&[d("init", Some(A), Some(B))]);
        assert_eq!(verdict, Freshness::Stale { binary: "init".to_string() });
    }

    /// One mismatch is enough. A guest-init change leaves each image's
    /// worker untouched, so this is the realistic shape, not a corner case.
    #[test]
    fn one_differing_binary_is_enough_when_the_other_matches() {
        let verdict = freshness(&[d("worker", Some(A), Some(A)), d("init", Some(A), Some(B))]);
        assert_eq!(verdict, Freshness::Stale { binary: "init".to_string() });
    }

    /// The regression that motivated dropping mtimes: cargo relinks an
    /// unchanged binary and moves its mtime. Identical content is FRESH
    /// however the timestamps sit, and six DGX images depended on this.
    #[test]
    fn identical_content_is_fresh_regardless_of_any_timestamp() {
        // No timestamp is even an input here — that is the assertion.
        assert_eq!(freshness(&[d("init", Some(A), Some(A))]), Freshness::Fresh);
    }

    /// Absence of a reference is not evidence of freshness. If this ever
    /// returned `Fresh`, #667 would be back: a clean bill of health for an
    /// image nothing looked at.
    #[test]
    fn nothing_comparable_is_indeterminate_never_fresh() {
        let verdict = freshness(&[d("init", None, None)]);
        assert_eq!(
            verdict,
            Freshness::Indeterminate {
                not_built: vec!["init".to_string()],
                unreadable_in_image: vec!["init".to_string()],
            }
        );
    }

    /// A half-known pair is still nothing comparable, and the verdict must
    /// say WHICH half was missing — the operator's next move differs.
    #[test]
    fn a_missing_target_binary_alone_is_indeterminate_and_says_so() {
        let verdict = freshness(&[d("init", Some(A), None)]);
        assert_eq!(
            verdict,
            Freshness::Indeterminate {
                not_built: vec!["init".to_string()],
                unreadable_in_image: vec![],
            }
        );
    }

    #[test]
    fn an_unreadable_image_copy_alone_is_indeterminate_and_says_so() {
        let verdict = freshness(&[d("init", None, Some(A))]);
        assert_eq!(
            verdict,
            Freshness::Indeterminate {
                not_built: vec![],
                unreadable_in_image: vec!["init".to_string()],
            }
        );
    }

    /// An empty registry entry (an unknown image) must reach the same honest
    /// answer rather than passing vacuously.
    #[test]
    fn an_empty_baked_list_is_indeterminate() {
        assert_eq!(
            freshness(&[]),
            Freshness::Indeterminate { not_built: vec![], unreadable_in_image: vec![] }
        );
    }

    /// A partially-comparable set still yields a real verdict from the pair
    /// that IS comparable — dropping to Indeterminate would throw away a
    /// usable signal.
    #[test]
    fn an_uncomparable_binary_does_not_mask_a_comparable_stale_one() {
        let verdict = freshness(&[d("worker", None, None), d("init", Some(A), Some(B))]);
        assert_eq!(verdict, Freshness::Stale { binary: "init".to_string() });
    }

    #[test]
    fn an_uncomparable_binary_beside_a_matching_one_is_fresh() {
        let verdict = freshness(&[d("worker", None, None), d("init", Some(A), Some(A))]);
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

    /// The two causes need two different remedies, so they must not render
    /// as one generic "could not check".
    #[test]
    fn the_indeterminate_reason_separates_the_two_causes() {
        let unbuilt = indeterminate_reason("web-fetch.ext4", &["a-bin".to_string()], &[]);
        assert!(unbuilt.contains("cargo build --release"), "must say how to fix: {unbuilt}");
        assert!(!unbuilt.contains("e2fsprogs"), "must not blame the wrong thing: {unbuilt}");

        let unreadable = indeterminate_reason("web-fetch.ext4", &[], &["a-bin".to_string()]);
        assert!(unreadable.contains("e2fsprogs"), "must name the missing tool: {unreadable}");
        assert!(!unreadable.contains("cargo build"), "must not blame the wrong thing: {unreadable}");
    }

    /// It must say the run is ungated. An operator who reads only the first
    /// clause should still learn that this check did not apply.
    #[test]
    fn the_indeterminate_reason_says_the_run_is_not_gated() {
        let msg = indeterminate_reason("web-fetch.ext4", &["a-bin".to_string()], &[]);
        assert!(msg.contains("NOT gated"), "the caveat is the point: {msg}");
    }

    /// The unregistered-image case has a different cause and so gets a
    /// different sentence: nothing knows what to look for, as opposed to
    /// having looked and found nothing.
    #[test]
    fn the_indeterminate_reason_distinguishes_an_unregistered_image() {
        let msg = indeterminate_reason("mystery.ext4", &[], &[]);
        assert!(msg.contains("not in the rootfs registry"), "must name the cause: {msg}");
    }
}
