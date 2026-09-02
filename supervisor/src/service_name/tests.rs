//! The service-name rule set, tested once for both backends.
//!
//! These cases were character-identical in
//! `systemd_user/builder/tests.rs` and `launchd_agents/builders/tests.rs`
//! before [#642]. Each ran on one platform only, so neither host ever
//! executed the other's copy and a divergence between them would have
//! been invisible to both. Here they run everywhere.
//!
//! [#642]: https://github.com/hherb/kastellan/issues/642

use super::{validate_service_name, MAX_NAME_LEN};
use crate::SupervisorError;

/// Every name the tree actually installs under must pass. The last two
/// are the shapes `tests-common` builds — a `kastellan-supervisor-test-`
/// prefix plus a `unique_suffix()` of digits and dashes.
#[test]
fn accepts_the_names_the_tree_installs() {
    for n in &[
        "kastellan",
        "kastellan-core",
        "kastellan.core",
        // launchd's reverse-DNS label convention. It was in that
        // backend's copy of this test and not in systemd's, which is
        // the one place the two lists actually differed — the rules
        // were identical, the coverage was not.
        "org.kastellan.core",
        "a_b",
        "abc123",
        "kastellan-supervisor-test-core-gboot-726614-1756758000000000000-3",
        "kastellan-supervisor-test-pg-l3pyrun-726614-1756758000000000000-0",
    ] {
        validate_service_name(n).expect(n);
    }
}

#[test]
fn rejects_empty() {
    let err = validate_service_name("").expect_err("empty must reject");
    assert!(matches!(err, SupervisorError::InvalidName(_)));
}

/// The half that matters most: a name reaches the filesystem as a
/// basename, so a separator or a NUL is a path-traversal primitive
/// rather than a cosmetic complaint.
#[test]
fn rejects_path_traversal() {
    for n in &["../evil", "a/b", "foo\\bar", "..", ".", "a\0b"] {
        let err = validate_service_name(n).expect_err(n);
        assert!(matches!(err, SupervisorError::InvalidName(_)), "{n}: {err}");
    }
}

#[test]
fn rejects_dot_prefix_and_dash_prefix() {
    for n in &[".hidden", "-flagish"] {
        let err = validate_service_name(n).expect_err(n);
        assert!(matches!(err, SupervisorError::InvalidName(_)), "{n}: {err}");
    }
}

/// The charset half — the one the `tests-common` hand-copy this issue
/// deleted could not see, and the one a real test label can actually
/// trip (a space or a `/` in a label sails past a length check and dies
/// later at `install`).
#[test]
fn rejects_whitespace_and_specials() {
    for n in &["has space", "has\ttab", "has;semi", "has*star", "has\nnl"] {
        let err = validate_service_name(n).expect_err(n);
        assert!(matches!(err, SupervisorError::InvalidName(_)), "{n}: {err}");
    }
}

/// **Both directions of the cap, deliberately.**
///
/// The two backend copies only ever asserted the reject side
/// (`MAX_NAME_LEN + 1`), which leaves the boundary itself unpinned: a
/// `>` mutated to `>=` rejects a legal 200-char name and passes every
/// one of those tests. Asserting at `MAX_NAME_LEN` *and* at
/// `MAX_NAME_LEN + 1` is what makes the comparison operator observable.
#[test]
fn the_cap_admits_its_own_length_and_refuses_one_more() {
    let at_cap = "a".repeat(MAX_NAME_LEN);
    validate_service_name(&at_cap).expect("a name of exactly MAX_NAME_LEN must be legal");

    let over_cap = "a".repeat(MAX_NAME_LEN + 1);
    let err = validate_service_name(&over_cap).expect_err("one over the cap must reject");
    assert!(matches!(err, SupervisorError::InvalidName(_)));
}

/// Pins the cap to a **literal**, not to itself.
///
/// `assert_eq!(MAX_NAME_LEN, MAX_NAME_LEN)` puts the constant on both
/// sides and passes at any value — the failure shape that cost #633 a
/// round. 200 is the number both backends carried privately and the
/// number `tests-common` hand-copied; changing it is a decision, and
/// this test is where that decision has to be made explicitly.
#[test]
fn the_cap_is_two_hundred() {
    assert_eq!(MAX_NAME_LEN, 200);
}

/// A non-ASCII name is refused, and refused by the **charset** rule.
///
/// Worth asserting which rule fires, not merely that one does: the cap
/// counts bytes while a reader counts characters, and if non-ASCII were
/// ever admitted, that gap would become a real off-by-N against the
/// filesystem's basename limit. Today it cannot, because ASCII is all
/// this gate accepts — and this is what says so.
#[test]
fn a_multibyte_name_is_refused_by_the_charset_gate() {
    let err = validate_service_name("kästellan").expect_err("non-ASCII must reject");
    match err {
        SupervisorError::InvalidName(msg) => {
            assert!(
                msg.contains("illegal character"),
                "expected the charset rule to fire, got: {msg}"
            );
        }
        other => panic!("expected InvalidName, got: {other}"),
    }
}
