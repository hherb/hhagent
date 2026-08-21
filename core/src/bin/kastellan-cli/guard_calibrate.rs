//! `guard calibrate [--corpus DIR] [--tau F]` — score a labelled corpus
//! through the shipping guard adjudicator and print a confusion matrix.
//!
//! Offline tooling. Nothing here runs in the daemon.

use std::path::PathBuf;
use std::process::ExitCode;

use crate::common::with_runtime;

pub(crate) fn run_guard(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: kastellan-cli guard calibrate [--corpus DIR] [--tau F]");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "calibrate" => run_guard_calibrate(&args[1..]),
        other => {
            eprintln!("guard: unknown subcommand {other}");
            ExitCode::from(2)
        }
    }
}

fn run_guard_calibrate(args: &[String]) -> ExitCode {
    let mut corpus_dir: Option<PathBuf> = None;
    let mut tau = kastellan_core::cassandra::guard_model::DEFAULT_TAU;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--corpus" => {
                i += 1;
                match args.get(i) {
                    Some(p) => corpus_dir = Some(PathBuf::from(p)),
                    None => {
                        eprintln!("--corpus requires a DIR argument");
                        return ExitCode::from(2);
                    }
                }
            }
            "--tau" => {
                i += 1;
                match args.get(i).and_then(|s| s.parse::<f32>().ok()) {
                    Some(v) if (0.0..=1.0).contains(&v) => tau = v,
                    _ => {
                        eprintln!("--tau requires a float in [0.0, 1.0]");
                        return ExitCode::from(2);
                    }
                }
            }
            other => {
                eprintln!("guard calibrate: unknown flag {other}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    let dir = corpus_dir.unwrap_or_else(default_corpus_dir);
    // Runtime construction deferred until here (all args parsed and
    // validated above) so a parse error never pays for a runtime it
    // won't use — the same posture the other CLI dispatchers take.
    with_runtime("guard calibrate", guard_calibrate_async(dir, tau))
}

/// Mirrors `observation_replay::default_captures_dir`: under `cargo run`
/// `CARGO_MANIFEST_DIR` points at `core/`, so the workspace root is one
/// level up. Installed binaries set neither var and fall back to a
/// CWD-relative path; `--corpus` is always the escape hatch.
fn default_corpus_dir() -> PathBuf {
    if let Some(manifest) = std::env::var_os("CARGO_MANIFEST_DIR") {
        let mut p = PathBuf::from(manifest);
        debug_assert_eq!(
            p.file_name().and_then(|s| s.to_str()),
            Some("core"),
            "default_corpus_dir assumes kastellan-cli lives in core/ \
             (CARGO_MANIFEST_DIR = {p:?})"
        );
        p.pop(); // strip `/core` to reach the workspace root
        p.push("tests/guard/corpus");
        return p;
    }
    PathBuf::from("tests/guard/corpus")
}

async fn guard_calibrate_async(dir: PathBuf, tau: f32) -> ExitCode {
    use kastellan_core::cassandra::guard_model::GuardClient;
    use kastellan_core::cassandra::injection_guard::screen;
    use kastellan_core::guard_calibration::corpus::load_corpus_from_dir;
    use kastellan_core::guard_calibration::report::{
        confusion_at, format_report, ScoredCase,
    };
    use kastellan_llm_router::RouterConfig;

    let cases = match load_corpus_from_dir(&dir) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    let cfg = match RouterConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("guard calibrate: router config: {e}");
            return ExitCode::from(1);
        }
    };
    let client = match GuardClient::from_config(&cfg) {
        Ok(None) => {
            eprintln!(
                "guard calibrate: the guard tier is unconfigured.\n\
                 Set KASTELLAN_LLM_GUARD_URL and KASTELLAN_LLM_GUARD_MODEL to a\n\
                 llama.cpp server running Shieldstral. It must NOT be the planner\n\
                 endpoint — a different model would return a number that looks like\n\
                 a score and means nothing."
            );
            return ExitCode::from(2);
        }
        // Covers both "only one of the two keys is set" and "the HTTP
        // client could not be built". The first is the one worth
        // separating from Ok(None): it is a misconfiguration, not an
        // opt-out, and reporting it as "unconfigured" is how a security
        // tier ends up silently off.
        Err(e) => {
            eprintln!("guard calibrate: guard tier is misconfigured: {e}");
            return ExitCode::from(2);
        }
        Ok(Some(c)) => c,
    };

    let mut scored: Vec<ScoredCase> = Vec::with_capacity(cases.len());
    for case in &cases {
        let catalogue_score = screen(&case.text).score;
        // Sequential on purpose: this is offline tooling against one
        // local server, and a burst of concurrent requests would make
        // any latency figure taken alongside it meaningless.
        let probability = match client.probability(&case.text).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("guard calibrate: {} failed: {e}", case.id);
                return ExitCode::from(1);
            }
        };
        scored.push(ScoredCase {
            id: case.id.clone(),
            label: case.label,
            provenance: case.provenance,
            catalogue_score,
            probability,
        });
    }

    print!("{}", format_report(&scored, tau));
    // A run containing any unmeasured case is reported as INVALID, not
    // as a slightly smaller sample — so the exit status has to say so
    // too, or a CI caller would read the zero and move on.
    if confusion_at(&scored, tau).is_valid() {
        ExitCode::from(0)
    } else {
        eprintln!("guard calibrate: run INVALID (unmeasured cases present)");
        ExitCode::from(1)
    }
}
