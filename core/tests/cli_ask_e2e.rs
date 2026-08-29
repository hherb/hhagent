//! End-to-end integration test for the `kastellan-cli ask` happy path
//! and the plan-iteration-cap failure path (Task 4.4).
//!
//! This is the regression pin that none of the other scheduler tests
//! satisfy: it spawns the **real `kastellan-cli` subprocess**, which
//! INSERTs a `tasks` row and LISTENs for the completion NOTIFY, while
//! the **real `kastellan` daemon** runs under `systemd --user` (Linux)
//! or `launchctl` (macOS), the **real sandboxed worker** runs under
//! bwrap (Linux) or sandbox-exec (macOS), and only the **LLM is
//! mocked** behind a queued multi-shot HTTP listener.
//!
//! Bring-up scaffolding (PG cluster, supervisor + sandbox skip
//! helpers, RAII guards, binary discovery, macOS launchd serial lock)
//! now lives in `kastellan-tests-common` as of issue #15.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use kastellan_supervisor::specs::core_service_spec;
use kastellan_supervisor::{default_supervisor, ServiceStatus};
use kastellan_tests_common::{
    bring_up_pg_cluster, cli_binary, core_binary, current_username, pg_bin_dir_or_skip,
    seed_tool_allowlist, shell_exec_worker_binary, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, unique_temp_root, wait_for_log_match,
    wait_for_status, PathGuard, PgCluster, ServiceGuard,
};
#[cfg(target_os = "macos")]
use kastellan_tests_common::serial_lock;

use kastellan_tests_common::scripted_llm::{
    embedding_envelope, envelope_for, plan_json, spawn_scripted_llm,
};

#[cfg(target_os = "linux")]
const ECHO_PATH: &str = "/usr/bin/echo";
#[cfg(target_os = "macos")]
const ECHO_PATH: &str = "/bin/echo";

/// Returns `true` (caller should `return` from its `#[test]`) when any
/// of the three workspace binaries this e2e exercises is missing.
fn skip_if_any_binary_missing() -> bool {
    for (label, p) in &[
        ("kastellan", core_binary()),
        ("kastellan-cli", cli_binary()),
        ("kastellan-worker-shell-exec", shell_exec_worker_binary()),
    ] {
        if !p.exists() {
            eprintln!(
                "\n[SKIP] {} binary missing at {}; run `cargo build --workspace`\n",
                label,
                p.display()
            );
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Daemon bring-up — wires the `kastellan` core service to the per-test
// PG cluster + mock LLM + workspace prompts + per-test allowlist.
// ---------------------------------------------------------------------------

/// Returned by [`bring_up_daemon`]. Surfaces the daemon's stdout/stderr
/// log files in test-failure messages so a flaky run shows what the
/// daemon was doing without a re-run.
struct Daemon {
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

fn bring_up_daemon(
    suffix: &str,
    data_dir: &Path,
    mock_base_url: &str,
    user: &str,
) -> (Daemon, (ServiceGuard, PathGuard, PathGuard)) {
    let core_log_dir = unique_temp_root("cli-clog");
    std::fs::create_dir_all(&core_log_dir).expect("create core log dir");
    let core_log_guard = PathGuard {
        path: core_log_dir.clone(),
    };

    let state_dir = unique_temp_root("cli-state");
    let state_guard = PathGuard {
        path: state_dir.clone(),
    };

    let binary = core_binary();
    let mut spec = core_service_spec(&binary, &core_log_dir);
    spec.name = format!("kastellan-supervisor-test-core-cliask-{suffix}");
    assert!(spec.name.len() <= 200);
    let stdout_path = core_log_dir.join(format!("{}.out", spec.name));
    let stderr_path = core_log_dir.join(format!("{}.err", spec.name));
    spec.stdout_log = Some(stdout_path.clone());
    spec.stderr_log = Some(stderr_path.clone());

    // Required env — see the spec doc for why each one is needed.
    spec.env.push((
        "KASTELLAN_DATA_DIR".into(),
        data_dir.to_string_lossy().into_owned(),
    ));
    spec.env.push(("USER".into(), user.to_string()));
    spec.env.push((
        "KASTELLAN_STATE_DIR".into(),
        state_dir.to_string_lossy().into_owned(),
    ));

    // Prompts: the daemon's prompt loader fails closed if the dir is
    // missing. Point at the in-tree `prompts/` (read-only access).
    let workspace_prompts = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("prompts");
    spec.env.push((
        "KASTELLAN_PROMPTS_DIR".into(),
        workspace_prompts.to_string_lossy().into_owned(),
    ));

    // LLM router → mock. The router's `compose_url` appends
    // `/chat/completions` to the base. Use `<mock>/v1` so the on-wire
    // URL matches the production OpenAI-compat shape.
    spec.env.push((
        "KASTELLAN_LLM_LOCAL_URL".into(),
        format!("{mock_base_url}/v1"),
    ));
    spec.env.push((
        "KASTELLAN_LLM_LOCAL_MODEL".into(),
        "test-local-model".into(),
    ));
    // 5 s is loose enough for slow CI runners — the mock responds
    // synchronously on accept, so on a healthy host this is sub-ms.
    spec.env.push(("KASTELLAN_LLM_TIMEOUT_MS".into(), "5000".into()));

    // Tool registry: register shell-exec. The argv allowlist is now
    // loaded from the DB at daemon start — see build_tool_registry.
    // Tests seed the allowlist via seed_tool_allowlist() before
    // calling bring_up_daemon so the daemon sees the correct entries.
    spec.env.push((
        "KASTELLAN_SHELL_EXEC_BIN".into(),
        shell_exec_worker_binary().to_string_lossy().into_owned(),
    ));

    let sup = default_supervisor();
    let service_guard = ServiceGuard {
        sup: default_supervisor(),
        name: spec.name.clone(),
    };
    sup.install(&spec).expect("install core");
    sup.start(&spec.name).expect("start core");

    wait_for_status(
        sup.as_ref(),
        &spec.name,
        |s| s == ServiceStatus::Active,
        Duration::from_secs(10),
    )
    .expect("core active");

    // Wait for the database probe to log success — that's our cue
    // that the daemon has finished bring-up and is ready to claim
    // tasks.
    wait_for_log_match(
        &stdout_path,
        |s| s.contains("scheduler spawned"),
        Duration::from_secs(10),
    )
    .expect("daemon should log 'scheduler spawned' within 10s");

    (
        Daemon {
            stdout_path,
            stderr_path,
        },
        (service_guard, core_log_guard, state_guard),
    )
}

fn echo_step(text: &str) -> serde_json::Value {
    serde_json::json!([{
        "tool":           "shell-exec",
        "method":         "shell.exec",
        "parameters":     {"argv": [ECHO_PATH, text]},
        "returns":        "stdout",
        "done_when":      "exit_code == 0",
        "classification": "Public",
    }])
}

fn cat_passwd_step() -> serde_json::Value {
    serde_json::json!([{
        "tool":           "shell-exec",
        "method":         "shell.exec",
        "parameters":     {"argv": ["/bin/cat", "/etc/passwd"]},
        "returns":        "stdout",
        "done_when":      "exit_code == 0",
        "classification": "Public",
    }])
}

/// Build the `audit_log` (actor, action) → count multiset.
async fn audit_multiset(pool: &sqlx::PgPool) -> HashMap<(String, String), usize> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT actor, action FROM audit_log ORDER BY id")
            .fetch_all(pool)
            .await
            .expect("select audit_log");
    let mut m = HashMap::new();
    for r in rows {
        *m.entry(r).or_insert(0_usize) += 1;
    }
    m
}

/// The stored `policy / guard_tier.boot` payload, as Postgres holds it.
///
/// Read back from the real table rather than from a sink double, so this
/// is the payload the daemon actually persisted — `db::audit::insert`
/// applies `truncate_payload` on the way in, and a double would assert
/// the row that was PASSED rather than the row that was STORED. (That
/// transform is inert for the two-key unconfigured payload this
/// currently reads; the reasoning is right for the class of assertion,
/// and reading from the table is what makes it stay right if the payload
/// ever grows.)
///
/// `fetch_all` rather than `fetch_one` because `fetch_one` returns the
/// FIRST row and errors only on zero — it does not reject duplicates —
/// so the uniqueness has to be asserted here to be asserted at all.
/// `ORDER BY id` keeps the failure message deterministic rather than
/// heap-ordered.
async fn guard_tier_boot_payload(pool: &sqlx::PgPool) -> serde_json::Value {
    let rows: Vec<(serde_json::Value,)> = sqlx::query_as(
        "SELECT payload FROM audit_log \
         WHERE actor = 'policy' AND action = 'guard_tier.boot' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("select guard_tier.boot rows");
    assert_eq!(
        rows.len(),
        1,
        "expected exactly one guard_tier.boot row (one per daemon start); got {}",
        rows.len()
    );
    rows.into_iter().next().expect("length asserted above").0
}

/// Build the per-test PG cluster + return the handle (with the
/// daemon-test-specific service-name infix).
fn cluster_for(suffix: &str) -> PgCluster {
    let bin_dir = pg_bin_dir_or_skip().expect("caller already short-circuited on missing pg");
    bring_up_pg_cluster(
        &bin_dir,
        "cli-d",
        "cli-l",
        &format!("kastellan-supervisor-test-pg-cliask-{suffix}"),
    )
}

// ---------------------------------------------------------------------------
// Test 1 — happy path
// ---------------------------------------------------------------------------

#[test]
fn ask_subprocess_completes_planned_task_end_to_end() {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    if skip_if_no_supervisor() {
        return;
    }
    if skip_if_sandbox_unavailable() {
        return;
    }
    if pg_bin_dir_or_skip().is_none() {
        return;
    }
    if skip_if_any_binary_missing() {
        return;
    }

    // Scope-end drop order is reverse declaration order, so the
    // bindings below resolve to:
    //   1. `_daemon_guards`  → stops + uninstalls the daemon service
    //   2. `mock`            → aborts the listener accept-task
    //   3. `cluster`         → stops PG, wipes data + log dirs
    let suffix = unique_suffix();
    let marker = format!("marker-{suffix}");
    let user = current_username();

    let cluster = cluster_for(&suffix);

    // worker_threads(1): `tool_host::dispatch` uses `block_in_place`,
    // which needs at least one spare worker. One is enough.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    // Each plan iteration: (1) embed request for recall, (2) chat
    // completion for plan generation. 2 iterations → 2 embed responses
    // and 2 chat responses. The mock routes by URL path so the two
    // queues are independent.
    let mock = rt.block_on(spawn_scripted_llm(
        vec![embedding_envelope(), embedding_envelope()],
        vec![
            envelope_for(&plan_json("act", echo_step(&marker), None)),
            envelope_for(&plan_json(
                "task_complete",
                serde_json::json!([]),
                Some(serde_json::json!({"kind": "text", "body": &marker})),
            )),
        ],
    ));

    // Apply migrations explicitly and seed the shell-exec allowlist
    // before the daemon boots — build_tool_registry reads the
    // allowlist from the DB now. The daemon's own probe will
    // idempotently re-apply migrations.
    rt.block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "test",
            "setup",
            serde_json::json!({"test": "cli_ask_e2e_setup"}),
        )
        .await
        .expect("probe run");
        let seed_pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("seed pool");
        seed_tool_allowlist(&seed_pool, "shell-exec", &[ECHO_PATH])
            .await
            .expect("seed shell-exec allowlist");
        drop(seed_pool);
    });

    let (daemon, _daemon_guards) =
        bring_up_daemon(&suffix, &cluster.data_dir, &mock.base_url, &user);

    // ---------- Spawn the real CLI subprocess ----------
    let output = Command::new(cli_binary())
        .arg("ask")
        .arg(format!("say {marker}"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("USER", &user)
        .env("KASTELLAN_DATA_DIR", cluster.data_dir.to_string_lossy().as_ref())
        .output()
        .expect("spawn kastellan-cli ask");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "CLI must exit 0 in the happy path; got {:?}\n\
         --- CLI stdout ---\n{}\n--- CLI stderr ---\n{}\n\
         --- daemon stdout ({}) ---\n{}\n\
         --- daemon stderr ({}) ---\n{}\n",
        output.status,
        stdout,
        stderr,
        daemon.stdout_path.display(),
        std::fs::read_to_string(&daemon.stdout_path).unwrap_or_default(),
        daemon.stderr_path.display(),
        std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
    );
    assert_eq!(
        stdout.trim_end(),
        marker,
        "CLI stdout must echo the marker verbatim; got:\n{stdout}\n--- stderr ---\n{stderr}\n"
    );

    // ---------- DB assertions ----------
    rt.block_on(async {
        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("connect runtime pool");

        let rows: Vec<(i64, String, i32, Option<serde_json::Value>)> =
            sqlx::query_as("SELECT id, state, plan_count, result FROM tasks ORDER BY id")
                .fetch_all(&pool)
                .await
                .expect("select tasks");
        assert_eq!(rows.len(), 1, "expected exactly one task row, got {rows:?}");
        let (_, state, plan_count, result) = &rows[0];
        assert_eq!(state, "completed", "task state must be 'completed'; got {state}");
        assert_eq!(*plan_count, 2, "expected plan_count == 2 (two LLM rounds); got {plan_count}");
        let result_body = result
            .as_ref()
            .and_then(|v| v.get("body"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(result_body, marker, "result.body must equal marker; got {result_body:?}");

        let m = audit_multiset(&pool).await;
        assert_eq!(m.get(&("core".into(), "startup".into())), Some(&1),
                   "expected 1× core/startup; multiset = {m:?}");
        assert_eq!(m.get(&("core".into(), "registry.loaded".into())), Some(&1),
                   "expected 1× core/registry.loaded (build_tool_registry summary row); multiset = {m:?}");
        // The guard tier reports itself once per boot whether or not it is
        // configured — here it is NOT, and that is the point: "the security
        // tier is off on this host" must be a query, not an inference from a
        // row that is absent for two different reasons.
        assert_eq!(m.get(&("policy".into(), "guard_tier.boot".into())), Some(&1),
                   "expected 1× policy/guard_tier.boot per daemon start; multiset = {m:?}");
        // ...and the row a real boot STORED equals what the shared pure
        // builder produces (#627). The count above says the daemon wrote
        // a row; this says it wrote THAT row, which is the seam
        // `boot_report`'s unit tests cannot see: they would all stay
        // green if `main.rs` went back to composing the payload inline
        // and the two copies drifted. The sibling test below asserts the
        // count only — the payload is a property of the boot, not of the
        // task, and pinning it twice would double the maintenance for no
        // extra coverage.
        //
        // Structural equality on two `serde_json::Value`s, not byte
        // equality: key order and whitespace are irrelevant here, and
        // JSONB would not round-trip key order anyway. Do not "strengthen"
        // this into a string comparison — it would fail for the wrong
        // reason.
        //
        // ⚠️ This pins the UNCONFIGURED arm only; both daemons in this
        // file boot without a guard tier. The configured arm — eleven
        // keys, the rates, the coverage finding — is pinned by nothing,
        // and issue #633 carries the recipe for closing it.
        assert_eq!(
            guard_tier_boot_payload(&pool).await,
            kastellan_core::cassandra::guard_model::boot_report::not_configured_payload(),
            "the daemon must record the shared unconfigured payload verbatim"
        );
        assert_eq!(m.get(&("cli".into(), "task.submitted".into())), Some(&1),
                   "expected 1× cli/task.submitted (producer-side row from kastellan-cli ask); multiset = {m:?}");
        assert_eq!(m.get(&("agent".into(), "plan.formulate".into())), Some(&2),
                   "expected 2× agent/plan.formulate (one per LLM call); multiset = {m:?}");
        // PgRecallBuilder calls embed_query once per plan iteration before
        // the chat-completion: 2 plan iterations → 2 embed audit rows.
        assert_eq!(m.get(&("llm:router".into(), "embed".into())), Some(&2),
                   "expected 2× llm:router/embed (one per recall+plan iteration); multiset = {m:?}");
        assert_eq!(m.get(&("cassandra:chain".into(), "verdict".into())), Some(&2),
                   "expected 2× cassandra:chain/verdict (one per plan); multiset = {m:?}");
        assert_eq!(m.get(&("tool:shell-exec".into(), "shell.exec".into())), Some(&1),
                   "expected 1× tool:shell-exec/shell.exec (the echo step); multiset = {m:?}");
        assert_eq!(m.get(&("scheduler".into(), "plan.outcome".into())), Some(&1),
                   "expected 1× scheduler/plan.outcome (only plan A executed steps); multiset = {m:?}");
        // Spec §7: one running-transition row + one terminal-state row
        // + one finalize summary row per task.
        assert_eq!(m.get(&("scheduler".into(), "task.running".into())), Some(&1),
                   "expected 1× scheduler/task.running (claim_one transition); multiset = {m:?}");
        assert_eq!(m.get(&("scheduler".into(), "task.completed".into())), Some(&1),
                   "expected 1× scheduler/task.completed (happy-path terminal); multiset = {m:?}");
        assert_eq!(m.get(&("scheduler".into(), "task.finalize".into())), Some(&1),
                   "expected 1× scheduler/task.finalize (per-task summary); multiset = {m:?}");

        let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log");
        // +1 for test/setup (pre-seed probe), +1 core/startup, +1 core/registry.loaded,
        // +1 policy/guard_tier.boot, +1 cli/task.submitted, +2 agent/plan.formulate,
        // +2 llm:router/embed (recall), +2 cassandra:chain/verdict,
        // +1 tool:shell-exec/shell.exec, +1 scheduler/plan.outcome,
        // +1 scheduler/task.running, +1 scheduler/task.completed,
        // +1 scheduler/task.finalize
        //
        // `policy/guard_tier.boot` is written once per daemon start whether or
        // not a guard is configured (here: not), because "the tier is off" is
        // exactly the fact that must be queryable rather than inferred from an
        // absent row. This exact-multiset assertion is what caught it being
        // added — keep it exact.
        let expected_total: i64 = 1 + 1 + 1 + 1 + 1 + 2 + 2 + 2 + 1 + 1 + 1 + 1 + 1; // = 16
        assert_eq!(
            total.0, expected_total,
            "audit_log row count mismatch (expected {expected_total}, got {}); multiset = {m:?}",
            total.0
        );

        // Spec §7 finalize payload spot-checks.
        let finalize_payload: (sqlx::types::Json<serde_json::Value>,) = sqlx::query_as(
            "SELECT payload FROM audit_log \
             WHERE actor = 'scheduler' AND action = 'task.finalize' LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .expect("select task.finalize row");
        let fp = &finalize_payload.0.0;
        assert_eq!(fp["state"], "completed",
                   "task.finalize.state should be 'completed'; got {fp:?}");
        assert_eq!(fp["plan_count"], 2,
                   "task.finalize.plan_count should be 2 (one non-terminal + one terminal plan); got {fp:?}");
        assert_eq!(fp["total_llm_calls"], 2,
                   "task.finalize.total_llm_calls should be 2; got {fp:?}");
        assert_eq!(fp["total_dispatch_calls"], 1,
                   "task.finalize.total_dispatch_calls should be 1 (single echo step under plan A); got {fp:?}");
        assert!(fp["total_duration_ms"].is_number(),
                "task.finalize.total_duration_ms must be a number; got {fp:?}");
        assert!(fp["started_at"].is_string(),
                "task.finalize.started_at must be an RFC 3339 string; got {fp:?}");
        assert!(fp["finished_at"].is_string(),
                "task.finalize.finished_at must be an RFC 3339 string; got {fp:?}");

        // Slice — automatic floor inference (2026-05-16): every
        // agent/plan.formulate row carries `classification_floor_source`.
        // Happy-path instruction is the marker string ("EXEC_E2E_HAPPY_…");
        // no clinical / secret / personal keywords match the catalogue, so
        // the inference returns Public + empty signals → source=Default
        // and the `classification_floor_signals` key is omitted.
        let plan_rows: Vec<(sqlx::types::Json<serde_json::Value>,)> = sqlx::query_as(
            "SELECT payload FROM audit_log \
             WHERE actor = 'agent' AND action = 'plan.formulate' \
             ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("select plan.formulate rows");
        assert_eq!(plan_rows.len(), 2, "expected 2 plan.formulate rows");
        for (i, row) in plan_rows.iter().enumerate() {
            let p = &row.0.0;
            let src = p.get("classification_floor_source")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!(
                    "plan.formulate row {i} must carry classification_floor_source; got {p:?}"));
            assert_eq!(src, "default",
                "plan.formulate row {i}: expected source=default for non-clinical 'marker' prompt; got {src:?}");
            assert!(p.get("classification_floor_signals").is_none(),
                "plan.formulate row {i}: default source must omit signals key; got {p:?}");
            // Slice C (prompt assembler, 2026-05-16): the three new
            // keys are present on every plan.formulate row. The exact
            // SHA varies across runs because the assembled prompt
            // includes the L0 starter rules; just assert presence + shape.
            assert!(p.get("system_prompt_sha256")
                .and_then(|v| v.as_str())
                .map(|s| s.len() == 64)
                .unwrap_or(false),
                "plan.formulate row {i} must carry system_prompt_sha256 as a 64-char hex string; got {p:?}");
            assert!(p.get("l0_count").and_then(|v| v.as_u64()).is_some(),
                "plan.formulate row {i} must carry numeric l0_count; got {p:?}");
            assert!(p.get("l1_count").and_then(|v| v.as_u64()).is_some(),
                "plan.formulate row {i} must carry numeric l1_count; got {p:?}");
            assert!(p.get("recall_count").and_then(|v| v.as_u64()).is_some(),
                "plan.formulate row {i} must carry numeric recall_count; got {p:?}");
            assert!(p.get("recalled_memory_ids").and_then(|v| v.as_array()).is_some(),
                "plan.formulate row {i} must carry array recalled_memory_ids; got {p:?}");
            let sha = p.get("recall_query_sha256").and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("plan.formulate row {i} must carry recall_query_sha256; got {p:?}"));
            assert_eq!(sha.len(), 64,
                "plan.formulate row {i}: recall_query_sha256 must be 64 hex chars; got {sha}");
        }

        pool.close().await;
    });

    // Mock dial count: 2 embeds + 2 chat-completions (one of each per
    // plan iteration). With URL routing each endpoint's count is
    // asserted independently — an unexpected extra dial to one side
    // does not silently inflate the other side's total.
    let embed_dialed = mock.embed_requests.lock().unwrap().len();
    let chat_dialed = mock.chat_requests.lock().unwrap().len();
    assert_eq!(
        embed_dialed, 2,
        "expected daemon to dial mock embed endpoint exactly 2× in happy path; got {embed_dialed}",
    );
    assert_eq!(
        chat_dialed, 2,
        "expected daemon to dial mock chat endpoint exactly 2× in happy path; got {chat_dialed}",
    );
    // First chat-completion must carry the cached `agent_planner`
    // prompt. With URL routing this is `chat_requests[0]`, regardless
    // of how the embed dials interleave.
    let chat_captured = mock.chat_requests.lock().unwrap();
    let first_chat_body = &chat_captured[0];
    assert!(
        first_chat_body.contains("Constitutional Principles"),
        "first chat request must carry the cached planner prompt; got first {} chars:\n{}",
        first_chat_body.len().min(800),
        &first_chat_body[..first_chat_body.len().min(800)],
    );
}

// ---------------------------------------------------------------------------
// Test 2 — plan-iteration-cap failure path
// ---------------------------------------------------------------------------

#[test]
fn ask_subprocess_fails_after_plan_iteration_cap() {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    if skip_if_no_supervisor() {
        return;
    }
    if skip_if_sandbox_unavailable() {
        return;
    }
    if pg_bin_dir_or_skip().is_none() {
        return;
    }
    if skip_if_any_binary_missing() {
        return;
    }

    let suffix = unique_suffix();
    let user = current_username();

    let cluster = cluster_for(&suffix);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    // Same non-terminal plan five times — every iteration the agent
    // tries to cat /etc/passwd, every iteration the worker denies it,
    // every iteration the inner-loop replans. On the sixth would-be
    // iteration the plan-iter cap kicks in (DEFAULT_MAX_PLANS_FAST=5)
    // and we return Outcome::Failed.
    //
    // Each plan iteration: (1) embed request for recall, (2) chat
    // completion for plan generation. 5 iterations → 5 embed responses
    // and 5 chat responses (the same denied plan repeated).
    let denied_plan = envelope_for(&plan_json("act", cat_passwd_step(), None));
    let mock = rt.block_on(spawn_scripted_llm(
        vec![
            embedding_envelope(),
            embedding_envelope(),
            embedding_envelope(),
            embedding_envelope(),
            embedding_envelope(),
        ],
        vec![
            denied_plan.clone(),
            denied_plan.clone(),
            denied_plan.clone(),
            denied_plan.clone(),
            denied_plan,
        ],
    ));

    // Apply migrations before the daemon boots so build_tool_registry
    // can connect. No allowlist seeding: every shell.exec call must
    // surface POLICY_DENIED, which is the failure-path's assertion
    // target.
    rt.block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "test",
            "setup",
            serde_json::json!({"test": "cli_ask_e2e_setup"}),
        )
        .await
        .expect("probe run");
    });

    let (daemon, _daemon_guards) =
        bring_up_daemon(&suffix, &cluster.data_dir, &mock.base_url, &user);

    let output = Command::new(cli_binary())
        .arg("ask")
        .arg("do the bad thing")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("USER", &user)
        .env("KASTELLAN_DATA_DIR", cluster.data_dir.to_string_lossy().as_ref())
        .output()
        .expect("spawn kastellan-cli ask");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        !output.status.success(),
        "CLI must exit non-zero in the failure path; got {:?}\n\
         --- CLI stdout ---\n{}\n--- CLI stderr ---\n{}\n\
         --- daemon stdout ({}) ---\n{}\n\
         --- daemon stderr ({}) ---\n{}\n",
        output.status,
        stdout,
        stderr,
        daemon.stdout_path.display(),
        std::fs::read_to_string(&daemon.stdout_path).unwrap_or_default(),
        daemon.stderr_path.display(),
        std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
    );
    assert!(
        stderr.contains("failed"),
        "CLI stderr must mention 'failed'; got:\n{stderr}"
    );

    rt.block_on(async {
        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("connect runtime pool");

        let rows: Vec<(i64, String, i32, Option<serde_json::Value>)> =
            sqlx::query_as("SELECT id, state, plan_count, result FROM tasks ORDER BY id")
                .fetch_all(&pool)
                .await
                .expect("select tasks");
        assert_eq!(rows.len(), 1, "expected exactly one task row, got {rows:?}");
        let (_, state, plan_count, _) = &rows[0];
        assert_eq!(state, "failed", "task state must be 'failed'; got {state}");
        assert_eq!(*plan_count, 5, "plan_count must equal cap (5); got {plan_count}");

        // Each of the five iterations dispatched the denied step, so
        // we expect 5 tool:shell-exec rows whose payload carries an
        // `err` string mentioning the JSON-RPC POLICY_DENIED code
        // (`-32001`).
        let denied_rows: Vec<(serde_json::Value,)> = sqlx::query_as(
            "SELECT payload FROM audit_log \
             WHERE actor = 'tool:shell-exec' AND action = 'shell.exec' \
             ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("select shell-exec audit rows");
        assert_eq!(
            denied_rows.len(),
            5,
            "expected 5 tool:shell-exec rows (one per iter); got {}",
            denied_rows.len()
        );
        for (i, (payload,)) in denied_rows.iter().enumerate() {
            let err_str = payload.get("err").and_then(|e| e.as_str()).unwrap_or("");
            assert!(
                err_str.contains("-32001"),
                "iter {i}: expected err string to carry POLICY_DENIED code -32001; \
                 got err={err_str:?}; payload={payload}"
            );
            assert!(
                !payload
                    .as_object()
                    .map(|o| o.contains_key("result"))
                    .unwrap_or(true),
                "iter {i}: denied row must not carry a `result` key; payload={payload}"
            );
        }

        let m = audit_multiset(&pool).await;
        assert_eq!(m.get(&("core".into(), "startup".into())), Some(&1),
                   "expected 1× core/startup; multiset = {m:?}");
        assert_eq!(m.get(&("core".into(), "registry.loaded".into())), Some(&1),
                   "expected 1× core/registry.loaded (build_tool_registry summary row); multiset = {m:?}");
        // The guard tier reports itself once per boot whether or not it is
        // configured — here it is NOT, and that is the point: "the security
        // tier is off on this host" must be a query, not an inference from a
        // row that is absent for two different reasons.
        assert_eq!(m.get(&("policy".into(), "guard_tier.boot".into())), Some(&1),
                   "expected 1× policy/guard_tier.boot per daemon start; multiset = {m:?}");
        assert_eq!(m.get(&("cli".into(), "task.submitted".into())), Some(&1),
                   "expected 1× cli/task.submitted (producer-side row from kastellan-cli ask); multiset = {m:?}");
        assert_eq!(m.get(&("agent".into(), "plan.formulate".into())), Some(&5),
                   "expected 5× agent/plan.formulate (one per LLM call before cap); multiset = {m:?}");
        // PgRecallBuilder calls embed_query once per plan iteration before
        // the chat-completion: 5 plan iterations → 5 embed audit rows.
        assert_eq!(m.get(&("llm:router".into(), "embed".into())), Some(&5),
                   "expected 5× llm:router/embed (one per recall+plan iteration); multiset = {m:?}");
        assert_eq!(m.get(&("cassandra:chain".into(), "verdict".into())), Some(&5),
                   "expected 5× cassandra:chain/verdict (one per plan); multiset = {m:?}");
        assert_eq!(m.get(&("tool:shell-exec".into(), "shell.exec".into())), Some(&5),
                   "expected 5× tool:shell-exec/shell.exec (one per denied dispatch); multiset = {m:?}");
        assert_eq!(m.get(&("scheduler".into(), "plan.outcome".into())), Some(&5),
                   "expected 5× scheduler/plan.outcome (one per non-terminal plan); multiset = {m:?}");
        assert_eq!(m.get(&("scheduler".into(), "task.running".into())), Some(&1),
                   "expected 1× scheduler/task.running (claim_one transition); multiset = {m:?}");
        assert_eq!(m.get(&("scheduler".into(), "task.failed".into())), Some(&1),
                   "expected 1× scheduler/task.failed (plan-cap exhaustion terminal); multiset = {m:?}");
        assert_eq!(m.get(&("scheduler".into(), "task.finalize".into())), Some(&1),
                   "expected 1× scheduler/task.finalize (per-task summary); multiset = {m:?}");

        let total: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log");
        // +1 test/setup (pre-seed probe), +1 core/startup, +1 core/registry.loaded,
        // +1 policy/guard_tier.boot (once per daemon start, configured or not),
        // +1 cli/task.submitted, +5 agent/plan.formulate, +5 llm:router/embed (recall),
        // +5 cassandra:chain/verdict, +5 tool:shell-exec/shell.exec,
        // +5 scheduler/plan.outcome, +1 scheduler/task.running,
        // +1 scheduler/task.failed, +1 scheduler/task.finalize
        let expected_total: i64 = 1 + 1 + 1 + 1 + 1 + 5 + 5 + 5 + 5 + 5 + 1 + 1 + 1; // = 33
        assert_eq!(
            total.0, expected_total,
            "audit_log row count mismatch (expected {expected_total}, got {}); multiset = {m:?}",
            total.0
        );

        let finalize_payload: (sqlx::types::Json<serde_json::Value>,) = sqlx::query_as(
            "SELECT payload FROM audit_log \
             WHERE actor = 'scheduler' AND action = 'task.finalize' LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .expect("select task.finalize row");
        let fp = &finalize_payload.0.0;
        assert_eq!(fp["state"], "failed",
                   "task.finalize.state should be 'failed'; got {fp:?}");
        assert_eq!(fp["plan_count"], 5,
                   "task.finalize.plan_count should equal cap (5); got {fp:?}");
        assert_eq!(fp["total_llm_calls"], 5,
                   "task.finalize.total_llm_calls should be 5; got {fp:?}");
        assert_eq!(fp["total_dispatch_calls"], 5,
                   "task.finalize.total_dispatch_calls should be 5 (one per denied iter); got {fp:?}");

        // Slice D (recall lane, 2026-05-17): every plan.formulate row
        // must carry the three recall keys produced by PgRecallBuilder.
        // The `memories` table is empty in this test (we never seed it),
        // so both the semantic and lexical lanes return 0 rows regardless
        // of what embedding the mock returns — hence recall_count=0 and
        // recalled_memory_ids=[]. recall_query_sha256 is always the
        // SHA-256 of the task instruction, computed before the recall
        // fan-out, so it's present and 64 hex chars even on the empty path.
        let plan_rows: Vec<(sqlx::types::Json<serde_json::Value>,)> = sqlx::query_as(
            "SELECT payload FROM audit_log \
             WHERE actor = 'agent' AND action = 'plan.formulate' \
             ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("select plan.formulate rows (cap-exhaustion path)");
        assert_eq!(plan_rows.len(), 5, "expected 5 plan.formulate rows (cap-exhaustion path)");
        for (i, row) in plan_rows.iter().enumerate() {
            let p = &row.0.0;
            assert!(p.get("recall_count").and_then(|v| v.as_u64()).is_some(),
                "plan.formulate row {i} must carry numeric recall_count; got {p:?}");
            assert!(p.get("recalled_memory_ids").and_then(|v| v.as_array()).is_some(),
                "plan.formulate row {i} must carry array recalled_memory_ids; got {p:?}");
            let sha = p.get("recall_query_sha256").and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("plan.formulate row {i} must carry recall_query_sha256; got {p:?}"));
            assert_eq!(sha.len(), 64,
                "plan.formulate row {i}: recall_query_sha256 must be 64 hex chars; got {sha}");
        }

        pool.close().await;
    });

    // Mock dial count: 5 embeds + 5 chat-completions, one of each per
    // plan iteration before the cap fires. Per-endpoint assertions
    // catch a stray extra dial to either side directly.
    let embed_dialed = mock.embed_requests.lock().unwrap().len();
    let chat_dialed = mock.chat_requests.lock().unwrap().len();
    assert_eq!(
        embed_dialed, 5,
        "expected daemon to dial mock embed endpoint exactly 5× before cap; got {embed_dialed}",
    );
    assert_eq!(
        chat_dialed, 5,
        "expected daemon to dial mock chat endpoint exactly 5× before cap; got {chat_dialed}",
    );
}
