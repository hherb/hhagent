//! The pinned Shieldstral guard weights, verified at use (issue #592).
//!
//! # Why this exists
//!
//! Every document in the tree said the two hosts ran the same guard
//! model — ROADMAP: *"Runtime + quantisation PINNED … `Shieldstral-1.0-3B-Q8_0`
//! … on BOTH hosts"*. Measured 2026-08-22, they did not: the Mac held
//! upstream's file and the DGX held a **different Q8_0 build at the
//! identical byte length**, with a valid GGUF header, which loaded and
//! served correct verdicts. Nothing in the tree would have noticed.
//!
//! **Pinning a quantisation label is not pinning the bytes.** Q8_0's
//! size follows from tensor shapes, so two independent conversions of
//! one model agree on size while differing in metadata, tensor ordering
//! or imatrix use. Size is therefore *not* a discriminator, which is
//! precisely why this module hashes.
//!
//! It matters because the measurement-3 corpus design fits τ on each
//! host and compares the two **as a test of the cross-platform claim**.
//! Run against two different builds, either outcome is uninterpretable:
//! agreement reads as vindication, disagreement as a platform problem.
//!
//! # Honest limitation: we cannot hash what we do not open
//!
//! kastellan never opens the GGUF — `llama-server` does, and we reach it
//! over HTTP. llama.cpp's `/v1/models` reports an **empty** `digest`,
//! and the fields it does report (`ftype`, `size`, `n_params`) are the
//! shape facts that two Q8_0 builds share. So the endpoint cannot prove
//! which bytes it loaded.
//!
//! What it *can* do is name the file: `/props` reports `model_path`. So
//! the check is "ask the server which file it opened, then hash that
//! file ourselves". Two limits travel with that, and neither is sold
//! around:
//!
//!   * it **trusts the server's self-report** of the path. A server
//!     lying about `model_path` defeats it. That is a far weaker
//!     adversary than the one this is for — the real failure here was an
//!     honest server loading a file nobody had checked;
//!   * it is **TOCTOU**. We hash the file; the server opened it earlier
//!     and may reopen it later. What this buys is catching the file that
//!     was *never* the pinned one, which is the case that actually
//!     occurred.
//!
//! # Cost
//!
//! Hashing the 3.6 GB file measured **1.65 s** on the DGX, twice, warm
//! (`sha256sum`, 2026-08-22). Paid **once per calibration run**, as a
//! precondition — against a run that makes ~100 adjudications at up to
//! ~3.2 s each at `SCAN_BYTE_CAP`, it is not worth caching, and a cache
//! is exactly what would let a swapped file go unnoticed.
//!
//! This is the same posture [`kastellan_sandbox::guest_kernel_pin`]
//! documents for the micro-VM guest kernel, and the same
//! bash-and-Rust duplication: `scripts/eval/lib/guard-weights.sh`
//! carries the sum for operator pre-flight, and
//! `kastellan-tests-common`'s `rust_and_bash_guard_pins_agree` fails the
//! PR if the two ever drift.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// sha256 of upstream's `Shieldstral-1.0-3B-Q8_0.gguf`.
///
/// This is HuggingFace's canonical LFS oid for
/// `noctrex/Shieldstral-1.0-3B-GGUF` → `Shieldstral-1.0-3B-Q8_0.gguf`,
/// confirmed byte-for-byte on both hosts on 2026-08-22.
///
/// Must equal `KASTELLAN_GUARD_WEIGHTS_SHA256` in
/// `scripts/eval/lib/guard-weights.sh`. Bump both together, in the same
/// deliberate step as the model change — and never "fix" a mismatch by
/// pasting in whatever a failure printed. A mismatch means either the
/// model was changed on purpose (then this is a reviewable commit that
/// also re-runs measurement 3) or the file is not what it claims (then
/// it is an incident).
pub const PINNED_SHA256: &str = "35b755bed2d473fb3d88f7d1d7b83203bd9f0c1b8bff42624e6fd9231d89d3c4";

/// Byte length of the pinned file.
///
/// **Deliberately not part of the check** — #592's whole point is that
/// the wrong file had this exact length. It is carried only so a
/// mismatch message can tell the operator which of two very different
/// situations they are in: a same-size mismatch is a different
/// *quantiser run* of the right model, a different-size mismatch is the
/// wrong file altogether.
pub const PINNED_SIZE_BYTES: u64 = 3_651_679_008;

/// Where the pinned file comes from, for the error message. An operator
/// reading a mismatch needs to know what to fetch.
pub const PINNED_SOURCE: &str =
    "https://huggingface.co/noctrex/Shieldstral-1.0-3B-GGUF (Shieldstral-1.0-3B-Q8_0.gguf)";

/// Read this many bytes at a time while hashing.
///
/// The weights are ~3.6 GB. Streaming keeps peak memory flat instead of
/// scaling with the file — reading this one to a `Vec` would be a
/// 3.6 GB allocation. Same chunk size as the guest-kernel pin, and
/// small enough that a unit test can cross the boundary cheaply.
const HASH_CHUNK_BYTES: usize = 64 * 1024;

/// A hashed file: what it is, and how big it was.
///
/// Size rides along because it is free to count while streaming and it
/// is the diagnostic that separates "different build of the right
/// model" from "wrong file" — see [`PINNED_SIZE_BYTES`]. Taking it from
/// the same read as the hash also avoids a second `stat` that could
/// describe a different file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDigest {
    pub sha256: String,
    pub size_bytes: u64,
}

/// Whether a hash is the pinned one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WeightsVerdict {
    /// Matches [`PINNED_SHA256`].
    Pinned,
    /// Does not match. Carries the actual hash so the caller can name it
    /// — in a refusal, or in the report stamp when the operator passed
    /// `--weights-unpinned`.
    Unpinned { actual: String },
}

/// What the report and `RunMeta` record about the weights behind a run.
///
/// Exists so an artefact can never be silent about this. A calibration
/// report's τ is only meaningful against known bytes, so the bytes
/// travel *with* the number rather than depending on an operator
/// remembering which server was up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WeightsProvenance {
    /// Hashed, and it is the pinned file.
    Pinned,
    /// Hashed, and it is **not** the pinned file. The run proceeded only
    /// because `--weights-unpinned` was passed.
    Unpinned { digest: FileDigest },
}

impl WeightsProvenance {
    /// The `weights:` line for the report header.
    ///
    /// The unpinned rendering states the consequence, not just the fact:
    /// a reader who sees only a hash has to know what it means, and the
    /// person most likely to read this report is the one who will paste
    /// its τ into production.
    pub fn header_line(&self) -> String {
        match self {
            Self::Pinned => format!("weights:       {PINNED_SHA256} (pinned)"),
            Self::Unpinned { digest } => format!(
                "weights:       {} ({} bytes) UNPINNED\n\
                 \x20              expected {PINNED_SHA256}\n\
                 \x20              This run CANNOT support the cross-host tau comparison.",
                digest.sha256, digest.size_bytes
            ),
        }
    }
}

/// Why the guard weights could not be verified.
///
/// Every variant is fatal unless the operator passed
/// `--weights-unpinned`. They are kept **separate** rather than collapsed
/// into one "weights bad" string because they call for four different
/// actions: fix the server, upgrade the server, fix the path, or treat
/// it as an incident.
#[derive(Debug)]
pub enum WeightsPinError {
    /// `/props` could not be fetched or parsed.
    PropsUnavailable(String),
    /// `/props` parsed but carried no usable `model_path`.
    NoModelPath,
    /// `model_path` named a file we cannot read — the ordinary case
    /// being that the server runs on a different host from this tool.
    Unreadable(PathBuf, std::io::Error),
    /// The file exists and is not the pinned one.
    Mismatch { path: PathBuf, actual: FileDigest },
}

impl WeightsPinError {
    /// A short, stable, whitespace-free token naming which refusal this
    /// is.
    ///
    /// Separate from [`fmt::Display`] because the two have different
    /// jobs: `Display` is a paragraph telling an operator what to *do*,
    /// and this is a token that has to fit in one field of a report
    /// header. Interpolating the former where the latter belongs
    /// renders several lines of prose wearing a field label — which is
    /// exactly what the first version of the CLI's opt-out path did.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::PropsUnavailable(_) => "props-unreachable",
            Self::NoModelPath => "no-model-path",
            Self::Unreadable(..) => "unreadable",
            Self::Mismatch { .. } => "mismatch",
        }
    }
}

impl fmt::Display for WeightsPinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PropsUnavailable(why) => write!(
                f,
                "cannot reach the guard backend's /props endpoint to learn which \
                 weights it loaded: {why}\n\
                 /props is a llama.cpp endpoint; a backend that does not serve it \
                 cannot have its weights verified. Re-run with --weights-unpinned \
                 to proceed on an explicitly unverified run."
            ),
            Self::NoModelPath => write!(
                f,
                "the guard backend's /props carried no `model_path`, so there is no \
                 file to hash. Re-run with --weights-unpinned to proceed on an \
                 explicitly unverified run."
            ),
            Self::Unreadable(path, e) => write!(
                f,
                "the guard backend loaded {} but this tool cannot read it: {e}\n\
                 Weights are verified by hashing the file the server named, so this \
                 check only works where the server and this tool share a filesystem. \
                 Run the calibration on the host serving the model, or re-run with \
                 --weights-unpinned to proceed on an explicitly unverified run.",
                path.display()
            ),
            // The size clause is the diagnosis #592 turned on: same size
            // means a different quantiser run of the RIGHT model, which
            // is the case that looks correct and is not.
            Self::Mismatch { path, actual } => write!(
                f,
                "the guard weights at {} are NOT the pinned file -- refusing to fit a \
                 threshold against unknown bytes.\n  \
                 expected: {PINNED_SHA256}\n  \
                 actual:   {} ({} bytes)\n  {}\n\
                 Fetch the pinned file from {PINNED_SOURCE}. If the model was changed \
                 on purpose, update PINNED_SHA256 in \
                 core/src/cassandra/guard_model/weights_pin.rs AND \
                 KASTELLAN_GUARD_WEIGHTS_SHA256 in scripts/eval/lib/guard-weights.sh \
                 together, and re-run measurement 3 -- a tau fitted on other weights \
                 does not transfer. To calibrate a CANDIDATE model without changing \
                 the pin, re-run with --weights-unpinned. See issue #592.",
                path.display(),
                actual.sha256,
                actual.size_bytes,
                if actual.size_bytes == PINNED_SIZE_BYTES {
                    "Same size as the pinned file, so this is a DIFFERENT QUANTISER RUN \
                     of the right model -- not corruption, and not the wrong model."
                } else {
                    "Different size from the pinned file, so this is a different file \
                     altogether -- check the server was pointed at the intended path."
                },
            ),
        }
    }
}

impl std::error::Error for WeightsPinError {}

/// Extract `model_path` from a llama.cpp `/props` body.
///
/// Pure — no IO, no HTTP. `None` for every shape that is not a string at
/// the top-level `model_path` key, so a server that reports the field as
/// `null`, a number, or an object is treated as "did not tell us" rather
/// than coerced into a path.
pub fn model_path_from_props(props: &serde_json::Value) -> Option<&str> {
    props.get("model_path")?.as_str()
}

/// Is `sha256` the `expected` hash?
///
/// Pure, and `expected` is a **parameter** rather than a read of
/// [`PINNED_SHA256`] for the same reason `guest_kernel_pin::verify_kernel`
/// takes one: with the pin hard-wired, the accepting arm could only be
/// exercised by a 3.6 GB fixture, so an implementation that always
/// rejected would pass every test that could be written.
///
/// The comparison is **case-sensitive** on purpose: [`digest_file`]
/// always emits lowercase, and the constant's casing is pinned by
/// `pinned_sha256_is_64_lowercase_hex`. Accepting either case would
/// tolerate a pin that the shape test is there to reject loudly.
pub fn classify(sha256: &str, expected: &str) -> WeightsVerdict {
    if sha256 == expected {
        WeightsVerdict::Pinned
    } else {
        WeightsVerdict::Unpinned { actual: sha256.to_string() }
    }
}

/// Hash `path`, streaming it, and count its bytes on the same pass.
///
/// The only IO in this module.
pub fn digest_file(path: &Path) -> std::io::Result<FileDigest> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK_BYTES];
    let mut size_bytes: u64 = 0;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size_bytes += n as u64;
        hasher.update(&buf[..n]);
    }
    let sha256: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(FileDigest { sha256, size_bytes })
}

/// Hash the file at `path` and say whether it matches `expected`.
///
/// `Ok(Pinned)` / `Ok(Unpinned{..})` both mean "we successfully found
/// out". Deciding what an `Unpinned` *costs* is the caller's job,
/// because that differs between the calibration harness (refuse unless
/// the operator opted out) and any future consumer.
///
/// Takes `expected` so both arms are reachable from a unit test with a
/// small fixture — see [`classify`].
pub fn verify_weights_against(
    path: &Path,
    expected: &str,
) -> Result<WeightsProvenance, WeightsPinError> {
    let digest =
        digest_file(path).map_err(|e| WeightsPinError::Unreadable(path.to_path_buf(), e))?;
    match classify(&digest.sha256, expected) {
        WeightsVerdict::Pinned => Ok(WeightsProvenance::Pinned),
        WeightsVerdict::Unpinned { .. } => Ok(WeightsProvenance::Unpinned { digest }),
    }
}

/// [`verify_weights_against`] with the in-repo pin. The thin wrapper
/// production uses; the logic lives in the parameterised form above.
pub fn verify_weights_at(path: &Path) -> Result<WeightsProvenance, WeightsPinError> {
    verify_weights_against(path, PINNED_SHA256)
}


#[cfg(test)]
mod tests;
