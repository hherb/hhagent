//! End-to-end: the real `kastellan` daemon registers, advertises, and dispatches
//! `mail.*` — the daemon/planner leg. The mail worker runs under the real
//! sandbox against a plain-HTTP `mock_localmail`. Force-routing is off
//! (`KASTELLAN_EGRESS_FORCE_ROUTING=0`) so the daemon-spawned worker takes the
//! DIRECT path to the plain-HTTP mock (the force-routed path can't reach a
//! plain-HTTP/self-signed origin — the webpki wall, covered structurally by
//! `mail_e2e`'s tier 1b).
//!
//! - `daemon_planner_dispatches_mail_search_end_to_end` (always-on): a SCRIPTED
//!   planner plan calls `mail.search`; asserts registration + `<tools>`
//!   advertisement + dispatch + completion.
//! - `live_llm_selects_mail_unprompted` (`#[ignore]`): a REAL local LLM given a
//!   mail-ish question must reach for `mail.*` on its own.
//! - `mock_localmail_shapes_match_real_localmail` (`#[ignore]`, Mac-only): pins
//!   the mock against real localmail (closes the #487 drift failure mode).
//!
//! Skips as-pass without PG / supervisor / sandbox / the mail+cli+core binaries.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::process::Command;

use kastellan_tests_common::daemon::{bring_up_daemon, DaemonGuards, DaemonHandle};
use kastellan_tests_common::mock_localmail::{spawn_mock_localmail, MockLocalmail};
use kastellan_tests_common::scripted_llm::{
    embedding_envelope, envelope_for, plan_json, spawn_scripted_llm,
};
use kastellan_tests_common::{
    bring_up_pg_cluster, cli_binary, core_binary, current_username, pg_bin_dir_or_skip,
    skip_if_no_supervisor, skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary,
    PgCluster,
};

/// The single plan step that calls the mail tool.
fn mail_search_step() -> serde_json::Value {
    serde_json::json!([{
        "tool":           "mail",
        "method":         "mail.search",
        "parameters":     {"query": "invoice"},
        "returns":        "results",
        "done_when":      "true",
        "classification": "Public",
    }])
}

/// `true` (caller should `return`) when any prerequisite binary / host facility
/// is missing — the shared skip-as-pass guard for both daemon tiers.
fn skip_prereqs() -> bool {
    for (label, p) in &[
        ("kastellan", core_binary()),
        ("kastellan-cli", cli_binary()),
        ("kastellan-worker-mail", workspace_target_binary("kastellan-worker-mail")),
    ] {
        if !p.exists() {
            eprintln!("\n[SKIP] {label} binary missing at {}; cargo build --workspace\n", p.display());
            return true;
        }
    }
    skip_if_no_supervisor() || skip_if_sandbox_unavailable() || pg_bin_dir_or_skip().is_none()
}

/// The live pieces a mail-daemon tier needs kept alive for its whole run. Field
/// order matters for drop (Rust drops fields top-to-bottom): the daemon guards
/// (stop + uninstall the service) drop BEFORE `cluster` (stops PG), so the
/// daemon tears down while its database is still up.
struct MailDaemon {
    daemon: DaemonHandle,
    _guards: DaemonGuards,
    _mock_mail: MockLocalmail,
    _token_dir: tempfile::TempDir,
    cluster: PgCluster,
}

/// Bring up the per-test PG cluster + a plain-HTTP mock localmail + the real
/// daemon with the mail worker registered (endpoint = mock, 0600 token file,
/// binary path) and force-routing OFF. `llm_base_url` is the OpenAI-compatible
/// endpoint the daemon's router dials (`bring_up_daemon` appends `/v1`);
/// `model_override` replaces the harness default `test-local-model` when `Some`.
fn bring_up_mail_daemon(
    rt: &tokio::runtime::Runtime,
    suffix: &str,
    user: &str,
    llm_base_url: &str,
    model_override: Option<&str>,
) -> MailDaemon {
    let cluster = bring_up_pg_cluster(
        &pg_bin_dir_or_skip().unwrap(),
        "maild-d",
        "maild-l",
        &format!("kastellan-supervisor-test-pg-maild-{suffix}"),
    );

    let mock_mail = rt.block_on(spawn_mock_localmail());

    // Migrations before the daemon boots (its own probe re-applies idempotently).
    // Mail derives its allowlist from the endpoint env, so there is NO
    // tool_allowlists seed (unlike shell-exec).
    rt.block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "test",
            "setup",
            serde_json::json!({"test": "mail_daemon_e2e_setup"}),
        )
        .await
        .expect("probe run");
    });

    // 0600 token file; its dir must outlive the daemon.
    let token_dir = tempfile::tempdir().unwrap();
    let token_file = token_dir.path().join("mail-token");
    std::fs::write(&token_file, b"test-bearer-token").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))
            .expect("chmod token 0600");
    }

    let mut extra_env = vec![
        ("KASTELLAN_MAIL_ENDPOINT".to_string(), mock_mail.base_url.clone()),
        ("KASTELLAN_MAIL_TOKEN_FILE".to_string(), token_file.to_string_lossy().into_owned()),
        (
            "KASTELLAN_MAIL_BIN".to_string(),
            workspace_target_binary("kastellan-worker-mail").to_string_lossy().into_owned(),
        ),
        ("KASTELLAN_EGRESS_FORCE_ROUTING".to_string(), "0".to_string()),
    ];
    // Override the harness's hard-coded `test-local-model` for a live LLM.
    // `bring_up_daemon` pushes the default first, then extends with extra_env, so
    // this later entry wins when the supervisor materialises the process env.
    if let Some(model) = model_override {
        extra_env.push(("KASTELLAN_LLM_LOCAL_MODEL".to_string(), model.to_string()));
    }

    // bring_up_daemon sets KASTELLAN_DATA_DIR from the cluster's data_dir.
    let (daemon, guards) =
        bring_up_daemon("maild", suffix, &cluster.data_dir, llm_base_url, user, extra_env);

    MailDaemon {
        cluster,
        daemon,
        _guards: guards,
        _mock_mail: mock_mail,
        _token_dir: token_dir,
    }
}

/// Run `kastellan-cli ask <question>` against the daemon's cluster. `env_clear`
/// plus only the data-dir env: the operator CLI deliberately omits worker
/// registration (the #179 invariant) — only the daemon registers mail.
fn cli_ask(fixture: &MailDaemon, user: &str, question: &str) -> std::process::Output {
    Command::new(cli_binary())
        .arg("ask")
        .arg(question)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("USER", user)
        .env("KASTELLAN_DATA_DIR", fixture.cluster.data_dir.to_string_lossy().as_ref())
        .output()
        .expect("spawn kastellan-cli ask")
}

/// Panic with the daemon logs attached if the CLI did not exit 0.
fn assert_cli_ok(fixture: &MailDaemon, output: &std::process::Output) {
    assert!(
        output.status.success(),
        "CLI must exit 0; got {:?}\n--- cli stderr ---\n{}\n--- daemon stdout ({}) ---\n{}\n--- daemon stderr ({}) ---\n{}\n",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        fixture.daemon.stdout_path.display(),
        std::fs::read_to_string(&fixture.daemon.stdout_path).unwrap_or_default(),
        fixture.daemon.stderr_path.display(),
        std::fs::read_to_string(&fixture.daemon.stderr_path).unwrap_or_default(),
    );
}

async fn count_audit(pool: &sqlx::PgPool, actor: &str, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE actor = $1 AND action = $2")
        .bind(actor)
        .bind(action)
        .fetch_one(pool)
        .await
        .expect("count audit rows")
}

/// Tier 2a — a SCRIPTED planner plan calls `mail.search`; the daemon registers +
/// advertises + dispatches it and the task completes.
#[test]
fn daemon_planner_dispatches_mail_search_end_to_end() {
    if skip_prereqs() {
        return;
    }
    let suffix = unique_suffix();
    let user = current_username();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    // Same 2-iteration shape as cli_ask_e2e's happy path: one embed + one plan
    // per iteration; plan A executes the mail.search step (result=None), plan B
    // is terminal.
    let scripted = rt.block_on(spawn_scripted_llm(
        vec![embedding_envelope(), embedding_envelope()],
        vec![
            envelope_for(&plan_json("act", mail_search_step(), None)),
            envelope_for(&plan_json(
                "task_complete",
                serde_json::json!([]),
                Some(serde_json::json!({"kind": "text", "body": "done"})),
            )),
        ],
    ));

    let fixture = bring_up_mail_daemon(&rt, &suffix, &user, &scripted.base_url, None);
    let output = cli_ask(&fixture, &user, "find my latest invoice email");
    assert_cli_ok(&fixture, &output);

    rt.block_on(async {
        let pool = kastellan_db::pool::connect_runtime_pool(&fixture.cluster.conn_spec)
            .await
            .expect("connect runtime pool");

        let (state, plan_count): (String, i32) =
            sqlx::query_as("SELECT state, plan_count FROM tasks ORDER BY id LIMIT 1")
                .fetch_one(&pool)
                .await
                .expect("select task");
        assert_eq!(state, "completed", "task must complete; got {state}");
        assert_eq!(plan_count, 2, "expected 2 plan rounds; got {plan_count}");

        // The planner advertised mail in its <tools> block.
        let chat0 = scripted.chat_requests.lock().unwrap().first().cloned().unwrap_or_default();
        assert!(
            chat0.contains("mail.search"),
            "planner <tools> block must advertise mail.search; first chat request:\n{chat0}"
        );

        // mail.search actually dispatched — audit (actor,action) =
        // ("tool:mail","mail.search"), the shape cli_ask asserts for shell-exec.
        assert_eq!(count_audit(&pool, "tool:mail", "mail.search").await, 1,
                   "expected exactly one tool:mail/mail.search dispatch row");
        assert_eq!(count_audit(&pool, "agent", "plan.formulate").await, 2,
                   "expected 2 agent/plan.formulate rows");

        pool.close().await;
    });
}

/// Tier 2b — a REAL local LLM, given a mail-ish question, must select `mail.*`
/// unprompted (proves the tool docs are good enough for real model selection).
/// Portable: the mock origin needs no localmail. Point the daemon at a local
/// OpenAI-compatible endpoint via `KASTELLAN_MAIL_LIVE_LLM_URL`
/// (e.g. `http://127.0.0.1:11434/v1` for Ollama) + `KASTELLAN_MAIL_LIVE_LLM_MODEL`.
#[test]
#[ignore = "needs a real local LLM (KASTELLAN_MAIL_LIVE_LLM_URL); non-deterministic"]
fn live_llm_selects_mail_unprompted() {
    let Ok(llm_url) = std::env::var("KASTELLAN_MAIL_LIVE_LLM_URL") else {
        eprintln!("\n[SKIP] set KASTELLAN_MAIL_LIVE_LLM_URL to a local OpenAI-compatible endpoint\n");
        return;
    };
    if skip_prereqs() {
        return;
    }
    // bring_up_daemon appends /v1; strip a trailing /v1 the operator may include.
    let base = llm_url.strip_suffix("/v1").unwrap_or(&llm_url).to_string();
    let model = std::env::var("KASTELLAN_MAIL_LIVE_LLM_MODEL")
        .unwrap_or_else(|_| "gemma3".to_string());

    let suffix = unique_suffix();
    let user = current_username();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    let fixture = bring_up_mail_daemon(&rt, &suffix, &user, &base, Some(&model));
    let output = cli_ask(
        &fixture,
        &user,
        "search my email for the invoice from north coast health service",
    );
    assert_cli_ok(&fixture, &output);

    rt.block_on(async {
        let pool = kastellan_db::pool::connect_runtime_pool(&fixture.cluster.conn_spec)
            .await
            .expect("connect runtime pool");
        // The model reached for SOME mail.* tool on its own (actor = "tool:mail",
        // any method). We assert reach, not wording.
        let dispatched: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE actor = 'tool:mail'")
                .fetch_one(&pool)
                .await
                .expect("count mail dispatch rows");
        assert!(dispatched >= 1, "the real planner must reach for mail.* unprompted (0 dispatches)");
        pool.close().await;
    });
}

/// Mac-only fidelity gate: assert real localmail's `/v1` response SHAPES still
/// match `tests-common::mock_localmail`, so the hermetic mock cannot silently
/// drift (the #487 failure mode: mock served `hits`/`text-plain` while reality
/// served `results`/JSON, masking a real decode bug). Uses `curl -k` because the
/// dev-Mac localmail is HTTPS self-signed and the worker's transport is
/// webpki-only (that TLS path is NOT what this test checks). Run with `--ignored`
/// on the Mac; skips as-pass without the endpoint + token env.
#[test]
#[ignore = "needs real localmail (KASTELLAN_MAIL_ENDPOINT + KASTELLAN_MAIL_TOKEN); Mac-only"]
fn mock_localmail_shapes_match_real_localmail() {
    let (Ok(endpoint), Ok(token)) = (
        std::env::var("KASTELLAN_MAIL_ENDPOINT"),
        std::env::var("KASTELLAN_MAIL_TOKEN"),
    ) else {
        eprintln!("\n[SKIP] set KASTELLAN_MAIL_ENDPOINT + KASTELLAN_MAIL_TOKEN to the live localmail\n");
        return;
    };

    // `curl -k` a path; return (lowercased response headers, parsed-JSON-or-none).
    let curl = |method: &str, path: &str, body: Option<&str>| -> (String, Option<serde_json::Value>) {
        let mut cmd = Command::new("curl");
        cmd.args([
            "-sk", "-D", "-",
            "-X", method,
            "-H", &format!("Authorization: Bearer {token}"),
            "-H", "Content-Type: application/json",
        ]);
        if let Some(b) = body {
            cmd.args(["--data", b]);
        }
        cmd.arg(format!("{endpoint}{path}"));
        let out = cmd.output().expect("curl");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        match text.split_once("\r\n\r\n") {
            Some((h, b)) => (h.to_lowercase(), serde_json::from_str(b).ok()),
            None => (text.to_lowercase(), None),
        }
    };

    // 1. search → object with a `results` array (NOT `hits`).
    let (_h, search) = curl("POST", "/v1/search", Some("{\"query\":\"invoice\"}"));
    let search = search.expect("real localmail search must return JSON");
    assert!(
        search.get("results").map(|r| r.is_array()).unwrap_or(false),
        "real localmail search must key hits under `results`: {search}"
    );
    assert!(search.get("hits").is_none(), "real localmail must NOT use `hits` (the #487 drift)");

    // 2. accounts → JSON array.
    let (_h, accounts) = curl("GET", "/v1/accounts", None);
    assert!(
        accounts.expect("accounts JSON").is_array(),
        "real localmail /v1/accounts must be a JSON array"
    );

    // 3. attachment text → application/json {"text": …} (NOT text/plain). Find a
    //    real attachment sha via list → message; skip this leg (printed note) if
    //    the archive carries no attachment.
    let (_h, list) = curl("GET", "/v1/messages?limit=50", None);
    // list_messages must ALSO key rows under `results` (same drift surface as
    // search). get_message's shape is exercised implicitly by the sha discovery
    // below; get_attachment (raw bytes) is structurally simple and not JSON.
    assert!(
        list.as_ref().and_then(|v| v.get("results")).map(|r| r.is_array()).unwrap_or(false),
        "real localmail /v1/messages must key rows under `results`: {list:?}"
    );
    let mut sha: Option<String> = None;
    // Checked once, on the first detail actually fetched (see below).
    let mut detail_shape_checked = false;
    if let Some(rows) = list.as_ref().and_then(|v| v.get("results")).and_then(|r| r.as_array()) {
        for row in rows {
            let Some(id) = row.get("message_id").or_else(|| row.get("id")).and_then(|v| v.as_i64())
            else {
                continue;
            };
            let (_h, msg) = curl("GET", &format!("/v1/messages/{id}"), None);

            // 3a. get_message's own field shape. Previously this loop used the
            // detail response only to discover an attachment sha, so the
            // message-detail fields were the ONE surface this anti-drift gate
            // did not pin — which is exactly how `mock_localmail` was able to
            // drift into a mail-tool-only shape (`"from"` as a bare string,
            // `"body"` instead of `"body_text"`) that `workers/email-in`
            // cannot parse at all. That drift is silent by construction:
            // `build_event` reads `from.address`, gets `None`, and records the
            // message as `skipped` rather than erroring. Both asserts below
            // fail loudly on exactly that shape — indexing a JSON string with
            // `["address"]` yields `Null`, so `is_string()` is `false`.
            if let Some(msg) = msg.as_ref() {
                if !detail_shape_checked {
                    detail_shape_checked = true;
                    assert!(
                        msg["from"]["address"].is_string(),
                        "real localmail /v1/messages/{{id}} must serve `from` as an ADDRESS \
                         OBJECT (`_address()` → {{address, name}}), not a bare string — \
                         email-in reads `from.address`; got from = {}",
                        msg["from"]
                    );
                    assert!(
                        msg.get("body_text").is_some(),
                        "real localmail /v1/messages/{{id}} must name the plain-text body \
                         `body_text` (not `body`); got keys {:?}",
                        msg.as_object().map(|o| o.keys().collect::<Vec<_>>())
                    );
                }
            }

            if let Some(s) = msg
                .as_ref()
                .and_then(|m| m.get("attachments"))
                .and_then(|a| a.as_array())
                .and_then(|atts| atts.iter().find_map(|a| a.get("sha256").and_then(|s| s.as_str())))
            {
                sha = Some(s.to_string());
                break;
            }
        }
    }
    let Some(sha) = sha else {
        eprintln!("[NOTE] no attachment in the archive; skipping the attachment-text shape check");
        return;
    };
    let (head, text) = curl("GET", &format!("/v1/attachments/{sha}/text"), None);
    assert!(
        head.contains("application/json"),
        "attachment text must be application/json (the #487 contract), headers:\n{head}"
    );
    assert!(
        text.and_then(|v| v.get("text").map(|t| t.is_string())).unwrap_or(false),
        "attachment text must be a JSON {{\"text\": …}} envelope"
    );
}
