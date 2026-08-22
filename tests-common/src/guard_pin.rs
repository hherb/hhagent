//! Structural guards for the Shieldstral guard-weights pin (issue #592).
//!
//! The sha256 of the guard model is written down **twice** — once in
//! `core/src/cassandra/guard_model/weights_pin.rs`, which is what
//! `guard calibrate` checks automatically, and once in
//! `scripts/eval/lib/guard-weights.sh`, which is the operator's
//! pre-flight on a host where the tool is not built.
//!
//! The duplication is forced: `kastellan-core` is published to
//! crates.io and cannot `include_str!` a path outside its own
//! directory. That is the same constraint
//! `kastellan-sandbox::guest_kernel_pin` lives under, and this module is
//! the same answer — the sibling of
//! [`crate::microvm`]'s `rust_and_bash_kernel_pins_agree`.
//!
//! It lives in `tests-common` for one concrete reason: **CI runs
//! `cargo test -p kastellan-tests-common` on every PR.** A pin that
//! drifted between the two files would otherwise only surface on an
//! occasional operator run — and the drift arrives in exactly the PR
//! that changes one of them, which is the case a per-PR gate must
//! catch.

/// The operator-facing bash half of the guard-weights pin.
pub const GUARD_WEIGHTS_LIB: &str = "scripts/eval/lib/guard-weights.sh";

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The repository root, derived from this crate's manifest dir.
    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("tests-common has a workspace parent")
            .to_path_buf()
    }

    /// Pull `NAME="value"` out of the shared bash pin.
    ///
    /// Deliberately strict about the shape: if the assignment is
    /// reformatted, this panics rather than quietly matching nothing
    /// and letting the comparison pass vacuously — a test that can
    /// silently stop testing is worse than no test. Same reasoning, and
    /// same wording, as `microvm::tests::bash_pin_value`.
    ///
    /// Lives inside `mod tests` (as its `microvm` sibling does) because
    /// only tests use it: at module level it is `dead_code` in a
    /// non-test build, which `-D warnings` rejects.
    fn bash_pin_value(body: &str, name: &str) -> String {
        let prefix = format!("{name}=\"");
        let line = body
            .lines()
            .find(|l| l.starts_with(&prefix))
            .unwrap_or_else(|| panic!("{GUARD_WEIGHTS_LIB} has no line starting `{prefix}`"));
        line[prefix.len()..]
            .strip_suffix('"')
            .unwrap_or_else(|| panic!("malformed assignment in {GUARD_WEIGHTS_LIB}: {line}"))
            .to_string()
    }

    /// The Rust check and the bash pre-flight must agree, or one of
    /// them is verifying against a stale sum — which is #592's own
    /// failure mode one level up: a pin that everybody believes and
    /// nothing reconciles.
    #[test]
    fn rust_and_bash_guard_pins_agree() {
        let body = std::fs::read_to_string(repo_root().join(GUARD_WEIGHTS_LIB))
            .unwrap_or_else(|e| panic!("read {GUARD_WEIGHTS_LIB}: {e}"));

        assert_eq!(
            bash_pin_value(&body, "KASTELLAN_GUARD_WEIGHTS_SHA256"),
            kastellan_core::cassandra::guard_model::weights_pin::PINNED_SHA256,
            "the guard-weights sha256 drifted between {GUARD_WEIGHTS_LIB} and \
             core/src/cassandra/guard_model/weights_pin.rs -- bump both together"
        );
        assert_eq!(
            bash_pin_value(&body, "KASTELLAN_GUARD_WEIGHTS_SIZE_BYTES")
                .parse::<u64>()
                .expect("size pin is a number"),
            kastellan_core::cassandra::guard_model::weights_pin::PINNED_SIZE_BYTES,
            "the guard-weights size drifted between {GUARD_WEIGHTS_LIB} and \
             core/src/cassandra/guard_model/weights_pin.rs -- bump both together"
        );
    }

    /// The bash half must actually *check*, not merely record.
    ///
    /// A file that carried the sum and no comparison would satisfy the
    /// agreement test above while verifying nothing — the shape of
    /// defect this repo has paid for repeatedly (a claim in a document
    /// that the code stopped satisfying). So the pre-flight's own
    /// contract is asserted here too.
    #[test]
    fn the_bash_pin_verifies_rather_than_only_recording() {
        let body = std::fs::read_to_string(repo_root().join(GUARD_WEIGHTS_LIB))
            .unwrap_or_else(|e| panic!("read {GUARD_WEIGHTS_LIB}: {e}"));

        assert!(
            body.contains("require_guard_weights()"),
            "{GUARD_WEIGHTS_LIB} must define require_guard_weights"
        );
        assert!(
            body.contains("\"$actual\" = \"$KASTELLAN_GUARD_WEIGHTS_SHA256\""),
            "{GUARD_WEIGHTS_LIB} records the sum but never compares against it"
        );
        // Both hosts are first-class, and the two coreutils spell this
        // differently -- a Linux-only `sha256sum` would make the macOS
        // pre-flight fail for the wrong reason.
        assert!(
            body.contains("sha256sum") && body.contains("shasum"),
            "{GUARD_WEIGHTS_LIB} must handle both GNU and BSD coreutils"
        );
        // Verify-only: this must never fetch or repair, the same rule
        // `require_guest_kernel` follows.
        assert!(
            !body.contains("curl") && !body.contains("wget"),
            "{GUARD_WEIGHTS_LIB} must VERIFY only -- never fetch"
        );
    }
}
