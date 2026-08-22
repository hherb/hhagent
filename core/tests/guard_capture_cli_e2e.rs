//! `kastellan-cli guard capture`'s refusals, end to end, offline.
//!
//! **What this file exists to pin is the EXIT STATUS and the ORDERING**,
//! the same two things `guard_calibrate_cli_e2e` pins for the sibling
//! command — and for the same reason its module doc gives: before that
//! file existed, deleting the branch that turns "this run is not
//! believable" into a non-zero exit passed every test in the tree.
//!
//! `guard capture` reopened exactly that hole one command along. Its
//! unit tests cover `sha256_hex`, `is_injection_placeholder`,
//! `allowlist_host` and `write_case`; none of them calls `run`. Deleting
//! the `return ExitCode::FAILURE` on the pre-fetch refusal, or any of
//! the three `failures += 1`, or the `if failures > 0` block, passed
//! `cargo test --workspace`.
//!
//! **Every case here is offline by construction.** The pre-fetch
//! refusal returns before the first fetch, and the `--record` leg points
//! at a `.invalid` host, which is reserved by RFC 2606 and never
//! resolves — so the fetch fails fast without touching a real origin.
//!
//! Skips cleanly if the CLI binary hasn't been built.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::Path;
use std::process::Command;

use kastellan_tests_common::cli_binary;

/// One manifest entry, written to `dir`.
fn write_entry(dir: &Path, id: &str, sha256: Option<&str>) {
    let sha = match sha256 {
        Some(h) => format!(r#", "sha256": "{h}""#),
        None => String::new(),
    };
    std::fs::write(
        dir.join(format!("{id}.json")),
        format!(
            r#"{{
  "id": "{id}",
  "label": "benign",
  "provenance": "captured",
  "source": "https://guard-capture-e2e.invalid/{id}"{sha}
}}"#
        ),
    )
    .expect("write manifest entry");
}

/// Run `guard capture`, returning `(exit code, stdout, stderr)`.
fn capture(manifest: &Path, out: &Path, record: bool) -> Option<(i32, String, String)> {
    let bin = cli_binary();
    if !bin.is_file() {
        eprintln!("\n[SKIP] kastellan-cli not built; run `cargo build -p kastellan-core`\n");
        return None;
    }
    let mut cmd = Command::new(bin);
    cmd.args(["guard", "capture", "--manifest"])
        .arg(manifest)
        .arg("--out")
        .arg(out);
    if record {
        cmd.arg("--record");
    }
    let o = cmd.output().expect("run kastellan-cli");
    Some((
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    ))
}

/// THE ORDERING CLAIM, executed.
///
/// The source comment says the whole-manifest check runs "before any
/// fetch … so a partial materialisation cannot leave the out dir in a
/// state that looks complete". Both halves rest purely on
/// `create_dir_all` sitting *after* the refusal block; hoisting it
/// changed no test. The out dir path here does not exist, so its absence
/// afterwards is the ordering, observed.
#[test]
fn an_unrecorded_entry_refuses_before_any_fetch_and_never_creates_the_out_dir() {
    let d = tempfile::tempdir().expect("tempdir");
    let manifest = d.path().join("manifest");
    std::fs::create_dir(&manifest).expect("mkdir");
    write_entry(&manifest, "cap-001-recorded", Some(&"a".repeat(64)));
    write_entry(&manifest, "cap-002-unrecorded", None);
    let out = d.path().join("out");

    let Some((code, _stdout, stderr)) = capture(&manifest, &out, false) else {
        return;
    };
    assert_eq!(code, 1, "an unverifiable entry must not exit 0\n{stderr}");
    assert!(
        stderr.contains("cap-002-unrecorded") && stderr.contains("no sha256 recorded"),
        "the refusal must name the entry and the cause\n{stderr}"
    );
    assert!(
        stderr.contains("nothing fetched"),
        "it must say no network round trip was spent\n{stderr}"
    );
    assert!(
        !out.exists(),
        "the out dir must never be created when the manifest cannot be verified"
    );
}

/// The mirror leg: `--record` is what an unrecorded entry is FOR, so it
/// must get past the pre-check.
///
/// Without this, `if !record` could be inverted -- refusing in record
/// mode and skipping in verify mode, the fail-open direction -- and the
/// test above would still pass. The run still exits non-zero because the
/// `.invalid` host cannot be fetched; what it proves is that execution
/// reached the fetch at all, which the created out dir witnesses.
#[test]
fn record_mode_gets_past_the_pre_check_and_reaches_the_fetch() {
    let d = tempfile::tempdir().expect("tempdir");
    let manifest = d.path().join("manifest");
    std::fs::create_dir(&manifest).expect("mkdir");
    write_entry(&manifest, "cap-002-unrecorded", None);
    let out = d.path().join("out");

    let Some((code, _stdout, stderr)) = capture(&manifest, &out, true) else {
        return;
    };
    assert!(
        !stderr.contains("nothing fetched"),
        "--record must not refuse an entry that has no hash yet\n{stderr}"
    );
    assert!(
        out.exists(),
        "the out dir is created once the manifest passes, which is how we know \
         the pre-check did not refuse\n{stderr}"
    );
    // The .invalid host cannot resolve, so the fetch fails and the run
    // must say so rather than reporting success over zero captures.
    assert_eq!(code, 1, "a failed fetch must not exit 0\n{stderr}");
    assert!(
        stderr.contains("cap-002-unrecorded"),
        "the failure must name the entry\n{stderr}"
    );
}

/// A malformed hash is a manifest bug, and it is refused with its own
/// cause rather than being compared and reported as a drifted source.
#[test]
fn a_malformed_hash_refuses_with_the_manifest_cause_not_a_drift() {
    let d = tempfile::tempdir().expect("tempdir");
    let manifest = d.path().join("manifest");
    std::fs::create_dir(&manifest).expect("mkdir");
    write_entry(&manifest, "cap-003-malformed", Some("abc123"));
    let out = d.path().join("out");

    let Some((code, _stdout, stderr)) = capture(&manifest, &out, false) else {
        return;
    };
    assert_eq!(code, 1);
    assert!(
        stderr.contains("not 64 hex characters"),
        "must name the shape problem\n{stderr}"
    );
    // The refusal's own text says "do not treat this as a drifted
    // source", so the word appears -- what must NOT appear is the drift
    // refusal's phrasing, which is what sends an operator hunting a
    // source that did not move.
    assert!(
        !stderr.contains("The source has drifted"),
        "a malformed manifest entry is not a drifted source, and reporting it as \
         one sends the operator after the wrong thing\n{stderr}"
    );
    assert!(!out.exists());
}

/// Usage errors exit 2, distinct from a refusal's 1.
#[test]
fn usage_errors_exit_two() {
    let bin = cli_binary();
    if !bin.is_file() {
        eprintln!("\n[SKIP] kastellan-cli not built; run `cargo build -p kastellan-core`\n");
        return;
    }
    let d = tempfile::tempdir().expect("tempdir");
    for args in [
        vec!["guard", "capture"],
        vec!["guard", "capture", "--manifest"],
        vec!["guard", "capture", "--manifest", "/nonexistent"],
        vec!["guard", "capture", "--out"],
        vec!["guard", "capture", "--wat"],
    ] {
        let o = Command::new(&bin)
            .args(&args)
            .current_dir(d.path())
            .output()
            .expect("run kastellan-cli");
        assert_eq!(
            o.status.code(),
            Some(2),
            "{args:?} is a usage error, not a refusal: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
}
