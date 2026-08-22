//! Unit tests for the guard-weights pin.
//!
//! Lifted to a sibling file rather than left inline: `weights_pin/mod.rs`
//! plus these tests exceed this repo's single-file cap, and the rule is
//! to split *before* the change that grows a file past it, so the
//! movement stays reviewable on its own. Counts are deliberately not
//! quoted here — they rot on every edit, and the split decision does not
//! depend on them.

use super::*;
use std::io::Write;

/// sha256 of the empty input, from the standard test vectors.
const SHA256_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// sha256 of the 5 bytes `hello`, from the standard test vectors.
const SHA256_HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
/// The DGX's unverified build, the file that started #592. Kept as a
/// literal so the incident is a regression test, not a memory.
const DGX_ORIGINAL_SHA256: &str =
    "5cee57a981fefa688ba91825a0a9933d238d4b9147476275b3eac0afbeaf40f5";

fn write_file(dir: &std::path::Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).expect("create fixture");
    f.write_all(bytes).expect("write fixture");
    path
}

/// A digest for fixtures, through the checked constructor.
fn digest(sha256: &str, size_bytes: u64) -> FileDigest {
    FileDigest::from_hex(sha256, size_bytes).expect("fixture hash is 64 lowercase hex")
}

// ---------------- model_path_from_props ----------------

/// Abridged verbatim from the DGX's live llama-server, 2026-08-22.
fn real_props() -> serde_json::Value {
    serde_json::json!({
        "total_slots": 4,
        "model_alias": "shieldstral",
        "model_ftype": "Q8_0",
        "model_path": "/home/hherb/models/shieldstral/upstream/Shieldstral-1.0-3B-Q8_0.gguf",
        "modalities": {"vision": false, "video": false, "audio": false}
    })
}

#[test]
fn model_path_is_read_from_a_real_props_body() {
    assert_eq!(
        model_path_from_props(&real_props()),
        Some("/home/hherb/models/shieldstral/upstream/Shieldstral-1.0-3B-Q8_0.gguf")
    );
}

#[test]
fn no_model_path_when_the_key_is_absent() {
    let props = serde_json::json!({"model_alias": "shieldstral"});
    assert_eq!(model_path_from_props(&props), None);
}

#[test]
fn no_model_path_when_the_value_is_not_a_usable_string() {
    // A server reporting the field as any non-string has not told us
    // a path. Coercing would invent one.
    //
    // `""` is in this table for a reason found in review: it IS a
    // string, so it used to survive, and `PathBuf::from("")` opens as
    // ENOENT -- which would report `Unreadable` and advise the operator
    // to "run the calibration on the host serving the model", sending
    // them to another machine over a server that simply said nothing.
    for bad in [
        serde_json::json!(null),
        serde_json::json!(7),
        serde_json::json!(true),
        serde_json::json!(["/a/b"]),
        serde_json::json!({"path": "/a/b"}),
        serde_json::json!(""),
    ] {
        let props = serde_json::json!({"model_path": bad});
        assert_eq!(model_path_from_props(&props), None, "coerced {props}");
    }
}

#[test]
fn no_model_path_when_the_body_is_not_an_object() {
    for bad in [
        serde_json::json!("model_path"),
        serde_json::json!([{"model_path": "/a/b"}]),
        serde_json::json!(null),
    ] {
        assert_eq!(model_path_from_props(&bad), None, "accepted {bad}");
    }
}

// ---------------- FileDigest ----------------

#[test]
fn from_hex_accepts_a_well_formed_sum() {
    let d = FileDigest::from_hex(SHA256_HELLO, 5).expect("well formed");
    assert_eq!(d.sha256(), SHA256_HELLO);
    assert_eq!(d.size_bytes(), 5);
}

/// The invariant that lets [`hash_matches`] be a plain `==`, and that
/// makes the "fabricate a digest for a file we never opened" shortcut
/// -- the one review found in the CLI's opt-out path -- impossible to
/// write rather than merely discouraged.
#[test]
fn from_hex_rejects_everything_that_is_not_64_lowercase_hex() {
    let too_long = format!("{SHA256_HELLO}a");
    let upper = SHA256_HELLO.to_uppercase();
    for bad in [
        "",
        "abc",
        &SHA256_HELLO[..63], // too short
        too_long.as_str(),
        upper.as_str(),
        "2CF24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824", // mixed case
        "<unverified: props-unreachable>", // the shortcut this invariant exists to forbid
        "zzz24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824", // non-hex
    ] {
        assert!(FileDigest::from_hex(bad, 0).is_none(), "accepted {bad:?}");
    }
}

// ---------------- hash_matches ----------------

#[test]
fn hash_matches_accepts_a_matching_hash() {
    assert!(hash_matches(PINNED_SHA256, PINNED_SHA256));
}

#[test]
fn hash_matches_rejects_a_different_hash() {
    assert!(!hash_matches(SHA256_HELLO, PINNED_SHA256));
}

/// The incident itself, as a regression test: the DGX's build is a
/// valid, working, correctly-labelled Q8_0 that is not the pinned file.
#[test]
fn hash_matches_rejects_the_dgx_build_that_started_issue_592() {
    assert!(!hash_matches(DGX_ORIGINAL_SHA256, PINNED_SHA256));
    assert_ne!(DGX_ORIGINAL_SHA256, PINNED_SHA256, "the fixture must be a real second build");
}

#[test]
fn hash_matches_is_case_sensitive() {
    // Not a tolerance question: `FileDigest` cannot hold an uppercase
    // sum at all (`from_hex_rejects_everything_that_is_not_64_lowercase_hex`),
    // so this pins that the comparison does not quietly re-introduce a
    // second spelling for one hash.
    assert!(!hash_matches(&PINNED_SHA256.to_uppercase(), PINNED_SHA256));
}

#[test]
fn pinned_sha256_is_64_lowercase_hex() {
    assert!(
        FileDigest::from_hex(PINNED_SHA256, PINNED_SIZE_BYTES).is_some(),
        "PINNED_SHA256 is not 64 lowercase hex: {PINNED_SHA256}"
    );
}

// ---------------- digest_file ----------------

#[test]
fn digest_file_matches_the_standard_vectors() {
    let dir = tempfile::tempdir().expect("tempdir");

    let empty = write_file(dir.path(), "empty", b"");
    let d = digest_file(&empty).expect("digest empty");
    assert_eq!(d.sha256(), SHA256_EMPTY);
    assert_eq!(d.size_bytes(), 0);

    let hello = write_file(dir.path(), "hello", b"hello");
    let d = digest_file(&hello).expect("digest hello");
    assert_eq!(d.sha256(), SHA256_HELLO);
    assert_eq!(d.size_bytes(), 5);
}

/// The streaming loop must accumulate across reads, not hash the last
/// chunk. Sized to cross the boundary three times and leave a short
/// final read, so both the loop and the tail are exercised.
#[test]
fn digest_file_accumulates_across_chunk_boundaries() {
    let dir = tempfile::tempdir().expect("tempdir");
    let n = HASH_CHUNK_BYTES * 3 + 12345;
    let body: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
    let path = write_file(dir.path(), "big", &body);

    let d = digest_file(&path).expect("digest big");
    assert_eq!(d.size_bytes(), n as u64);

    let mut oneshot = Sha256::new();
    oneshot.update(&body);
    let expected: String = oneshot.finalize().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(d.sha256(), expected, "streamed hash must equal the one-shot hash");
}

#[test]
fn digest_file_errors_on_a_missing_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = digest_file(&dir.path().join("absent")).expect_err("must error");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

// ---------------- verify_weights_against ----------------

#[test]
fn verify_returns_the_digest_when_the_file_matches() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_file(dir.path(), "w.gguf", b"hello");
    let d = verify_weights_against(&path, SHA256_HELLO).expect("verify");
    assert_eq!(d.sha256(), SHA256_HELLO);
    assert_eq!(d.size_bytes(), 5);
}

#[test]
fn verify_reports_mismatch_with_the_digest_when_the_file_differs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_file(dir.path(), "w.gguf", b"hello");
    match verify_weights_against(&path, PINNED_SHA256) {
        Err(WeightsPinError::Mismatch { path: p, actual }) => {
            assert_eq!(p, path);
            assert_eq!(actual.sha256(), SHA256_HELLO);
            assert_eq!(actual.size_bytes(), 5);
        }
        other => panic!("expected Mismatch, got {other:?}"),
    }
}

#[test]
fn verify_reports_unreadable_rather_than_mismatch_for_a_missing_file() {
    // The distinction is load-bearing: "we could not look" and "we
    // looked and it was wrong" call for different operator actions,
    // and only the second is an incident.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("absent.gguf");
    match verify_weights_against(&path, PINNED_SHA256) {
        Err(WeightsPinError::Unreadable(p, _)) => assert_eq!(p, path),
        other => panic!("expected Unreadable, got {other:?}"),
    }
}

/// A relative `model_path` is refused, never resolved.
///
/// This is a **fail-open** if it is resolved, not merely a diagnosis
/// problem: a relative path is interpreted against this process's cwd,
/// so a copy of the pinned file at the same relative path under the
/// CLI's working directory would hash as pinned while the server served
/// entirely different bytes -- #592's own shape, arrived at through the
/// fix for #592.
#[test]
fn verify_refuses_a_relative_path_rather_than_resolving_it_against_our_cwd() {
    // The path names nothing that exists, which is what makes this also
    // pin the ORDER: were the absolute check to run after the open, a
    // non-existent relative path would come back `Unreadable`. Getting
    // `RelativePath` proves the refusal precedes the read, without
    // needing a `set_current_dir` that would race the other tests.
    let rel = PathBuf::from("models/Shieldstral-1.0-3B-Q8_0.gguf");
    match verify_weights_against(&rel, PINNED_SHA256) {
        Err(WeightsPinError::RelativePath(p)) => assert_eq!(p, rel),
        other => panic!("expected RelativePath, got {other:?}"),
    }
}

#[test]
fn verify_weights_at_uses_the_in_repo_pin() {
    // Pins the wiring of the thin wrapper: a fixture that is not the
    // pinned file must come back as a Mismatch through it.
    //
    // Limit, stated rather than implied: this catches a wrapper wired
    // to the fixture's own hash, and nothing more -- swapping
    // PINNED_SHA256 for any other 64-hex constant is invisible here,
    // because the only file small enough to keep in the tree is by
    // construction not the 3.6 GB pinned one.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_file(dir.path(), "w.gguf", b"hello");
    match verify_weights_at(&path) {
        Err(WeightsPinError::Mismatch { actual, .. }) => {
            assert_eq!(actual.sha256(), SHA256_HELLO)
        }
        other => panic!("expected Mismatch, got {other:?}"),
    }
}

// ---------------- report rendering ----------------

/// The pinned header must report what was **measured**, not recite the
/// constant back.
///
/// The first version rendered `PINNED_SHA256` unconditionally, so the
/// assertion `contains(PINNED_SHA256)` could not fail -- and a reader
/// could not tell a computed hash from one quoted out of the binary's
/// `.rodata`, which is the exact distinction this field exists to
/// carry. The fixture below therefore uses a hash that is NOT the pin.
#[test]
fn the_pinned_header_line_names_the_hash_it_measured() {
    let line = WeightsProvenance::Pinned {
        path: PathBuf::from("/m/w.gguf"),
        digest: digest(SHA256_HELLO, 5),
    }
    .header_line();
    assert!(line.contains(SHA256_HELLO), "must name the measured hash: {line}");
    assert!(!line.contains(PINNED_SHA256), "must not recite the constant: {line}");
    assert!(line.contains("pinned"), "{line}");
    assert!(line.contains("/m/w.gguf"), "must name the file it hashed: {line}");
}

/// The stamp has to carry the *consequence*, not just the hash --
/// the reader most likely to see it is the one about to copy this
/// report's tau into production.
#[test]
fn the_unpinned_header_line_states_the_consequence() {
    let line = WeightsProvenance::Unpinned {
        path: PathBuf::from("/m/w.gguf"),
        digest: digest(DGX_ORIGINAL_SHA256, 42),
    }
    .header_line();
    assert!(line.contains(DGX_ORIGINAL_SHA256), "{line}");
    assert!(line.contains("/m/w.gguf"), "must name the file it hashed: {line}");
    assert!(line.contains(PINNED_SHA256), "must name what was expected: {line}");
    assert!(line.contains("UNPINNED"), "{line}");
    assert!(line.contains("CANNOT"), "must state the consequence: {line}");
}

/// The third state: nothing was hashed at all.
///
/// It must not borrow the shape of a measurement. The version review
/// caught rendered `<unverified: props-unreachable> (0 bytes)` -- a
/// byte count for a file that was never opened, in the same field
/// position a real streamed count occupies.
#[test]
fn the_unverified_header_line_reports_no_measurement() {
    let line = WeightsProvenance::Unverified { kind: "props-unreachable" }.header_line();
    assert!(line.contains("<unverified: props-unreachable>"), "{line}");
    assert!(line.contains("nothing was hashed"), "must say no hash was taken: {line}");
    assert!(!line.contains("bytes"), "must not report a byte count it never measured: {line}");
    assert!(line.contains("CANNOT"), "must state the consequence: {line}");
}

/// One grep must find every untrustworthy run. `UNPINNED` is that
/// token, so both non-pinned variants carry it and the pinned one does
/// not -- otherwise an operator filtering their reports would see a
/// `/props`-unreachable run as clean.
#[test]
fn exactly_the_untrustworthy_variants_carry_the_unpinned_token() {
    let pinned = WeightsProvenance::Pinned {
        path: PathBuf::from("/m/w.gguf"),
        digest: digest(PINNED_SHA256, PINNED_SIZE_BYTES),
    };
    assert!(!pinned.header_line().contains("UNPINNED"), "{}", pinned.header_line());
    for bad in [
        WeightsProvenance::Unpinned {
            path: PathBuf::from("/m/w.gguf"),
            digest: digest(DGX_ORIGINAL_SHA256, 42),
        },
        WeightsProvenance::Unverified { kind: "no-model-path" },
    ] {
        assert!(bad.header_line().contains("UNPINNED"), "{}", bad.header_line());
    }
}

// ---------------- error rendering ----------------

/// Every refusal, as a list.
///
/// **What the exhaustive `match` below does and does not buy.** It is
/// exhaustive over `WeightsPinError`, so adding a variant is a compile
/// error *here* -- which is the prompt to add it to this list. It does
/// not, and cannot, force the new variant into the returned `Vec`:
/// someone who satisfies the compiler by widening an arm still gets a
/// silently smaller list. Rust has no reflection over variants, so the
/// claim stops there rather than being overstated, which is the failure
/// this list already had once (an earlier version enumerated three of
/// four and omitted `Mismatch` -- both the likeliest refusal and the one
/// the opt-out exists for; the CLI e2e caught it, not the unit test).
fn all_refusals() -> Vec<WeightsPinError> {
    let cases = vec![
        WeightsPinError::PropsUnavailable("connection refused".to_string()),
        WeightsPinError::NoModelPath,
        WeightsPinError::RelativePath(PathBuf::from("models/w.gguf")),
        WeightsPinError::Unreadable(
            PathBuf::from("/m/w.gguf"),
            std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        ),
        WeightsPinError::Mismatch {
            path: PathBuf::from("/m/w.gguf"),
            actual: FileDigest::from_hex(SHA256_HELLO, 5).expect("valid"),
        },
    ];
    for e in &cases {
        match e {
            WeightsPinError::PropsUnavailable(_)
            | WeightsPinError::NoModelPath
            | WeightsPinError::RelativePath(_)
            | WeightsPinError::Unreadable(..)
            | WeightsPinError::Mismatch { .. } => {}
        }
    }
    cases
}

/// Every refusal names the escape hatch, because every one of them is
/// fatal without it and an operator who does not know it exists cannot
/// calibrate a candidate model at all.
#[test]
fn every_refusal_names_the_opt_out() {
    for e in all_refusals() {
        let msg = e.to_string();
        assert!(msg.contains("--weights-unpinned"), "{e:?} does not name the opt-out: {msg}");
    }
}

/// A same-size mismatch is a different quantiser run; a
/// different-size mismatch is the wrong file. #592 turned on exactly
/// that distinction, so the message must make it.
#[test]
fn a_same_size_mismatch_is_named_as_a_different_quantiser_run() {
    let e = WeightsPinError::Mismatch {
        path: PathBuf::from("/m/w.gguf"),
        actual: digest(DGX_ORIGINAL_SHA256, PINNED_SIZE_BYTES),
    };
    let msg = e.to_string();
    assert!(msg.contains("DIFFERENT QUANTISER RUN"), "{msg}");
    assert!(msg.contains(DGX_ORIGINAL_SHA256), "must name the actual hash: {msg}");
    assert!(msg.contains(PINNED_SHA256), "must name the expected hash: {msg}");
}

#[test]
fn a_different_size_mismatch_is_named_as_a_different_file() {
    let e = WeightsPinError::Mismatch {
        path: PathBuf::from("/m/w.gguf"),
        actual: digest(SHA256_HELLO, 5),
    };
    let msg = e.to_string();
    assert!(msg.contains("different file"), "{msg}");
    assert!(!msg.contains("DIFFERENT QUANTISER RUN"), "{msg}");
}

/// The mismatch message tells an operator which two files to edit when
/// the model was changed on purpose. Both paths must exist, or the
/// instruction sends them to `$EDITOR` to create an empty new file --
/// which is what the first version did, naming `weights_pin.rs` after
/// the module had been split into a directory.
#[test]
fn the_mismatch_message_names_files_that_exist() {
    let e = WeightsPinError::Mismatch {
        path: PathBuf::from("/m/w.gguf"),
        actual: digest(SHA256_HELLO, 5),
    };
    let msg = e.to_string();
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("core has a workspace parent")
        .to_path_buf();
    for named in
        ["core/src/cassandra/guard_model/weights_pin/mod.rs", "scripts/eval/lib/guard-weights.sh"]
    {
        assert!(msg.contains(named), "must name {named}: {msg}");
        assert!(repo_root.join(named).is_file(), "the message names a missing file: {named}");
    }
}

/// The relative-path refusal has to explain the *fail-open*, not just
/// report a rule -- an operator told only "paths must be absolute"
/// will assume pedantry and reach for the opt-out.
#[test]
fn the_relative_path_refusal_explains_why_resolving_would_be_unsafe() {
    let e = WeightsPinError::RelativePath(PathBuf::from("models/w.gguf"));
    let msg = e.to_string();
    assert!(msg.contains("models/w.gguf"), "must name the path: {msg}");
    assert!(msg.contains("working directory"), "must explain against what it would resolve: {msg}");
    assert!(msg.contains("absolute -m"), "must say how to fix the server: {msg}");
}

// ---------------- kind() ----------------

/// `kind` exists so a caller can name a refusal in ONE LINE.
///
/// The first version of the CLI's opt-out path interpolated the whole
/// `Display` message into the report's `weights:` header — and every
/// variant of that message is multi-line and paragraph-length, so an
/// unverified run would have rendered its header as several lines of
/// prose wearing a field label. Short, stable, whitespace-free.
///
/// Driven off [`all_refusals`] so it inherits the same compile-time
/// prompt; the earlier version hand-enumerated its own list and had no
/// guard at all.
#[test]
fn kind_is_a_short_single_token_for_every_variant() {
    let cases = all_refusals();
    let mut seen = std::collections::BTreeSet::new();
    for e in &cases {
        let k = e.kind();
        assert!(!k.is_empty(), "{e:?} has an empty kind");
        assert!(
            !k.chars().any(char::is_whitespace),
            "{e:?} kind {k:?} contains whitespace -- it must fit one header field"
        );
        assert!(k.len() <= 24, "{e:?} kind {k:?} is too long for a header field");
        assert!(seen.insert(k), "{e:?} kind {k:?} collides with another variant");
    }
}
