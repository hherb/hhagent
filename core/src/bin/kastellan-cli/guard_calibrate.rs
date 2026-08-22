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
        eprintln!("       kastellan-cli guard capture --manifest DIR --out DIR [--record]");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "calibrate" => run_guard_calibrate(&args[1..]),
        "capture" => crate::guard_capture::run(&args[1..]),
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
    use kastellan_core::cassandra::guard_model::policy::policy_digest;
    use kastellan_core::cassandra::guard_model::GuardClient;
    use kastellan_core::cassandra::injection_guard::{screen, BLOCK_THRESHOLD};
    use kastellan_core::guard_calibration::corpus::{load_corpus_from_dir, scannable_prefix};
    use kastellan_core::guard_calibration::report::{
        confusion_at, format_report, operating_point_invalidity, RunMeta, ScoredCase,
        BUDGET_SCOPE,
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
    // Captured before `from_config` consumes the config, so the report
    // header can name the endpoint that actually produced the scores.
    let guard_endpoint = cfg.guard_url.clone().unwrap_or_default();
    let guard_model = cfg.guard_model.clone().unwrap_or_default();
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
        // What the chokepoint would actually see. Production reaches
        // `screen` through `extract_scannable_text`, which caps at
        // SCAN_BYTE_CAP; scoring the full text would fit tau against
        // text the guard never receives. No shipped case is over the
        // cap — this is for measurement 3's captured half, where a
        // 200 KiB fetched page is ordinary.
        let text = scannable_prefix(&case.text);
        let catalogue_score = screen(text).score;
        // Cases the catalogue already blocks are NOT sent to the model.
        // The report excludes them, so the call could only ever be
        // discarded — and D4's reason for not consulting the model
        // above the threshold ("there is no verdict it could return
        // that would change the outcome, so the call would be pure
        // latency") applies to the harness exactly as it does to the
        // wiring. It also keeps an error on a case that is never
        // adjudicated from aborting the whole run.
        let probability = if catalogue_score >= BLOCK_THRESHOLD {
            None
        } else {
            // Sequential on purpose: this is offline tooling against one
            // local server, and a burst of concurrent requests would make
            // any latency figure taken alongside it meaningless.
            match client.probability(text).await {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("guard calibrate: {} failed: {e}", case.id);
                    return ExitCode::from(1);
                }
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

    let meta = RunMeta {
        endpoint: guard_endpoint,
        model: guard_model,
        policy_digest: policy_digest(),
        // The harness models Strict only. Named rather than assumed:
        // `GuardProfile::for_tool` returns Relaxed for the web tools, so
        // production adjudicates some cases this report excludes.
        profile: "Strict (web-fetch/web-search run Relaxed; not modelled here)",
    };
    print!("{}", format_report(&scored, tau, &meta));

    // A run that is not believable must not exit 0, or a CI caller
    // reads the zero and moves on. TWO SOURCES, because the matrix and
    // D7's operating point can each be unbelievable on their own:
    //
    // * the matrix -- an unmeasured case means fix the backend, an empty
    //   matrix means fix the corpus;
    // * the operating point -- until this call existed the exit code
    //   came from the matrix ALONE, so a corpus whose budget scope holds
    //   no benign cases, or which is single-class, or which no threshold
    //   fits, printed its `NONE (...)` line and exited 0. Deleting the
    //   whole operating-point section changed no exit code anywhere.
    let confusion = confusion_at(&scored, tau);
    let invalidity = confusion
        .invalidity()
        .map(str::to_string)
        .or_else(|| operating_point_invalidity(&scored, BUDGET_SCOPE));
    match invalidity {
        None => ExitCode::from(0),
        Some(reason) => {
            eprintln!("guard calibrate: run INVALID ({reason})");
            ExitCode::from(1)
        }
    }
}
