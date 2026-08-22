//! Unit tests for the guard-weights pin.
//!
//! Lifted to a sibling file rather than left inline: `weights_pin.rs`
//! reached 619 lines, and this repo's rule is to split *before* the
//! change that grows a file past the cap, so the movement stays
//! reviewable on its own. Production is ~310 lines, tests ~300.

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
fn no_model_path_when_the_value_is_not_a_string() {
    // A server reporting the field as any non-string has not told us
    // a path. Coercing would invent one.
    for bad in [
        serde_json::json!(null),
        serde_json::json!(7),
        serde_json::json!(true),
        serde_json::json!(["/a/b"]),
        serde_json::json!({"path": "/a/b"}),
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

// ---------------- classify ----------------

#[test]
fn classify_accepts_a_matching_hash() {
    assert_eq!(classify(PINNED_SHA256, PINNED_SHA256), WeightsVerdict::Pinned);
}

#[test]
fn classify_rejects_a_different_hash_and_names_it() {
    assert_eq!(
        classify(SHA256_HELLO, PINNED_SHA256),
        WeightsVerdict::Unpinned { actual: SHA256_HELLO.to_string() }
    );
}

/// The regression this module exists for: the DGX's original build
/// is the right model, the right quantisation and the right size,
/// and must still be refused.
#[test]
fn classify_rejects_the_dgx_build_that_started_issue_592() {
    assert_eq!(
        classify(DGX_ORIGINAL_SHA256, PINNED_SHA256),
        WeightsVerdict::Unpinned { actual: DGX_ORIGINAL_SHA256.to_string() }
    );
}

#[test]
fn classify_is_case_sensitive() {
    // Documents the strictness rather than merely inheriting it:
    // `digest_file` emits lowercase and the pin's casing is asserted
    // below, so an uppercase hash reaching here means something
    // upstream is not what this module assumes.
    let upper = PINNED_SHA256.to_ascii_uppercase();
    assert_eq!(
        classify(&upper, PINNED_SHA256),
        WeightsVerdict::Unpinned { actual: upper }
    );
}

#[test]
fn pinned_sha256_is_64_lowercase_hex() {
    assert_eq!(PINNED_SHA256.len(), 64, "not a sha256");
    assert!(
        PINNED_SHA256.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "PINNED_SHA256 must be lowercase hex -- `classify` compares case-sensitively"
    );
}

// ---------------- digest_file ----------------

#[test]
fn digest_file_matches_the_standard_vectors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let empty = digest_file(&write_file(dir.path(), "empty", b"")).expect("hash empty");
    assert_eq!(empty.sha256, SHA256_EMPTY);
    assert_eq!(empty.size_bytes, 0);

    let hello = digest_file(&write_file(dir.path(), "hello", b"hello")).expect("hash hello");
    assert_eq!(hello.sha256, SHA256_HELLO);
    assert_eq!(hello.size_bytes, 5);
}

/// The weights are ~3.6 GB and the reader is chunked, so the loop
/// must accumulate across reads. An implementation that hashed only
/// the first chunk would pass every small-file test above.
///
/// The expectation is computed one-shot over the same bytes: that is
/// not circular, it is exactly the property under test.
#[test]
fn digest_file_accumulates_across_chunk_boundaries() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Deliberately not a multiple of the chunk size, so the final
    // short read is exercised too.
    let len = HASH_CHUNK_BYTES * 3 + 12345;
    let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    let got = digest_file(&write_file(dir.path(), "big", &bytes)).expect("hash big");

    let want: String =
        Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(got.sha256, want, "chunked hash disagrees with one-shot");
    assert_eq!(got.size_bytes, len as u64, "byte count lost across chunks");
}

#[test]
fn digest_file_errors_on_a_missing_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = digest_file(&dir.path().join("nope")).expect_err("must not invent a hash");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

// ---------------- verify_weights_against ----------------

#[test]
fn verify_reports_pinned_when_the_file_matches() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_file(dir.path(), "w.gguf", b"hello");
    assert_eq!(
        verify_weights_against(&path, SHA256_HELLO).expect("verify"),
        WeightsProvenance::Pinned
    );
}

#[test]
fn verify_reports_unpinned_with_the_digest_when_the_file_differs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_file(dir.path(), "w.gguf", b"hello");
    match verify_weights_against(&path, PINNED_SHA256).expect("verify") {
        WeightsProvenance::Unpinned { digest } => {
            assert_eq!(digest.sha256, SHA256_HELLO);
            assert_eq!(digest.size_bytes, 5);
        }
        other => panic!("expected Unpinned, got {other:?}"),
    }
}

#[test]
fn verify_reports_unreadable_rather_than_unpinned_for_a_missing_file() {
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

#[test]
fn verify_weights_at_uses_the_in_repo_pin() {
    // Pins the wiring of the thin wrapper: a fixture that is not the
    // pinned file must come back Unpinned through it.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_file(dir.path(), "w.gguf", b"hello");
    match verify_weights_at(&path).expect("verify") {
        WeightsProvenance::Unpinned { digest } => assert_eq!(digest.sha256, SHA256_HELLO),
        other => panic!("expected Unpinned, got {other:?}"),
    }
}

// ---------------- report rendering ----------------

#[test]
fn the_pinned_header_line_names_the_hash() {
    let line = WeightsProvenance::Pinned.header_line();
    assert!(line.contains(PINNED_SHA256), "{line}");
    assert!(line.contains("pinned"), "{line}");
}

/// The stamp has to carry the *consequence*, not just the hash --
/// the reader most likely to see it is the one about to copy this
/// report's tau into production.
#[test]
fn the_unpinned_header_line_states_the_consequence() {
    let line = WeightsProvenance::Unpinned {
        digest: FileDigest { sha256: DGX_ORIGINAL_SHA256.to_string(), size_bytes: 42 },
    }
    .header_line();
    assert!(line.contains(DGX_ORIGINAL_SHA256), "{line}");
    assert!(line.contains("UNPINNED"), "{line}");
    assert!(line.contains("CANNOT"), "must state the consequence: {line}");
}

// ---------------- error rendering ----------------

/// A same-size mismatch is a different quantiser run; a
/// different-size mismatch is the wrong file. #592 turned on exactly
/// that distinction, so the message must make it.
#[test]
fn a_same_size_mismatch_is_named_as_a_different_quantiser_run() {
    let msg = WeightsPinError::Mismatch {
        path: PathBuf::from("/m/w.gguf"),
        actual: FileDigest {
            sha256: DGX_ORIGINAL_SHA256.to_string(),
            size_bytes: PINNED_SIZE_BYTES,
        },
    }
    .to_string();
    assert!(msg.contains("DIFFERENT QUANTISER RUN"), "{msg}");
    assert!(msg.contains(PINNED_SHA256), "{msg}");
    assert!(msg.contains(DGX_ORIGINAL_SHA256), "{msg}");
}

#[test]
fn a_different_size_mismatch_is_named_as_a_different_file() {
    let msg = WeightsPinError::Mismatch {
        path: PathBuf::from("/m/w.gguf"),
        actual: FileDigest { sha256: SHA256_HELLO.to_string(), size_bytes: 5 },
    }
    .to_string();
    assert!(msg.contains("different file altogether"), "{msg}");
    assert!(!msg.contains("DIFFERENT QUANTISER RUN"), "{msg}");
}

/// EVERY refusal must name the opt-out, or an operator hits a wall
/// with no way forward and no idea one exists.
///
/// The first version of this test enumerated three variants and
/// omitted `Mismatch` — which is both the most likely refusal and
/// the one the opt-out exists *for*, since calibrating a candidate
/// guard model produces exactly it. The CLI e2e caught the gap.
///
/// **The generalisable form: a hand-enumerated test proves nothing
/// about the case it forgot.** So the list below is guarded by an
/// exhaustive `match`: a new variant that is not added here is a
/// compile error, not a silently smaller test.
#[test]
fn every_refusal_names_the_opt_out() {
    let cases: Vec<WeightsPinError> = vec![
        WeightsPinError::PropsUnavailable("connection refused".to_string()),
        WeightsPinError::NoModelPath,
        WeightsPinError::Unreadable(
            PathBuf::from("/m/w.gguf"),
            std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        ),
        WeightsPinError::Mismatch {
            path: PathBuf::from("/m/w.gguf"),
            actual: FileDigest { sha256: SHA256_HELLO.to_string(), size_bytes: 5 },
        },
    ];
    for e in &cases {
        match e {
            WeightsPinError::PropsUnavailable(_)
            | WeightsPinError::NoModelPath
            | WeightsPinError::Unreadable(..)
            | WeightsPinError::Mismatch { .. } => {}
        }
    }
    for e in cases {
        let msg = e.to_string();
        assert!(msg.contains("--weights-unpinned"), "{e:?} does not name the opt-out: {msg}");
    }
}

// ---------------- kind() ----------------

/// `kind` exists so a caller can name a refusal in ONE LINE.
///
/// The first version of the CLI's opt-out path interpolated the whole
/// `Display` message into the report's `weights:` header — and every
/// variant of that message is multi-line and paragraph-length, so an
/// unverified run would have rendered its header as several lines of
/// prose wearing a field label. Short, stable, whitespace-free.
#[test]
fn kind_is_a_short_single_token_for_every_variant() {
    let cases: Vec<WeightsPinError> = vec![
        WeightsPinError::PropsUnavailable("connection refused".to_string()),
        WeightsPinError::NoModelPath,
        WeightsPinError::Unreadable(
            PathBuf::from("/m/w.gguf"),
            std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        ),
        WeightsPinError::Mismatch {
            path: PathBuf::from("/m/w.gguf"),
            actual: FileDigest { sha256: SHA256_HELLO.to_string(), size_bytes: 5 },
        },
    ];
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
    assert_eq!(seen.len(), cases.len(), "every variant needs a distinct kind");
}
