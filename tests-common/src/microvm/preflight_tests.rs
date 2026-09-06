//! Preflight tests for the micro-VM e2e guards: the launcher probe order,
//! the operator-facing hint wording, and #667's image-freshness enforcement.
//!
//! Split out of `mod.rs` in the same change that added the freshness gate —
//! the file was over the 500-line cap with the tests inline. Test names and
//! assertions are unchanged by the move.

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

// ---------------------------------------------------------------
// #667: the freshness verdict must reach the operator as an ACTION,
// not merely as a correctly-worded string.
//
// Every test below drives `skip_if_image_stale_to` / a rendered
// message rather than asserting on `stale_reason` alone. That
// distinction is the whole point: a wording-only assertion leaves the
// `panic!` deletable with the suite still green, which is exactly the
// mutation that would silently restore #667.
// ---------------------------------------------------------------

/// An `Indeterminate` verdict with a realistic cause, so the rendered caveat
/// is the one an operator would actually see.
fn indeterminate() -> Freshness {
    Freshness::Indeterminate {
        not_built: vec![GUEST_INIT_BIN.to_string()],
        unreadable_in_image: vec![],
    }
}

/// Hold the env lock and force [`REQUIRE_ENV`] to a known state, so
/// these tests cannot race the ambient value on an operator's host.
fn with_require<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
    let _lock = crate::env_lock();
    let _guard = match value {
        Some(v) => crate::EnvVarGuard::set(REQUIRE_ENV, v),
        None => crate::EnvVarGuard::unset(REQUIRE_ENV),
    };
    f()
}

#[test]
fn a_fresh_image_does_not_skip_and_says_nothing() {
    let mut out = Vec::new();
    let skipped = with_require(None, || {
        skip_if_image_stale_to("kv-demo.ext4", Freshness::Fresh, &mut out)
    });
    assert!(!skipped, "a fresh image must run");
    assert!(out.is_empty(), "a fresh image must be silent, got {:?}", String::from_utf8(out));
}

/// The load-bearing branch. A stale image must STOP the run, not skip
/// it: a `[SKIP]` here would be #667 wearing a different hat, since a
/// skip reports green.
#[test]
#[should_panic(expected = "DIFFERS")]
fn a_stale_image_panics_rather_than_skipping() {
    let mut out = Vec::new();
    with_require(None, || {
        skip_if_image_stale_to(
            "kv-demo.ext4",
            Freshness::Stale { binary: GUEST_INIT_BIN.to_string() },
            &mut out,
        )
    });
}

/// ...and it must panic even with the REQUIRE knob explicitly OFF.
/// Staleness is not an unmet precondition an operator may reasonably
/// lack; it is positive evidence that the run would prove nothing.
#[test]
#[should_panic(expected = "#667")]
fn a_stale_image_panics_even_when_require_is_off() {
    let mut out = Vec::new();
    with_require(Some("0"), || {
        skip_if_image_stale_to(
            "kv-demo.ext4",
            Freshness::Stale { binary: GUEST_INIT_BIN.to_string() },
            &mut out,
        )
    });
}

/// The panic must carry the rebuild command. An operator who has just
/// been told their gate is invalid wants the one line that fixes it,
/// and the two build-script directories are what makes assembling it
/// by hand error-prone.
#[test]
fn the_stale_panic_names_the_rebuild_command() {
    let panicked = std::panic::catch_unwind(|| {
        let mut sink = Vec::new();
        skip_if_image_stale_to(
            "kv-demo.ext4",
            Freshness::Stale { binary: GUEST_INIT_BIN.to_string() },
            &mut sink,
        )
    })
    .unwrap_err();
    let msg = panicked
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_else(|| "<non-string panic>".to_string());
    assert!(msg.contains("build-kv-demo-rootfs.sh"), "must name this image's script: {msg}");
    assert!(msg.contains(REBUILD_ALL_SCRIPT), "must offer the rebuild-all route: {msg}");
}

/// Indeterminate must RUN, not skip: absence of a reference binary is
/// not evidence of staleness, and downgrading a real VM run to a skip
/// would lose coverage for nothing.
#[test]
fn an_indeterminate_verdict_warns_but_still_runs() {
    let mut out = Vec::new();
    let skipped = with_require(None, || {
        skip_if_image_stale_to("kv-demo.ext4", indeterminate(), &mut out)
    });
    assert!(!skipped, "must still run — the image may be perfectly current");
    let rendered = String::from_utf8(out).expect("utf8");
    assert!(rendered.contains("[WARN]"), "the caveat must be visible: {rendered}");
    assert!(
        !rendered.contains("[SKIP]"),
        "must NOT inflate the skip count for a test that ran: {rendered}"
    );
}

/// ...but an operator demanding a fully-gated run gets a failure
/// instead. This is the half that lets CI insist the check applied.
#[test]
#[should_panic(expected = "KASTELLAN_MICROVM_REQUIRE_E2E")]
fn an_indeterminate_verdict_fails_under_require() {
    let mut out = Vec::new();
    with_require(Some("1"), || {
        skip_if_image_stale_to("kv-demo.ext4", indeterminate(), &mut out)
    });
}

/// The knob reads the one project flag dialect, not a strict
/// `Some("1")` — the operator-facing skew #654 was filed about. A
/// `kastellan.env` saying `=true` must demand, not silently skip.
#[test]
fn the_require_knob_speaks_the_project_flag_dialect() {
    use crate::gliner_e2e::UnmetAction;
    for truthy in ["1", "true", "TRUE", "yes", "on", "  true  "] {
        assert_eq!(
            with_require(Some(truthy), require_action),
            UnmetAction::Fail,
            "{truthy:?} must demand a real run"
        );
    }
    for falsey in ["0", "false", "no", "off", ""] {
        assert_eq!(
            with_require(Some(falsey), require_action),
            UnmetAction::Skip,
            "{falsey:?} must stay an opt-out"
        );
    }
    assert_eq!(with_require(None, require_action), UnmetAction::Skip, "unset is the default");
}

/// An unmet precondition still skips-as-passes by default, which is
/// what keeps a plain `cargo test` green on a host with no KVM. The
/// line must actually be EMITTED, not merely renderable.
#[test]
fn an_unmet_precondition_emits_a_skip_line_by_default() {
    let mut out = Vec::new();
    let skipped =
        with_require(None, || report_unmet_microvm_to("no /dev/kvm on this host", &mut out));
    assert!(skipped, "the caller must return");
    let rendered = String::from_utf8(out).expect("utf8");
    assert!(rendered.contains("[SKIP]"), "must be greppable: {rendered}");
    assert!(rendered.contains("no /dev/kvm"), "must carry the reason: {rendered}");
}

/// The documented false green this knob exists for: `firecracker` off
/// the non-interactive ssh PATH made the whole suite skip-as-pass.
#[test]
#[should_panic(expected = "KASTELLAN_MICROVM_REQUIRE_E2E")]
fn an_unmet_precondition_fails_under_require() {
    let mut out = Vec::new();
    with_require(Some("1"), || report_unmet_microvm_to("firecracker not on PATH", &mut out));
}

/// The rebuild-all script every staleness message points at must
/// exist, for the same reason `every_build_script_exists` pins the
/// per-image ones: a hint naming a nonexistent path is worse than no
/// hint, and this one is quoted on the failure path an operator only
/// ever reads when something is already wrong.
#[test]
fn the_rebuild_all_script_exists_and_is_executable() {
    let path = repo_root().join(REBUILD_ALL_SCRIPT);
    assert!(path.is_file(), "missing {REBUILD_ALL_SCRIPT}: {}", path.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert!(mode & 0o111 != 0, "{REBUILD_ALL_SCRIPT} is not executable (mode {mode:o})");
    }
}

/// The rebuild-all script must actually drive every image in the
/// registry. Without this it could silently cover seven of eight —
/// and the eighth is exactly the kv-demo image whose staleness filed
/// #667 in the first place.
#[test]
fn the_rebuild_all_script_covers_every_registered_image() {
    let body = std::fs::read_to_string(repo_root().join(REBUILD_ALL_SCRIPT))
        .unwrap_or_else(|e| panic!("read {REBUILD_ALL_SCRIPT}: {e}"));
    for entry in crate::microvm::images::ROOTFS_IMAGES {
        assert!(
            body.contains(entry.build_script),
            "{REBUILD_ALL_SCRIPT} never runs {} (for {})",
            entry.build_script,
            entry.image
        );
    }
}
