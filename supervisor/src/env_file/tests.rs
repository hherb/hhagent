use super::*;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Tempdir helper mirroring `launchd_agents::tests::TestRoot`:
/// unique per process+test+call, removed on drop.
struct TestRoot(PathBuf);
impl TestRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kastellan-envfile-test-{}-{}-{}",
            std::process::id(),
            label,
            n
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test root");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn file(&self, name: &str, contents: &str) -> PathBuf {
        let p = self.0.join(name);
        fs::write(&p, contents).expect("write fixture");
        p
    }
}
impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter().map(|(k, val)| (k.to_string(), val.to_string())).collect()
}

#[test]
fn parse_env_file_skips_comments_blanks_and_keeps_embedded_equals() {
    let parsed = parse_env_file("# header\n\nFOO=bar\n  BAZ =qux=zap\nnokey\n");
    assert_eq!(
        parsed,
        pairs(&[
            ("FOO", "bar"),
            // key trimmed; value taken after the FIRST '=' (so an embedded '=',
            // e.g. a URL query, is preserved). Lines without '=' ("nokey") and
            // '#' comments are skipped.
            ("BAZ", "qux=zap"),
        ])
    );
}

/// The fixture below was run through a live systemd user manager on the DGX
/// (2026-08-09) via `systemd-run --user -p EnvironmentFile=… --pipe /usr/bin/env`.
/// The expectations are that manager's actual output, not a reading of the man
/// page. Before #528 five of these seven diverged, so one hand-written overlay
/// produced two different runtime environments on Linux and macOS — the launchd
/// backend folds these pairs into the plist, while systemd parses the file
/// itself.
#[test]
fn parse_env_file_matches_systemds_measured_grammar() {
    let parsed = parse_env_file(concat!(
        "A=\"a b\"\n",
        "B=  c  \n",
        "C='d e'\n",
        "D=plain\n",
        "E=f\"g\n",
        "export F=h\n",
        ";G=i\n",
    ));
    assert_eq!(
        parsed,
        pairs(&[
            ("A", "a b"), // surrounding double quotes stripped
            ("B", "c"),   // value whitespace trimmed
            ("C", "d e"), // surrounding single quotes stripped
            ("D", "plain"),
            ("E", "f\"g"), // an UNMATCHED quote is literal
                           // `export F=h` and `;G=i` are dropped entirely, exactly as systemd
                           // drops them — they must not become keys named `export F` and `;G`.
        ])
    );
}

#[test]
fn parse_env_file_leaves_a_json_value_untouched() {
    // KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA is JSON, is operator-added, and is one
    // of the keys #458 keeps losing — so it travels this path on every macOS
    // install. It neither starts nor ends with a quote, so nothing is stripped.
    let parsed = parse_env_file("KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA={\"10.0.0.3\":\"/c.pem\"}\n");
    assert_eq!(parsed, pairs(&[("KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA", "{\"10.0.0.3\":\"/c.pem\"}")]));
}

/// Second DGX measurement (same day, same method), covering the cases the first
/// fixture did not reach. It overturned the obvious "strip a matched pair" rule:
/// a quote only matters as the value's FIRST character, where systemd enters a
/// quoted state and leaves it at the matching close *or at end-of-line*.
#[test]
fn parse_env_file_strips_a_leading_quote_even_when_unterminated() {
    // Measured: systemd yields `a`, not `"a`.
    assert_eq!(parse_env_file("A=\"a\n"), pairs(&[("A", "a")]));
    // Measured: the trailing `'` does not close a `"`, so it stays in the value.
    assert_eq!(parse_env_file("A=\"a'\n"), pairs(&[("A", "a'")]));
    // Measured: a bare pair collapses to empty.
    assert_eq!(parse_env_file("A=\"\"\n"), pairs(&[("A", "")]));
    assert_eq!(parse_env_file("A=\n"), pairs(&[("A", "")]));
    // Measured: quotes are stripped but the whitespace INSIDE them survives —
    // so the value trim must happen outside the quotes, not inside.
    assert_eq!(parse_env_file("A='  x  '\n"), pairs(&[("A", "  x  ")]));
}

#[test]
fn merge_env_file_values_override_inline_env_keeping_position() {
    let mut env = pairs(&[("A", "1"), ("B", "2")]);
    merge_env(&mut env, pairs(&[("B", "override"), ("C", "3")]));
    assert_eq!(
        env,
        pairs(&[
            ("A", "1"),
            ("B", "override"), // overridden in place
            ("C", "3"),        // new key appended
        ])
    );
}

#[test]
fn merge_env_takes_the_last_occurrence_of_a_key_repeated_in_one_batch() {
    // systemd takes the last, and `diff_env_files` was already fixed to agree.
    // The overlay is the file a human APPENDS to over time, so a repeated key is
    // a realistic shape — and iterating `from` in reverse would survive every
    // other test in this file.
    let mut env = Vec::new();
    merge_env(&mut env, pairs(&[("A", "first"), ("B", "b"), ("A", "last")]));
    assert_eq!(env, pairs(&[("A", "last"), ("B", "b")]));
}

// ---------- fold_env_files: the ordering + optionality contract ----------

#[test]
fn fold_env_files_applies_files_in_order_with_the_later_one_winning() {
    // This is the whole mechanism of #458: the generated file first, the
    // operator's overlay second. Reversing the list here would silently restore
    // the bug on macOS, where these pairs are baked into the plist.
    let root = TestRoot::new("order");
    let generated = root.file("kastellan.env", "MODEL=stock-tag\nDATA=/d\n");
    let overlay = root.file("kastellan.env.local", "MODEL=tuned-tag\n");

    let mut env = pairs(&[("MODEL", "inline-from-spec")]);
    fold_env_files(
        &mut env,
        &[
            EnvFileRef { path: generated, optional: false },
            EnvFileRef { path: overlay, optional: true },
        ],
    )
    .expect("fold");

    assert_eq!(env, pairs(&[("MODEL", "tuned-tag"), ("DATA", "/d")]));
}

#[test]
fn fold_env_files_skips_a_missing_optional_file() {
    // The normal state of the overlay: it does not exist, and that is not an
    // error on either platform.
    let root = TestRoot::new("opt-missing");
    let generated = root.file("kastellan.env", "A=1\n");

    let mut env = Vec::new();
    fold_env_files(
        &mut env,
        &[
            EnvFileRef { path: generated, optional: false },
            EnvFileRef { path: root.path().join("kastellan.env.local"), optional: true },
        ],
    )
    .expect("a missing optional file is not an error");

    assert_eq!(env, pairs(&[("A", "1")]));
}

#[test]
fn fold_env_files_errors_on_a_missing_required_file() {
    let root = TestRoot::new("req-missing");
    let err = fold_env_files(
        &mut Vec::new(),
        &[EnvFileRef { path: root.path().join("kastellan.env"), optional: false }],
    )
    .expect_err("a missing REQUIRED file must fail the install");
    assert!(format!("{err:?}").contains("read environment_file"), "{err:?}");
}

#[test]
fn fold_env_files_errors_on_an_unreadable_optional_file() {
    // `optional` forgives ABSENCE, not unreadability. Treating a file the
    // operator wrote but we cannot decode as an empty one is #458 wearing a new
    // hat: they believe the overlay applies, and it silently does not.
    let root = TestRoot::new("opt-unreadable");
    let overlay = root.path().join("kastellan.env.local");
    fs::write(&overlay, [b'A', b'=', 0xff, 0xfe, b'\n']).expect("write fixture");

    let err = fold_env_files(&mut Vec::new(), &[EnvFileRef { path: overlay, optional: true }])
        .expect_err("an unreadable overlay must not be mistaken for an absent one");
    assert!(format!("{err:?}").contains("read environment_file"), "{err:?}");
}

#[test]
fn validate_env_file_path_requires_an_absolute_path() {
    assert!(validate_env_file_path(Path::new("/home/u/.config/kastellan/kastellan.env")).is_ok());
    let err = validate_env_file_path(Path::new("kastellan.env"))
        .expect_err("systemd drops a relative EnvironmentFile= and starts with no environment");
    assert!(format!("{err:?}").contains("must be absolute"), "{err:?}");
}

#[test]
fn validate_env_file_path_rejects_control_characters() {
    // #530 moved this guarantee here. `systemd_user::builder` now emits
    // `EnvironmentFile=` bare, because a quoted path is one systemd drops
    // (measured) — and a bare path containing a newline would end the
    // directive and inject the next line as another one:
    //
    //   EnvironmentFile=/tmp/x
    //   ExecStartPre=/evil
    //
    // Quoting used to make that case fail *safe* (systemd dropped the quoted
    // path), so removing it moves the burden onto validation. Both backends
    // call this before any rendering, so a path can only reach a renderer
    // after control characters have been refused.
    let err = validate_env_file_path(Path::new("/tmp/x\nExecStartPre=/evil"))
        .expect_err("a newline in the path would inject a unit directive");
    assert!(format!("{err:?}").contains("control character"), "{err:?}");

    // A space is NOT a control character and must stay legal: `$HOME` can
    // contain one, systemd accepts it bare, and launchd handles it natively.
    // Rejecting it would refuse an install that works.
    validate_env_file_path(Path::new("/home/first last/.config/kastellan/kastellan.env"))
        .expect("a path with spaces is legitimate on both platforms");
}

// ---------- operator-overlay observability (#531) ----------

#[test]
fn an_absent_overlay_is_reported_as_absent_not_as_empty() {
    // The whole point of #531: before it, "absent because I don't want one"
    // and "absent because I typo'd the path" produced the same silence. The
    // renderer must name the path it looked at, so an operator who wrote
    // `~/.config/kastellan.env.local` (missing directory component) can see
    // that the installer looked somewhere else.
    let dir = TestRoot::new("absent");
    let path = dir.path().join("kastellan.env.local");

    let state = inspect_overlay(&path);
    assert_eq!(state, OverlayState::Absent);

    let line = render_overlay_found(&path, &state);
    assert!(line.contains(&path.display().to_string()), "must name the path: {line}");
    assert!(line.contains("none"), "{line}");
}

#[test]
fn a_present_overlay_reports_its_key_count_and_never_its_values() {
    // Key names are operator-facing diagnostics; values are endpoints, token
    // FILE paths and model tags — the install transcript and the daemon log
    // are both plaintext with none of `audit_log`'s role gating, so the
    // convention (stated in the threat model's "User data in the daemon log")
    // is names only. This test is the enforcement.
    let dir = TestRoot::new("present");
    let path = dir.file(
        "kastellan.env.local",
        "KASTELLAN_LLM_TIMEOUT_MS=180000\nKASTELLAN_MAIL_TOKEN_FILE=/home/u/.config/kastellan/mail-token\n",
    );

    let state = inspect_overlay(&path);
    assert_eq!(state, OverlayState::Present { keys: 2 });

    let line = render_overlay_found(&path, &state);
    assert!(line.contains("2 keys"), "{line}");
    assert!(!line.contains("180000"), "a value leaked into the transcript: {line}");
    assert!(!line.contains("mail-token"), "a value leaked into the transcript: {line}");
}

#[test]
fn an_unreadable_overlay_is_distinguished_from_an_absent_one() {
    // A file that exists but cannot be read is the operator's worst case: they
    // wrote it and believe it applies. `fold_env_files` already refuses to
    // treat it as empty (that would be #458 wearing a new hat); the report must
    // not flatten it into "none" either.
    let dir = TestRoot::new("unreadable");
    let path = dir.path().join("subdir"); // a directory reads as an Io error, portably
    fs::create_dir_all(&path).expect("mkdir");

    let state = inspect_overlay(&path);
    assert!(matches!(state, OverlayState::Unreadable { .. }), "{state:?}");
    let line = render_overlay_found(&path, &state);
    assert!(line.to_lowercase().contains("unreadable"), "{line}");
}

#[test]
fn every_overlay_key_present_in_the_environment_counts_as_applied() {
    let overlay = vec![
        ("KASTELLAN_LLM_TIMEOUT_MS".to_string(), "180000".to_string()),
        ("KASTELLAN_MAIL_ENDPOINT".to_string(), "https://10.0.0.3:8443".to_string()),
    ];
    let live = |k: &str| match k {
        "KASTELLAN_LLM_TIMEOUT_MS" => Some("180000".to_string()),
        "KASTELLAN_MAIL_ENDPOINT" => Some("https://10.0.0.3:8443".to_string()),
        _ => None,
    };
    assert!(unapplied_keys(&overlay, live).is_empty());
}

#[test]
fn a_key_missing_or_overridden_in_the_environment_is_named() {
    // The two live failure shapes this exists to catch. MISSING is the #530
    // case (systemd dropped a quoted `EnvironmentFile=` and started anyway)
    // and the typo case. DIFFERENT is the ordering case — the overlay listed
    // BEFORE the generated file instead of after, so `kastellan.env` wins and
    // the operator's tuning is silently overridden, which is #458 exactly.
    let overlay = vec![
        ("APPLIED".to_string(), "same".to_string()),
        ("MISSING".to_string(), "x".to_string()),
        ("OVERRIDDEN".to_string(), "wanted".to_string()),
    ];
    let live = |k: &str| match k {
        "APPLIED" => Some("same".to_string()),
        "OVERRIDDEN" => Some("something-else".to_string()),
        _ => None,
    };
    assert_eq!(unapplied_keys(&overlay, live), vec!["MISSING", "OVERRIDDEN"]);
}

#[test]
fn a_repeated_key_is_judged_on_its_last_value_and_named_once() {
    // Within one file a repeated key resolves to its LAST occurrence — the
    // rule `merge_env` and `diff_env_files` already follow, and the natural
    // shape of an overlay an operator appended a correction to. Judging the
    // first value would report a key as unapplied precisely when the
    // correction DID apply.
    let overlay = vec![
        ("K".to_string(), "superseded".to_string()),
        ("K".to_string(), "current".to_string()),
    ];
    assert!(unapplied_keys(&overlay, |_| Some("current".to_string())).is_empty());
    assert_eq!(unapplied_keys(&overlay, |_| Some("superseded".to_string())), vec!["K"]);
}

#[test]
fn the_applied_report_names_unapplied_keys_but_never_values() {
    let path = PathBuf::from("/home/u/.config/kastellan/kastellan.env.local");

    let all_ok = render_overlay_applied(&path, 5, &[]);
    assert!(all_ok.contains("5"), "{all_ok}");
    assert!(all_ok.contains("applied"), "{all_ok}");

    let partial = render_overlay_applied(&path, 5, &["KASTELLAN_MAIL_ENDPOINT".to_string()]);
    assert!(partial.contains("KASTELLAN_MAIL_ENDPOINT"), "{partial}");
    assert!(partial.contains("1 of 5"), "{partial}");
    // The distinction must survive a skim: an operator scanning the log has to
    // be able to tell these two lines apart without counting.
    assert_ne!(all_ok, partial);
}

#[test]
fn a_repeated_key_counts_once_everywhere_it_is_counted() {
    // The numerator and the denominator must mean the same thing. `unapplied_keys`
    // resolves a repeated key to one entry, so a count taken from the raw parsed
    // pairs would render "1 of 3 keys did not reach this process" against a file
    // declaring TWO — an operator who appended a correction reads a number that
    // does not match their file and has no way to tell which count is wrong.
    //
    // Asserted through `inspect_overlay` against a real file, NOT by recomputing
    // `declared_keys(...).len()` here: the first cut of this test did the latter
    // and passed happily with `inspect_overlay` still counting raw pairs, since
    // it never called the function whose behaviour it is named for.
    let dir = TestRoot::new("dupkeys");
    let path = dir.file("kastellan.env.local", "A=superseded\nB=b\nA=current\n");

    assert_eq!(parse_env_file("A=x\nB=b\nA=y\n").len(), 3, "the parser reports raw pairs, by design");
    assert_eq!(
        inspect_overlay(&path),
        OverlayState::Present { keys: 2 },
        "three lines, two declared keys"
    );

    // And the count agrees with what the applied-check will report against it.
    let pairs = parse_env_file(&fs::read_to_string(&path).expect("read back"));
    assert_eq!(unapplied_keys(&pairs, |_| None).len(), 2);
}
