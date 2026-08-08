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

#[test]
fn parse_env_file_strips_only_a_matched_pair() {
    // An unterminated quote is not a pair, and a quote of the other kind at the
    // far end is not a pair either.
    assert_eq!(parse_env_file("A=\"a\n"), pairs(&[("A", "\"a")]));
    assert_eq!(parse_env_file("A=\"a'\n"), pairs(&[("A", "\"a'")]));
    // A bare pair of quotes IS a pair, and yields the empty value.
    assert_eq!(parse_env_file("A=\"\"\n"), pairs(&[("A", "")]));
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
