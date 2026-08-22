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
//! A third hazard is **not** left as a limitation, because it is a
//! fail-open rather than a gap: a *relative* `model_path` resolves
//! against **this** process's working directory, not the server's, so a
//! copy of the pinned file sitting at the same relative path under the
//! CLI's cwd would hash as `Pinned` while the server served other
//! bytes. [`WeightsPinError::RelativePath`] refuses instead — the same
//! rule `SandboxPolicy.fs_read` follows.
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

/// Is `s` exactly 64 lowercase hex digits — the shape [`digest_file`]
/// emits and the only shape [`FileDigest`] may hold?
///
/// Lowercase specifically, not merely hex: [`hash_matches`] compares
/// byte-for-byte, so admitting uppercase would make two spellings of
/// one hash unequal. Rejecting it at construction is what lets the
/// comparison stay a plain `==`.
fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A hashed file: what it is, and how big it was.
///
/// Size rides along because it is free to count while streaming and it
/// is the diagnostic that separates "different build of the right
/// model" from "wrong file" — see [`PINNED_SIZE_BYTES`]. Taking it from
/// the same read as the hash also avoids a second `stat` that could
/// describe a different file.
///
/// **The fields are private, and that is load-bearing.** The first
/// version of the CLI's opt-out path needed a "we never hashed
/// anything" state, `WeightsProvenance` had no variant for it, so it
/// synthesised a `FileDigest` holding `"<unverified: …>"` and
/// `size_bytes: 0` — putting a fabricated byte count into the operator
/// artefact this module exists to make trustworthy. With no public
/// constructor that shortcut does not compile, and the missing state
/// had to become [`WeightsProvenance::Unverified`], which is what it
/// always was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDigest {
    sha256: String,
    size_bytes: u64,
}

impl FileDigest {
    /// The lowercase hex sha256. 64 chars, guaranteed by construction.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Bytes read while hashing — a count, never a `stat`.
    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    /// Build one from a hash that was computed elsewhere — a fixture, or
    /// a sum parsed out of the bash pin.
    ///
    /// `None` for anything that is not 64 lowercase hex digits, so the
    /// invariant [`digest_file`] establishes cannot be bypassed by the
    /// one other way in.
    pub fn from_hex(sha256: &str, size_bytes: u64) -> Option<Self> {
        is_sha256_hex(sha256).then(|| Self { sha256: sha256.to_string(), size_bytes })
    }
}

/// What the report and `RunMeta` record about the weights behind a run.
///
/// Exists so an artefact can never be silent about this. A calibration
/// report's τ is only meaningful against known bytes, so the bytes
/// travel *with* the number rather than depending on an operator
/// remembering which server was up.
///
/// Three variants, not two, because "we hashed it and it differs" and
/// "we never hashed anything" are different claims and a report that
/// conflates them is wrong about what it did. Both non-pinned variants
/// carry the word `UNPINNED` so one grep finds every untrustworthy run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WeightsProvenance {
    /// Hashed, and it matched. Carries the digest it *measured* rather
    /// than letting the header recite [`PINNED_SHA256`] back — the
    /// point of the field is that a reader can tell a computed hash
    /// from a quoted constant.
    Pinned { path: PathBuf, digest: FileDigest },
    /// Hashed, and it is **not** the pinned file. The run proceeded only
    /// because `--weights-unpinned` was passed.
    Unpinned { path: PathBuf, digest: FileDigest },
    /// **Nothing was hashed** — `/props` was unreachable, named no
    /// path, named a relative one, or named a file we could not read —
    /// and `--weights-unpinned` let the run proceed anyway. `kind` is
    /// [`WeightsPinError::kind`], which is already constrained to be
    /// short and whitespace-free so the header stays one field.
    Unverified { kind: &'static str },
}

impl WeightsProvenance {
    /// The `weights:` block for the report header.
    ///
    /// Every non-pinned rendering states the consequence, not just the
    /// fact: a reader who sees only a hash has to know what it means,
    /// and the person most likely to read this report is the one who
    /// will paste its τ into production.
    pub fn header_line(&self) -> String {
        match self {
            Self::Pinned { path, digest } => format!(
                "weights:       {} ({} bytes) pinned\n\
                 \x20              {}",
                digest.sha256(),
                digest.size_bytes(),
                path.display()
            ),
            Self::Unpinned { path, digest } => format!(
                "weights:       {} ({} bytes) UNPINNED\n\
                 \x20              {}\n\
                 \x20              expected {PINNED_SHA256}\n\
                 \x20              This run CANNOT support the cross-host tau comparison.",
                digest.sha256(),
                digest.size_bytes(),
                path.display()
            ),
            Self::Unverified { kind } => format!(
                "weights:       <unverified: {kind}> UNPINNED -- nothing was hashed\n\
                 \x20              expected {PINNED_SHA256}\n\
                 \x20              This run CANNOT support the cross-host tau comparison."
            ),
        }
    }
}

/// Why the guard weights could not be verified.
///
/// Every variant is fatal unless the operator passed
/// `--weights-unpinned`. They are kept **separate** rather than collapsed
/// into one "weights bad" string because they call for different
/// actions: fix the server, upgrade the server, restart it with an
/// absolute path, run somewhere else, or treat it as an incident.
#[derive(Debug)]
pub enum WeightsPinError {
    /// `/props` could not be fetched or parsed.
    PropsUnavailable(String),
    /// `/props` parsed but carried no usable `model_path`.
    NoModelPath,
    /// `model_path` was relative. Refused rather than resolved — see the
    /// module doc: a relative path is interpreted against *this*
    /// process's cwd, so it can hash a different file than the server
    /// opened, and do it silently.
    RelativePath(PathBuf),
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
            Self::RelativePath(_) => "relative-path",
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
            Self::RelativePath(path) => write!(
                f,
                "the guard backend reported a RELATIVE model_path ({}), which cannot \
                 be verified.\n\
                 A relative path resolves against THIS tool's working directory, not \
                 the server's, so hashing it could silently hash a different file \
                 that happens to sit at the same relative path -- and report it as \
                 pinned. Restart llama-server with an absolute -m path, or re-run \
                 with --weights-unpinned to proceed on an explicitly unverified run.",
                path.display()
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
                 core/src/cassandra/guard_model/weights_pin/mod.rs AND \
                 KASTELLAN_GUARD_WEIGHTS_SHA256 in scripts/eval/lib/guard-weights.sh \
                 together, and re-run measurement 3 -- a tau fitted on other weights \
                 does not transfer. To calibrate a CANDIDATE model without changing \
                 the pin, re-run with --weights-unpinned. See issue #592.",
                path.display(),
                actual.sha256(),
                actual.size_bytes(),
                if actual.size_bytes() == PINNED_SIZE_BYTES {
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
/// Pure — no IO, no HTTP. `None` for every shape that is not a
/// **non-empty** string at the top-level `model_path` key, so a server
/// that reports the field as `null`, a number, an object, or `""` is
/// treated as "did not tell us" rather than coerced into a path. The
/// empty case matters because `PathBuf::from("")` opens as `ENOENT`,
/// which would misdiagnose a silent server as an unreachable filesystem
/// and send the operator to re-run on another host.
pub fn model_path_from_props(props: &serde_json::Value) -> Option<&str> {
    props.get("model_path")?.as_str().filter(|s| !s.is_empty())
}

/// Is `sha256` the `expected` hash?
///
/// Pure, and `expected` is a **parameter** rather than a read of
/// [`PINNED_SHA256`] for the same reason `guest_kernel_pin::verify_kernel`
/// takes one: with the pin hard-wired, the accepting arm could only be
/// exercised by a 3.6 GB fixture, so an implementation that always
/// rejected would pass every test that could be written.
///
/// A plain `==` is sufficient because [`FileDigest`] can only hold 64
/// lowercase hex digits — see [`is_sha256_hex`]. Casing is not a
/// tolerance question here; it is unrepresentable.
pub fn hash_matches(sha256: &str, expected: &str) -> bool {
    sha256 == expected
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

/// Hash the file at `path` and require that it matches `expected`.
///
/// `Ok(digest)` is the single success: the file was absolute, readable,
/// and its bytes are the ones asked for. **Every** other outcome is a
/// [`WeightsPinError`] naming which one, because there is no caller for
/// whom "these are not the weights you asked for" is a value rather
/// than a problem — the calibration harness's `--weights-unpinned` is a
/// decision applied to the *error*, which keeps that policy in one
/// place instead of splitting it across a `Result` and an enum.
///
/// Takes `expected` so the accepting arm is reachable from a unit test
/// with a small fixture — see [`hash_matches`].
pub fn verify_weights_against(path: &Path, expected: &str) -> Result<FileDigest, WeightsPinError> {
    if !path.is_absolute() {
        return Err(WeightsPinError::RelativePath(path.to_path_buf()));
    }
    let digest =
        digest_file(path).map_err(|e| WeightsPinError::Unreadable(path.to_path_buf(), e))?;
    if hash_matches(digest.sha256(), expected) {
        Ok(digest)
    } else {
        Err(WeightsPinError::Mismatch { path: path.to_path_buf(), actual: digest })
    }
}

/// [`verify_weights_against`] with the in-repo pin. The thin wrapper
/// production uses; the logic lives in the parameterised form above.
pub fn verify_weights_at(path: &Path) -> Result<FileDigest, WeightsPinError> {
    verify_weights_against(path, PINNED_SHA256)
}

#[cfg(test)]
mod tests;
