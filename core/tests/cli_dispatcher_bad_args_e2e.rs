//! Pin the unknown-action exit-code contract for the `kastellan-cli`
//! per-subcommand dispatchers that don't already have a dedicated
//! bad-args test (`entities` is already covered by
//! `cli_entities_e2e::cli_entities_bad_args_exit_code_two`).
//!
//! Why this file exists: [Issue #97][issue-97] moved the
//! `multi_thread_runtime` construction in 4 dispatchers from
//! *before* the action match to *inside* the known-action arms,
//! so an invalid action (`kastellan-cli tasks frobnicate`) no longer
//! spawns tokio worker threads it never uses. The structural change
//! is invisible from outside; the observable contract is
//! "invalid action -> exit 2 + the same `<dispatcher>: unknown ...`
//! stderr line as before." These tests pin that observable surface
//! so a future refactor cannot change the wording or the exit code
//! by accident.
//!
//! [issue-97]: https://github.com/hherb/kastellan/issues/97
//!
//! No Postgres or daemon required: the bad-action path must exit
//! before any DB connection is attempted, so passing
//! `KASTELLAN_DATA_DIR=/nonexistent-...` proves the early-exit invariant.
//! Skips cleanly if the CLI binary hasn't been built.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::process::Command;

use kastellan_tests_common::cli_binary;

/// Build a minimal env block that points the CLI at a non-existent
/// data dir. Used only as defence-in-depth: a bad-action path must
/// never reach `resolve_connect_spec`, so the value of
/// `KASTELLAN_DATA_DIR` is irrelevant. If the early-exit invariant
/// regresses, the CLI would error on `connect_runtime_pool` and
/// produce a *different* stderr — the assertions below would catch
/// that as a contract change.
fn bad_args_env() -> Vec<(String, String)> {
    let mut env = vec![(
        "KASTELLAN_DATA_DIR".to_string(),
        "/nonexistent-kastellan-cli-bad-args-test".to_string(),
    )];
    if let Some(home) = std::env::var_os("HOME") {
        env.push(("HOME".to_string(), home.to_string_lossy().into_owned()));
    }
    env
}

#[test]
fn cli_tasks_unknown_subcommand_exits_two() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "[SKIP] cli_tasks_unknown_subcommand_exits_two: kastellan-cli binary not built at {}",
            bin.display(),
        );
        return;
    }

    let out = Command::new(&bin)
        .args(["tasks", "frobnicate"])
        .env_clear()
        .envs(bad_args_env())
        .output()
        .expect("spawn cli tasks frobnicate");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(2),
        "`tasks frobnicate` must exit 2; got {:?}\nstderr={stderr}",
        out.status,
    );
    assert!(
        stderr.contains("tasks: unknown subcommand"),
        "stderr must carry the dispatcher-prefixed unknown-subcommand line; got: {stderr}",
    );
}

#[test]
fn cli_memory_l1_unknown_action_exits_two() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "[SKIP] cli_memory_l1_unknown_action_exits_two: kastellan-cli binary not built at {}",
            bin.display(),
        );
        return;
    }

    let out = Command::new(&bin)
        .args(["memory", "l1", "frobnicate"])
        .env_clear()
        .envs(bad_args_env())
        .output()
        .expect("spawn cli memory l1 frobnicate");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(2),
        "`memory l1 frobnicate` must exit 2; got {:?}\nstderr={stderr}",
        out.status,
    );
    assert!(
        stderr.contains("memory l1: unknown action"),
        "stderr must carry the dispatcher-prefixed unknown-action line; got: {stderr}",
    );
}

#[test]
fn cli_tools_allowlist_unknown_subcommand_exits_two() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "[SKIP] cli_tools_allowlist_unknown_subcommand_exits_two: kastellan-cli binary not built at {}",
            bin.display(),
        );
        return;
    }

    let out = Command::new(&bin)
        .args(["tools", "allowlist", "frobnicate"])
        .env_clear()
        .envs(bad_args_env())
        .output()
        .expect("spawn cli tools allowlist frobnicate");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(2),
        "`tools allowlist frobnicate` must exit 2; got {:?}\nstderr={stderr}",
        out.status,
    );
    assert!(
        stderr.contains("tools allowlist: unknown subcommand"),
        "stderr must carry the dispatcher-prefixed unknown-subcommand line; got: {stderr}",
    );
}

/// `entities kinds` mirror of the `relations kinds` posture below
/// (both ride on `connect_admin_pool`). The unknown-action path must
/// exit 2 *before* runtime construction so a typo doesn't burn tokio
/// worker threads.
#[test]
fn cli_entities_kinds_unknown_action_exits_two() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "[SKIP] cli_entities_kinds_unknown_action_exits_two: kastellan-cli binary not built at {}",
            bin.display(),
        );
        return;
    }

    let out = Command::new(&bin)
        .args(["entities", "kinds", "frobnicate"])
        .env_clear()
        .envs(bad_args_env())
        .output()
        .expect("spawn cli entities kinds frobnicate");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(2),
        "`entities kinds frobnicate` must exit 2; got {:?}\nstderr={stderr}",
        out.status,
    );
    assert!(
        stderr.contains("entities kinds: unknown subcommand"),
        "stderr must carry the dispatcher-prefixed unknown-subcommand line; got: {stderr}",
    );
}

/// `relations kinds` mirrors the with_runtime posture: the unknown-
/// action path returns from the dispatcher *before* tokio runtime
/// construction or any DB connection attempt. Pin the early-exit
/// observable contract (exit 2 + the dispatcher-prefixed stderr line)
/// so a future refactor cannot drift it.
#[test]
fn cli_relations_kinds_unknown_action_exits_two() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "[SKIP] cli_relations_kinds_unknown_action_exits_two: kastellan-cli binary not built at {}",
            bin.display(),
        );
        return;
    }

    let out = Command::new(&bin)
        .args(["relations", "kinds", "frobnicate"])
        .env_clear()
        .envs(bad_args_env())
        .output()
        .expect("spawn cli relations kinds frobnicate");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(2),
        "`relations kinds frobnicate` must exit 2; got {:?}\nstderr={stderr}",
        out.status,
    );
    assert!(
        stderr.contains("relations kinds: unknown subcommand"),
        "stderr must carry the dispatcher-prefixed unknown-subcommand line; got: {stderr}",
    );
}

/// Top-level `relations garbage` (one level up from `kinds garbage`)
/// also must exit 2 before runtime construction.
#[test]
fn cli_relations_top_level_unknown_subcommand_exits_two() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "[SKIP] cli_relations_top_level_unknown_subcommand_exits_two: kastellan-cli binary not built at {}",
            bin.display(),
        );
        return;
    }

    let out = Command::new(&bin)
        .args(["relations", "garbage"])
        .env_clear()
        .envs(bad_args_env())
        .output()
        .expect("spawn cli relations garbage");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(2),
        "`relations garbage` must exit 2; got {:?}\nstderr={stderr}",
        out.status,
    );
    assert!(
        stderr.contains("relations: unknown subcommand"),
        "stderr must carry the dispatcher-prefixed unknown-subcommand line; got: {stderr}",
    );
}

/// `relations show` (no entity-id) must exit 2 with the usage line
/// *before* any runtime construction or DB connection attempt — same
/// Issue #97 posture as the other relations dispatchers.
#[test]
fn cli_relations_show_missing_id_exits_two() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "[SKIP] cli_relations_show_missing_id_exits_two: kastellan-cli binary not built at {}",
            bin.display(),
        );
        return;
    }

    let out = Command::new(&bin)
        .args(["relations", "show"])
        .env_clear()
        .envs(bad_args_env())
        .output()
        .expect("spawn cli relations show");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(2),
        "`relations show` must exit 2; got {:?}\nstderr={stderr}",
        out.status,
    );
    assert!(
        stderr.contains("usage: kastellan-cli relations show"),
        "stderr must carry the show-usage line; got: {stderr}",
    );
}

/// `guard` mirrors the `observation` posture: the dispatcher validates
/// its subcommand and every flag BEFORE `with_runtime`, so a typo
/// exits 2 without spawning tokio worker threads or touching the
/// network. Added with the guard-model slice, which introduced the
/// dispatcher; this file is the contract every dispatcher rides on.
#[test]
fn cli_guard_unknown_subcommand_exits_two() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "[SKIP] cli_guard_unknown_subcommand_exits_two: kastellan-cli binary not built at {}",
            bin.display(),
        );
        return;
    }

    let out = Command::new(&bin)
        .args(["guard", "frobnicate"])
        .env_clear()
        .envs(bad_args_env())
        .output()
        .expect("spawn cli guard frobnicate");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(2),
        "`guard frobnicate` must exit 2; got {:?}\nstderr={stderr}",
        out.status,
    );
    assert!(
        stderr.contains("guard: unknown subcommand"),
        "stderr must carry the dispatcher-prefixed unknown-subcommand line; got: {stderr}",
    );
}

/// Bare `guard` with no subcommand prints usage and exits 2.
#[test]
fn cli_guard_with_no_subcommand_exits_two_with_usage() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "[SKIP] cli_guard_with_no_subcommand_exits_two_with_usage: kastellan-cli binary not built at {}",
            bin.display(),
        );
        return;
    }

    let out = Command::new(&bin)
        .args(["guard"])
        .env_clear()
        .envs(bad_args_env())
        .output()
        .expect("spawn cli guard");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(2), "stderr={stderr}");
    assert!(stderr.contains("usage: kastellan-cli guard calibrate"), "got: {stderr}");
}

/// **Flag validation happens before the runtime AND before the corpus
/// load**, so a bad `--tau` cannot be mistaken for a corpus problem.
/// Both the out-of-range and the non-numeric spellings must be
/// rejected: `"nan".parse::<f32>()` SUCCEEDS, and a NaN tau silently
/// compares false against every score.
#[test]
fn cli_guard_calibrate_rejects_a_bad_tau_before_doing_anything() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "[SKIP] cli_guard_calibrate_rejects_a_bad_tau_before_doing_anything: kastellan-cli binary not built at {}",
            bin.display(),
        );
        return;
    }

    for tau in ["1.5", "-0.1", "banana", "nan", "inf"] {
        let out = Command::new(&bin)
            .args(["guard", "calibrate", "--tau", tau])
            .env_clear()
            .envs(bad_args_env())
            .output()
            .expect("spawn cli guard calibrate --tau");

        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(
            out.status.code(),
            Some(2),
            "`--tau {tau}` must exit 2; got {:?}\nstderr={stderr}",
            out.status,
        );
        assert!(
            stderr.contains("--tau requires a float in [0.0, 1.0]"),
            "`--tau {tau}` must name the constraint; got: {stderr}",
        );
    }
}

/// A flag missing its argument is a usage error, not a silent default.
#[test]
fn cli_guard_calibrate_rejects_a_dangling_corpus_flag() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "[SKIP] cli_guard_calibrate_rejects_a_dangling_corpus_flag: kastellan-cli binary not built at {}",
            bin.display(),
        );
        return;
    }

    let out = Command::new(&bin)
        .args(["guard", "calibrate", "--corpus"])
        .env_clear()
        .envs(bad_args_env())
        .output()
        .expect("spawn cli guard calibrate --corpus");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(2), "stderr={stderr}");
    assert!(stderr.contains("--corpus requires a DIR argument"), "got: {stderr}");
}

/// An unknown flag exits 2 rather than being ignored.
#[test]
fn cli_guard_calibrate_rejects_an_unknown_flag() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!(
            "[SKIP] cli_guard_calibrate_rejects_an_unknown_flag: kastellan-cli binary not built at {}",
            bin.display(),
        );
        return;
    }

    let out = Command::new(&bin)
        .args(["guard", "calibrate", "--frobnicate"])
        .env_clear()
        .envs(bad_args_env())
        .output()
        .expect("spawn cli guard calibrate --frobnicate");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(2), "stderr={stderr}");
    assert!(stderr.contains("guard calibrate: unknown flag"), "got: {stderr}");
}
