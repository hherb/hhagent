//! `guard capture --manifest DIR --out DIR [--record]` — materialise the
//! calibration corpus by driving the **real** `web-fetch` worker.
//!
//! **Why the real worker and not `curl`.** Spec D3. A document reaches
//! the chokepoint through the worker's own extraction and then through
//! [`extract_scannable_text`], which strips keys, flattens leaves
//! alphabetically and truncates at [`SCAN_BYTE_CAP`]. A corpus fetched
//! with `curl` would be scored on text production never sees, and the
//! resulting τ would be fitted against a fiction.
//!
//! **It goes through the existing chokepoint, not around it.**
//! `WorkerCommand::new` is deliberately module-private — its doc calls
//! editing `tool_host` "the reviewable opt-out for the dispatcher
//! chokepoint" — and CLAUDE.md forbids a spawn-unsandboxed escape
//! hatch. So this uses the already-public
//! [`kastellan_core::tool_host::dispatch_with_sink`] and avoids
//! Postgres by passing a **null audit sink**, rather than by avoiding
//! the chokepoint. No new dispatch API is introduced.
//!
//! **One command records and verifies**, so materialising a corpus and
//! capturing it cannot drift apart. `--record` prints the observed hash
//! for the operator to commit; without it, an entry whose hash differs
//! is a hard failure and an entry with no hash at all is refused rather
//! than passed.

use std::path::PathBuf;
use std::process::ExitCode;

use sha2::{Digest, Sha256};

use kastellan_core::cassandra::injection_guard::{extract_scannable_text, SCAN_BYTE_CAP};
use kastellan_core::guard_calibration::manifest::{
    load_manifest_from_dir, verify_requirement, ManifestEntry,
};
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, spawn_worker, AuditSink, WorkerSpec};
use kastellan_core::worker_manifest::{discover_binary, ResolveCtx};
use kastellan_core::workers::web_fetch::web_fetch_entry;

/// SHA-256 of the scannable text, lowercase hex.
///
/// **Over the SCANNABLE text, not the raw response.** That is what
/// production screens, and it is also what makes the hash stable
/// against JSON key ordering — `extract_scannable_text` flattens leaves
/// alphabetically with keys discarded, so a server that reorders its
/// fields does not read as a drifted source.
pub fn sha256_hex(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    format!("{:x}", h.finalize())
}

/// Discards every row.
///
/// Capture is an offline corpus-building step, not agent activity:
/// there is no task, no plan and no operator inbox for these rows to
/// belong to, and requiring a live Postgres to fetch a web page would
/// make the corpus harder to reproduce for no gain. The *chokepoint*
/// still runs — only its audit destination is a sink hole.
struct NullSink;

#[async_trait::async_trait]
impl AuditSink for NullSink {
    async fn insert(
        &self,
        _actor: &str,
        _action: &str,
        _payload: serde_json::Value,
    ) -> Result<i64, kastellan_db::DbError> {
        Ok(0)
    }
}

/// Was this result the injection placeholder rather than a page?
///
/// **This check is why the corpus cannot be silently corrupted.** On a
/// catalogue `Block`, `post_process::finalize` replaces the worker's
/// result with `injection_blocked_placeholder(..)`. Stored as a corpus
/// case that placeholder is a short, *benign-looking* document that the
/// catalogue does **not** block — so it would enter the adjudicated
/// population and be scored, recording a page as the opposite of what
/// it is.
///
/// Refusing costs nothing: a case the catalogue blocks is
/// `excluded_already_blocked` and contributes nothing to τ either way.
fn is_injection_placeholder(v: &serde_json::Value) -> bool {
    v.get("injection_blocked") == Some(&serde_json::Value::Bool(true))
}

/// What one manifest entry produced.
enum Outcome {
    Materialised { text: String, sha256: String, truncated: bool },
    Refused(String),
}

pub fn run(args: &[String]) -> ExitCode {
    let mut manifest_dir: Option<PathBuf> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut record = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--manifest" => {
                i += 1;
                match args.get(i) {
                    Some(p) => manifest_dir = Some(PathBuf::from(p)),
                    None => {
                        eprintln!("--manifest requires a DIR argument");
                        return ExitCode::from(2);
                    }
                }
            }
            "--out" => {
                i += 1;
                match args.get(i) {
                    Some(p) => out_dir = Some(PathBuf::from(p)),
                    None => {
                        eprintln!("--out requires a DIR argument");
                        return ExitCode::from(2);
                    }
                }
            }
            "--record" => record = true,
            other => {
                eprintln!("{USAGE}");
                eprintln!("unexpected argument: {other}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    let (Some(manifest_dir), Some(out_dir)) = (manifest_dir, out_dir) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };

    let entries = match load_manifest_from_dir(&manifest_dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    // In verify mode every entry must already carry a usable hash.
    // Checked for the WHOLE manifest before any fetch, so a manifest-wide
    // omission is reported without spending a single network round trip
    // — and so a partial materialisation cannot leave the out dir in a
    // state that looks complete.
    if !record {
        let refusals: Vec<String> = entries.iter().filter_map(verify_requirement).collect();
        if !refusals.is_empty() {
            for r in &refusals {
                eprintln!("REFUSED {r}");
            }
            eprintln!("\n{} entries cannot be verified; nothing fetched.", refusals.len());
            return ExitCode::FAILURE;
        }
    }

    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("cannot create {}: {e}", out_dir.display());
        return ExitCode::FAILURE;
    }

    let mut failures = 0usize;
    for entry in &entries {
        match capture_one(entry, record) {
            Ok(Outcome::Refused(why)) => {
                eprintln!("REFUSED {}: {why}", entry.id);
                failures += 1;
            }
            Ok(Outcome::Materialised { text, sha256, truncated }) => {
                if record {
                    println!(
                        "RECORD {} {sha256} ({} bytes{})",
                        entry.id,
                        text.len(),
                        if truncated { ", truncated at the cap" } else { "" }
                    );
                } else {
                    println!("OK {} ({} bytes)", entry.id, text.len());
                }
                if let Err(e) = write_case(&out_dir, entry, &text) {
                    eprintln!("WRITE-FAILED {}: {e}", entry.id);
                    failures += 1;
                }
            }
            Err(e) => {
                eprintln!("FETCH-FAILED {}: {e}", entry.id);
                failures += 1;
            }
        }
    }

    if failures > 0 {
        eprintln!("\n{failures} of {} entries failed.", entries.len());
        return ExitCode::FAILURE;
    }
    println!(
        "\n{} entries materialised into {}",
        entries.len(),
        out_dir.display()
    );
    ExitCode::SUCCESS
}

const USAGE: &str = "usage: kastellan-cli guard capture --manifest DIR --out DIR [--record]";

/// Fetch one entry through the real sandboxed worker and hash what the
/// chokepoint saw.
fn capture_one(entry: &ManifestEntry, record: bool) -> Result<Outcome, String> {
    let fetched = fetch_through_worker(&entry.source)?;
    if is_injection_placeholder(&fetched) {
        return Ok(Outcome::Refused(format!(
            "the catalogue already blocks {}, so dispatch returned the withheld \
             placeholder rather than the page. Storing it would record a \
             benign-looking document in place of the real one -- and a case the \
             catalogue blocks is excluded from the fit anyway, so nothing is lost \
             by dropping it",
            entry.source
        )));
    }
    let (text, truncated) = extract_scannable_text(&fetched, SCAN_BYTE_CAP);
    let observed = sha256_hex(&text);

    if !record {
        // Cannot be `None` here: the whole manifest was verified above.
        let expected = entry.verified_sha256().map_err(|e| e.to_string())?;
        if observed != expected {
            return Ok(Outcome::Refused(format!(
                "manifest {expected}, observed {observed}. The source has drifted; \
                 investigate before trusting any tau fitted against it"
            )));
        }
    }
    Ok(Outcome::Materialised {
        text,
        sha256: observed,
        truncated,
    })
}

fn write_case(
    out_dir: &std::path::Path,
    entry: &ManifestEntry,
    text: &str,
) -> Result<(), String> {
    // `Label` derives `Deserialize` only, so the wire spelling is
    // written out here rather than derived. That is the right way round:
    // this is the format the corpus loader must accept, so a mismatch is
    // a load error rather than a silent rename following a Rust
    // identifier.
    let label = match entry.label {
        kastellan_core::guard_calibration::corpus::Label::Attack => "attack",
        kastellan_core::guard_calibration::corpus::Label::Benign => "benign",
    };
    let case = serde_json::json!({
        "id": entry.id,
        "label": label,
        "provenance": "captured",
        "text": text,
        "notes": entry.notes,
    });
    let path = out_dir.join(format!("{}.json", entry.id));
    let body = serde_json::to_vec_pretty(&case).map_err(|e| e.to_string())?;
    std::fs::write(&path, body).map_err(|e| format!("{}: {e}", path.display()))
}

/// Fetch one URL through the real sandboxed `web-fetch` worker.
///
/// Requires the worker binary to be discoverable (`current_exe()`-relative)
/// and the host to carry a `web-fetch` `tool_allowlists` row for the
/// domain — without one this returns `-32001: host … not on allowlist`,
/// which is what every web-fetch attempt on the deployed DGX has hit.
fn fetch_through_worker(url: &str) -> Result<serde_json::Value, String> {
    let host = url
        .strip_prefix("https://")
        .and_then(|r| r.split('/').next())
        .ok_or_else(|| format!("cannot derive a host from {url:?}"))?
        .to_string();

    // The same `current_exe()`-relative discovery the daemon runs, driven
    // through the identical `ResolveCtx` seam (cf. `broker::config`), so a
    // capture run and a live dispatch cannot resolve different binaries.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf));
    let get_env = |k: &str| std::env::var(k).ok();
    let exists = |p: &std::path::Path| p.exists();
    let is_dir = |p: &std::path::Path| p.is_dir();
    let allowlist = |_t: &str| Vec::new();
    let canonicalize = |p: &std::path::Path| std::fs::canonicalize(p).ok();
    let ctx = ResolveCtx {
        get_env: &get_env,
        exists: &exists,
        is_dir: &is_dir,
        exe_dir: exe_dir.as_deref(),
        canonicalize: &canonicalize,
        allowlist: &allowlist,
    };
    let worker_path =
        discover_binary(&ctx, "KASTELLAN_WEB_FETCH_BIN", "kastellan-worker-web-fetch")
            .ok_or_else(|| {
                "kastellan-worker-web-fetch not found next to this binary; build the \
                 workspace or run `kastellan-cli install`"
                    .to_string()
            })?;
    let entry = web_fetch_entry(worker_path.clone(), &[host]);
    let backend = kastellan_sandbox::default_backend();
    let worker_str = worker_path.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &worker_str,
        args: &[],
        wall_clock_ms: None,
    };
    let mut worker = spawn_worker(&*backend, &spec).map_err(|e| e.to_string())?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    let result = rt.block_on(async {
        dispatch_with_sink(
            &NullSink,
            &Vault::new(),
            &mut worker,
            "web-fetch",
            "web.fetch",
            serde_json::json!({ "url": url }),
        )
        .await
        .map_err(|e| e.to_string())
    });
    let _ = worker.close();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hash is over text, so it is stable against anything
    /// `extract_scannable_text` normalises away.
    #[test]
    fn sha256_hex_is_lowercase_and_64_chars() {
        let h = sha256_hex("hello");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        assert_ne!(h, sha256_hex("hell0"), "must actually depend on the input");
    }

    /// The placeholder detector must fire on the real shape and not on
    /// an ordinary page that happens to mention the words.
    #[test]
    fn the_placeholder_is_detected_by_its_flag_not_its_prose() {
        let placeholder = serde_json::json!({
            "injection_blocked": true,
            "note": "[tool output withheld: failed injection screen]",
            "score": 0.9,
            "reason_codes": ["instruction_override"],
        });
        assert!(is_injection_placeholder(&placeholder));

        // A page ABOUT injection blocking is not a placeholder. Under
        // D4 that is a benign captured case and must materialise.
        let article = serde_json::json!({
            "title": "How injection_blocked placeholders work",
            "text": "When the screen fires, the tool output is withheld.",
        });
        assert!(!is_injection_placeholder(&article));

        // A page that merely carries the key with a non-true value.
        let odd = serde_json::json!({ "injection_blocked": "true" });
        assert!(
            !is_injection_placeholder(&odd),
            "a string is not the boolean the chokepoint writes"
        );
    }
}
