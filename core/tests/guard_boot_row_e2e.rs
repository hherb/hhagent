//! The seam pin for the **configured** `policy / guard_tier.boot` row
//! (issue [#633]).
//!
//! # What this closes
//!
//! `report_guard_tier` in `core/src/main.rs` has two arms, and after
//! [#627] only one of them was pinned end to end. `cli_ask_e2e` reads
//! the row a real daemon boot stored in real Postgres and asserts it
//! equals `boot_report::not_configured_payload()` — but both daemons in
//! that file boot *without* a guard tier, so the configured arm (eleven
//! keys, both rates, both counts, the coverage finding) was reachable by
//! no test in the tree. `guard_tier_e2e` never reads `audit_log` for
//! this action at all, and `report_guard_tier` is private to a binary
//! crate, so no unit test can call either arm.
//!
//! The worst case that gap allowed is not a transposition. It is
//! **deleting the `record(...)` call outright**: a configured host would
//! then write no boot row, and "the security tier is off on this host"
//! would revert to an inference from an absent row — which is precisely
//! the inference `not_configured_payload` exists to make unnecessary.
//!
//! # Why this needs no live guard endpoint
//!
//! [#627]'s PR body and the handover both said the configured arm
//! "needs a live guard endpoint, so only `guard_tier_e2e` can reach it".
//! That was wrong, and the wrong version is why the gap was written down
//! as unclosable rather than closed. `GuardTier::from_router_config`
//! needs the three guard env vars, **one** `/props` response, and a
//! timeout — and step 4 skips the probe *entirely* when the operator
//! pins one:
//!
//! ```text
//! let timeout = match cfg.guard_timeout_ms {
//!     Some(ms) => timeout::validate_operator_timeout(ms)?,   // no probe at all
//!     None     => { let summary = probe::run_probe(...).await; ... }
//! };
//! ```
//!
//! So a configured boot needs a mock that answers `/props` and nothing
//! else, and the row that results is **fully deterministic** — no
//! timing, no throughput, no flake. That is why this test asserts an
//! exact payload rather than a shape.
//!
//! # Why no task is submitted
//!
//! The boot row is written during bring-up, before the scheduler is
//! spawned, so a task would add several minutes of planner and worker
//! traffic to a test that reads one row. Skipping it also removes every
//! source of nondeterminism this file would otherwise inherit: no
//! dispatch means no adjudication, so the guard mock's chat queue can
//! stay empty and *be asserted empty*. (`guard_tier_e2e` already pins
//! "a pin costs no model traffic" at the tier level; the value of
//! re-asserting it here is that it holds for a whole daemon boot, which
//! is where a second tier construction would show up.)
//!
//! # Two legs, because one band is not the arm
//!
//! Both bands an operator pin can land in are booted:
//! [`PINNED_TIMEOUT_MS`] above the ceiling, which produces the largest
//! row this action has, and [`IN_BAND_TIMEOUT_MS`] inside it, which is
//! the **only configured state whose `coverage_finding` is null**. The
//! second exists because `record(...)` sits one line below
//! `report_guard_tier`'s `if let Some(finding)` warn block: folding the
//! two together would silence the boot row on every routine configured
//! host, and with only the above-ceiling leg the whole tree would still
//! be green.
//!
//! [#627]: https://github.com/hherb/kastellan/issues/627
//! [#633]: https://github.com/hherb/kastellan/issues/633

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use kastellan_supervisor::specs::core_service_spec;
use kastellan_supervisor::{default_supervisor, ServiceStatus};
use kastellan_tests_common::scripted_llm::{
    props_envelope, spawn_scripted_llm, spawn_scripted_llm_with_props, ScriptedLlm,
};
#[cfg(target_os = "macos")]
use kastellan_tests_common::serial_lock;
use kastellan_tests_common::{
    bring_up_pg_cluster, core_binary, current_username, pg_bin_dir_or_skip, skip_if_no_supervisor,
    stderr_tail, unique_suffix, unique_temp_root, wait_for_log_match, wait_for_status, PathGuard,
    PgCluster, ServiceGuard,
};

/// The operator's pinned per-request budget for the above-ceiling leg,
/// in milliseconds.
///
/// **Deliberately above `TIMEOUT_CEILING_MS` (120 000).** 350 s is
/// roughly what issue #612 tells a Metal operator to pin, so this is not
/// a contrived value — it is the configuration this project's own advice
/// produces. Choosing it means the row carries `PinBand::AboveCeiling`,
/// hence a non-null `coverage_finding`, hence the **largest payload this
/// row can have**: the `AboveCeiling` finding is 448 bytes against 309
/// for the next longest, a lead the four probe-derived numbers a
/// `Probed` row carries instead of `null`s cannot close.
///
/// The only other shape any test stores in Postgres is the 45-byte
/// `not_configured` row (`cli_ask_e2e`, and every daemon e2e that boots
/// without guard keys), which cannot approach the cap. So if
/// `boot_payload` ever grew past it, this is the test that would notice.
const PINNED_TIMEOUT_MS: u64 = 350_000;

/// The operator's pinned budget for the in-band leg, in milliseconds.
///
/// The **only configured state whose `coverage_finding` is null**, and
/// therefore the one worth a second boot: `record(...)` sits on the line
/// immediately after `report_guard_tier`'s `if let Some(finding)` warn
/// block, so an edit that pulls it inside would leave every *routine*
/// configured host with no boot row at all — and would pass the entire
/// workspace without this leg, since the above-ceiling row always has a
/// finding and every unit test calls `boot_payload` directly.
const IN_BAND_TIMEOUT_MS: u64 = 45_000;

/// A `const` block rather than an `assert!`: both sides are constants,
/// so the band membership these two legs depend on is a **compile**
/// error when it stops holding, not a failing run. Raising
/// `TIMEOUT_FLOOR_MS` past 45 s or dropping `TIMEOUT_CEILING_MS` below
/// 350 s would otherwise silently collapse the two legs onto one band
/// and leave the null-finding arm untested again. (Clippy's
/// `assertions_on_constants` refuses the runtime form anyway.)
const _: () = {
    use kastellan_core::cassandra::guard_model::timeout::{
        TIMEOUT_CEILING_MS, TIMEOUT_FLOOR_MS,
    };
    assert!(PINNED_TIMEOUT_MS > TIMEOUT_CEILING_MS);
    assert!(IN_BAND_TIMEOUT_MS > TIMEOUT_FLOOR_MS);
    assert!(IN_BAND_TIMEOUT_MS < TIMEOUT_CEILING_MS);
};

/// The per-request context the mock `/props` reports.
///
/// **Deliberately not `REQUIRED_GUARD_N_CTX` (66 048).** #627's review
/// found a mutant that froze `n_ctx` to that constant and survived,
/// because every fixture in `boot_report/tests.rs` passed it.
/// `the_scalars_and_the_basis_token_are_carried_verbatim` has since
/// killed that mutant at the unit layer by passing 131 072 explicitly —
/// so the reason to use it *here* is no longer that nothing else does.
/// It is that 131 072 is what the live DGX guard server reports, which
/// keeps this end-to-end leg independent of that one unit test rather
/// than resting on it.
const MOCK_N_CTX: u64 = 131_072;

/// The fitted threshold from measurement 3.
///
/// Written into the environment via `to_string()` rather than as a
/// separate string literal: Rust's float formatting emits the shortest
/// decimal that round-trips, so `TAU.to_string().parse::<f32>()` is
/// exactly `TAU` and the value the daemon parsed cannot silently differ
/// from the value this test compares against.
const TAU: f32 = 0.795_526_56;

/// A booted daemon's log files, surfaced so assertion-failure messages
/// can quote them.
struct Daemon {
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

/// Install, start, and wait for a daemon wired to the two mock backends
/// whose base URLs are passed in. The mocks themselves are spawned by
/// the caller — this only points the daemon's config at them.
///
/// Returns the daemon's log paths plus the RAII guards whose drop order
/// tears the service down before the directories it logs into.
///
/// **This is the third hand-rolled copy of a bring-up that already exists
/// as `kastellan_tests_common::bring_up_daemon`**, whose `extra_env`
/// parameter would carry the four guard vars below unchanged. It is not
/// migrated here only because doing so is [#634]'s whole content and
/// wants its own reviewable diff; the two differences that migration has
/// to absorb are this copy's 20-second log-match budget (the shared one
/// hardcodes 10) and its `pinned_timeout_ms` argument.
///
/// [#634]: https://github.com/hherb/kastellan/issues/634
fn bring_up_daemon(
    suffix: &str,
    data_dir: &Path,
    planner_base_url: &str,
    guard_base_url: &str,
    user: &str,
    pinned_timeout_ms: u64,
) -> (Daemon, (ServiceGuard, PathGuard, PathGuard)) {
    let core_log_dir = unique_temp_root("gboot-clog");
    std::fs::create_dir_all(&core_log_dir).expect("create core log dir");
    let core_log_guard = PathGuard {
        path: core_log_dir.clone(),
    };

    let state_dir = unique_temp_root("gboot-state");
    let state_guard = PathGuard {
        path: state_dir.clone(),
    };

    let binary = core_binary();
    let mut spec = core_service_spec(&binary, &core_log_dir);
    spec.name = format!("kastellan-supervisor-test-core-gboot-{suffix}");
    assert!(spec.name.len() <= 200);
    let stdout_path = core_log_dir.join(format!("{}.out", spec.name));
    let stderr_path = core_log_dir.join(format!("{}.err", spec.name));
    spec.stdout_log = Some(stdout_path.clone());
    spec.stderr_log = Some(stderr_path.clone());

    spec.env.push((
        "KASTELLAN_DATA_DIR".into(),
        data_dir.to_string_lossy().into_owned(),
    ));
    spec.env.push(("USER".into(), user.to_string()));
    spec.env.push((
        "KASTELLAN_STATE_DIR".into(),
        state_dir.to_string_lossy().into_owned(),
    ));

    // The prompt loader fails closed if the dir is missing.
    let workspace_prompts = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("prompts");
    spec.env.push((
        "KASTELLAN_PROMPTS_DIR".into(),
        workspace_prompts.to_string_lossy().into_owned(),
    ));

    // The planner backend. Nothing in this test drives a plan, but the
    // router is constructed at boot and a missing local URL is a config
    // error, so it has to point somewhere. `/v1` matches the on-wire
    // OpenAI-compat shape `compose_url` produces.
    spec.env.push((
        "KASTELLAN_LLM_LOCAL_URL".into(),
        format!("{planner_base_url}/v1"),
    ));
    spec.env.push((
        "KASTELLAN_LLM_LOCAL_MODEL".into(),
        "test-local-model".into(),
    ));
    spec.env.push(("KASTELLAN_LLM_TIMEOUT_MS".into(), "5000".into()));

    // The guard backend — a SEPARATE listener from the planner's, so the
    // two cannot consume each other's queues. `Router::props` strips the
    // `/v1` compat segment before appending `/props`, so the mock sees
    // `GET /props` at its root.
    spec.env.push((
        "KASTELLAN_LLM_GUARD_URL".into(),
        format!("{guard_base_url}/v1"),
    ));
    spec.env.push((
        "KASTELLAN_LLM_GUARD_MODEL".into(),
        "test-guard-model".into(),
    ));
    spec.env.push(("KASTELLAN_LLM_GUARD_TAU".into(), TAU.to_string()));
    // The key that makes this test deterministic: with a pin present,
    // `from_router_config` never runs the boot probe.
    spec.env.push((
        "KASTELLAN_LLM_GUARD_TIMEOUT_MS".into(),
        pinned_timeout_ms.to_string(),
    ));

    // `KASTELLAN_SHELL_EXEC_BIN` is deliberately NOT set. No task is
    // dispatched here, so no worker is ever spawned; discovery falls
    // back to the `current_exe()`-relative sibling and simply does not
    // register the tool if it is absent, which is not a boot failure.
    // That is also why this file skips `skip_if_sandbox_unavailable` —
    // adding a skip the test does not need only makes it silently
    // not-run on more hosts.

    let sup = default_supervisor();
    let service_guard = ServiceGuard {
        sup: default_supervisor(),
        name: spec.name.clone(),
    };
    sup.install(&spec).expect("install core");
    sup.start(&spec.name).expect("start core");

    // Stderr here too, for the same reason as the log wait below: on
    // launchd this is the first place a daemon that died before `main`
    // surfaces, and the bare status text names only the last polled
    // state.
    if let Err(e) = wait_for_status(
        sup.as_ref(),
        &spec.name,
        |s| s == ServiceStatus::Active,
        Duration::from_secs(10),
    ) {
        panic!("core active: {e}{}", stderr_tail(&stderr_path));
    }

    // `report_guard_tier` runs during bring-up, well before the
    // scheduler is spawned, and it `await`s the insert — so once this
    // line appears the insert has already been ATTEMPTED. Not that it
    // succeeded: `record` is deliberately non-fatal (an audit sink that
    // cannot take the row must not stop the tier), so a failure is an
    // `error!` in this same log plus an empty result below, not a race.
    //
    // Stderr goes into the failure message, and that is not decoration.
    // Tracing writes to STDOUT (`tracing_subscriber::fmt`'s default
    // writer), so a daemon that dies at the guard-tier step has already
    // put its boot lines there — stdout is not empty, it just never says
    // WHY. The reason propagates out of `main() -> Result<()>` and is
    // printed by `Termination` to stderr, which is the half
    // `wait_for_log_match`'s own timeout text ("full content was:")
    // cannot quote. The first Mac run of this test cost a round of
    // guessing to exactly that split.
    if let Err(e) = wait_for_log_match(
        &stdout_path,
        |s| s.contains("scheduler spawned"),
        Duration::from_secs(20),
    ) {
        panic!(
            "daemon should log 'scheduler spawned' within 20s: {e}{}",
            stderr_tail(&stderr_path)
        );
    }

    (
        Daemon {
            stdout_path,
            stderr_path,
        },
        (service_guard, core_log_guard, state_guard),
    )
}

fn cluster_for(suffix: &str) -> PgCluster {
    let bin_dir = pg_bin_dir_or_skip().expect("caller already short-circuited on missing pg");
    bring_up_pg_cluster(
        &bin_dir,
        "gboot-d",
        "gboot-l",
        &format!("kastellan-supervisor-test-pg-gboot-{suffix}"),
    )
}

/// The stored `policy / guard_tier.boot` payload, as Postgres holds it.
///
/// Read back from the real table rather than from a sink double:
/// `db::audit::insert` applies `truncate_payload` on the way in, so a
/// double asserts the payload that was *passed* rather than the one that
/// was *stored*. That distinction is live for this arm in a way it is
/// not for the two-key unconfigured one — the configured payload is the
/// larger of the two and the only one carrying a prose finding.
///
/// `fetch_all` + an explicit length assertion rather than `fetch_one`,
/// because `fetch_one` returns the FIRST row and errors only on zero: it
/// does not reject duplicates, so uniqueness has to be asserted here to
/// be asserted at all.
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


/// Boot one configured daemon at `pin_ms`, assert everything true of
/// **any** configured boot, then hand the stored row to `check` for the
/// band-specific half.
///
/// Two legs share this because the only thing that differs between them
/// is the pin and the row it produces: the bring-up, both mocks, and the
/// four "what the mocks saw" assertions are identical. A second
/// hand-rolled copy of them is how one leg quietly stops checking
/// something the other still does — the same argument #634 makes one
/// level up about `bring_up_daemon` itself.
fn with_configured_boot(pin_ms: u64, check: impl FnOnce(&serde_json::Value, &Daemon)) {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    if skip_if_no_supervisor() {
        return;
    }
    if pg_bin_dir_or_skip().is_none() {
        return;
    }
    if !core_binary().exists() {
        eprintln!(
            "\n[SKIP] kastellan binary missing at {}; run `cargo build --workspace`\n",
            core_binary().display()
        );
        return;
    }

    // Drop order is reverse declaration order:
    //   1. `_daemon_guards`  → stops + uninstalls the daemon service
    //   2. `guard`, `planner` → abort their accept-tasks
    //   3. `rt`              → shuts the runtime down
    //   4. `cluster`         → stops PG, wipes the data + log dirs
    // `rt` is declared BEFORE the two mocks so they are dropped while it
    // is still alive and their accept-tasks are cancelled before the
    // runtime shuts down rather than during it. Tidiness rather than
    // necessity — `JoinHandle::abort` on a handle whose runtime is
    // already gone is a safe no-op — but the tidy order is the one worth
    // writing down, because the other order's harmlessness is a fact
    // about tokio a reader would have to go and check.
    let suffix = unique_suffix();
    let user = current_username();
    let cluster = cluster_for(&suffix);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    // Multi-thread is load-bearing, not a default carried over.
    // `bring_up_daemon` blocks THIS thread for up to 30 s while the
    // daemon dials `/props`; only a multi-threaded runtime drives the
    // mocks' accept tasks in that window. A `current_thread` runtime
    // deadlocks here — the daemon waits for a `/props` nothing is
    // serving, and the test waits for a log line that never comes.

    // The planner's mock. Both queues are empty on purpose — no plan is
    // ever formulated here, and an empty queue 503s, so a stray planner
    // call would be loud rather than absorbed.
    let planner: ScriptedLlm = rt.block_on(spawn_scripted_llm(vec![], vec![]));

    // The guard's mock: answers `/props` forever, and 503s any chat
    // completion. A 503 here would mean the probe ran despite the pin.
    let guard: ScriptedLlm = rt.block_on(spawn_scripted_llm_with_props(
        vec![],
        vec![],
        Some(props_envelope(MOCK_N_CTX)),
    ));

    rt.block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "test",
            "setup",
            serde_json::json!({"test": "guard_boot_row_e2e_setup"}),
        )
        .await
        .expect("probe run");
    });

    let (daemon, _daemon_guards) = bring_up_daemon(
        &suffix,
        &cluster.data_dir,
        &planner.base_url,
        &guard.base_url,
        &user,
        pin_ms,
    );

    // ---- what the two mocks saw ------------------------------------------
    //
    // Asserted BEFORE the row is read, deliberately. These are the only
    // evidence in the file that the pin skipped the probe, and a failing
    // payload assertion further down would otherwise panic first and
    // take them with it — leaving a run that says "the row is wrong"
    // where it could have said "and the probe ran, which is why".
    //
    // Exactly one `/props`: one daemon boot verifies the guard context
    // once. A different number means a retry loop, a second tier
    // construction, or the supervisor restarting a crashed daemon —
    // none of which any other signal here would show.
    assert_eq!(
        *guard.props_requests.lock().unwrap(),
        1,
        "expected exactly one GET /props for one daemon boot (a second means a retry \
         loop, a second tier construction, or a supervisor restart){}",
        stderr_tail(&daemon.stderr_path)
    );
    // Zero chat completions on the guard listener is the direct evidence
    // that the operator pin skipped the probe — `run_probe` would have
    // issued up to `PROBE_SAMPLES` of them. Without this, a boot that
    // probed anyway and then discarded the result would still produce
    // the row asserted below.
    //
    // The guard is bound once rather than locked twice in one `assert!`:
    // the two-lock form is safe only because an `assert!` condition is
    // its own temporary scope, and the day someone rewrites it as an
    // `if let` the failure mode is a HANG, not a failed assertion.
    let guard_chats = guard.chat_requests.lock().unwrap().len();
    assert_eq!(
        guard_chats, 0,
        "a pinned timeout must skip the boot probe entirely; got {guard_chats} chat \
         request(s) on the guard backend"
    );
    // And the guard's traffic went to the guard's listener, not the
    // planner's — the reason the two are separate mocks.
    let planner_chats = planner.chat_requests.lock().unwrap().len();
    assert_eq!(
        planner_chats, 0,
        "no plan is formulated in this test; got {planner_chats} chat request(s) on \
         the planner backend"
    );
    assert_eq!(
        *planner.props_requests.lock().unwrap(),
        0,
        "the guard's /props must not reach the planner backend"
    );

    let stored = rt.block_on(async {
        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("connect runtime pool");
        guard_tier_boot_payload(&pool).await
    });

    // The row was stored whole. `guard_tier.boot` has no key in
    // `db::audit::PRESERVED_KEYS`, so an over-cap payload collapses to a
    // `{_truncated, sha256, len}` fingerprint rather than losing one
    // field — which would take the coverage finding with it.
    //
    // Checked BEFORE the structural equality in `check`, which is what
    // makes it an assertion rather than an ornament: a fingerprint row
    // fails that equality too, so tested afterwards this could only ever
    // run in a state where the test had already panicked.
    assert!(
        stored.get("_truncated").is_none(),
        "a guard_tier.boot payload must fit under db::audit::PAYLOAD_MAX_BYTES; \
         got a truncation fingerprint: {stored:?}"
    );

    check(&stored, &daemon);
}

/// The seam, at the band that produces the **largest** row: an
/// above-ceiling operator pin, with a non-null `coverage_finding`.
#[test]
fn a_configured_daemon_boot_stores_the_shared_configured_payload() {
    with_configured_boot(PINNED_TIMEOUT_MS, |stored, daemon| {
        // ---- the seam ----------------------------------------------------
        //
        // Structural equality against the shared pure builder, given the
        // same three inputs assembled independently here. This is what
        // `boot_report`'s unit tests cannot see: they would all stay
        // green if `main.rs` went back to composing the payload inline
        // and the two copies drifted, and they would stay green if the
        // `record(...)` call were deleted altogether.
        //
        // (Strictly, a deleted `record` is caught one step earlier, by
        // the row-count assertion inside `guard_tier_boot_payload` — the
        // equality never gets a row to compare. Worth knowing which line
        // holds which mutant before deleting either.)
        //
        // `serde_json::Value` equality, not byte equality — JSONB does
        // not round-trip key order, so a string comparison would fail
        // for the wrong reason.
        let budget = kastellan_core::cassandra::guard_model::timeout::validate_operator_timeout(
            PINNED_TIMEOUT_MS,
        )
        .expect("a positive pin is accepted");
        let expected = kastellan_core::cassandra::guard_model::boot_report::boot_payload(
            TAU, MOCK_N_CTX, &budget,
        );
        assert_eq!(
            *stored,
            expected,
            "the daemon must record the shared configured payload verbatim\n\
             --- daemon stdout ({}) ---\n{}{}",
            daemon.stdout_path.display(),
            std::fs::read_to_string(&daemon.stdout_path)
                .unwrap_or_else(|e| format!("<unreadable: {e}>")),
            stderr_tail(&daemon.stderr_path),
        );

        // ---- literals the shared builder cannot supply --------------------
        //
        // Equality above puts `boot_payload` on BOTH sides, so a defect
        // inside it moves the two together and passes — the exact shape
        // that let #627's `not_configured` token mutant survive two
        // assertions. These spell the durable wire format out, so a
        // renamed key or a drifted token fails HERE even when the
        // builder agrees with itself.
        //
        // Each is also killed by a unit test in `boot_report/tests.rs`,
        // so they add no NEW mutant to the tree — what they add is
        // independence from those tests continuing to exist.
        assert_eq!(stored["configured"], serde_json::json!(true));
        assert_eq!(stored["timeout_ms"], serde_json::json!(PINNED_TIMEOUT_MS));
        assert_eq!(stored["n_ctx"], serde_json::json!(MOCK_N_CTX));
        assert_eq!(
            stored["timeout_basis"],
            serde_json::json!("operator-above-ceiling"),
            "a pin above TIMEOUT_CEILING_MS must be countable by equality, not by \
             reading prose out of the finding"
        );
        assert!(
            stored["coverage_finding"].is_string(),
            "an above-ceiling pin is a finding; got {:?}",
            stored["coverage_finding"]
        );

        assert_probe_derived_keys_are_null(stored);
    });
}

/// The same seam at the **quiet** band, which is the one no test reached.
///
/// An in-band pin is the only configured state whose `coverage_finding`
/// is null, and it is the state most hosts that pin a timeout are
/// actually in. Its absence left a specific, plausible edit unguarded:
/// `record(...)` sits on the line after `report_guard_tier`'s
/// `if let Some(finding) { warn!(...) }` block, and folding the two
/// together — they are adjacent, and both "report the finding" — would
/// leave every routine configured host with no boot row while passing
/// every other test in the tree.
///
/// The full equality is asserted here too, not just the null finding.
/// The row differs from the above-ceiling one in three keys
/// (`timeout_ms`, `timeout_basis`, `coverage_finding`), so a builder
/// that ignored its band would agree with one leg and not the other.
#[test]
fn an_in_band_pin_stores_a_configured_row_with_a_null_coverage_finding() {
    with_configured_boot(IN_BAND_TIMEOUT_MS, |stored, daemon| {
        let budget = kastellan_core::cassandra::guard_model::timeout::validate_operator_timeout(
            IN_BAND_TIMEOUT_MS,
        )
        .expect("a positive pin is accepted");
        let expected = kastellan_core::cassandra::guard_model::boot_report::boot_payload(
            TAU, MOCK_N_CTX, &budget,
        );
        assert_eq!(
            *stored,
            expected,
            "a routine configured boot must record the shared payload verbatim\n\
             --- daemon stdout ({}) ---\n{}{}",
            daemon.stdout_path.display(),
            std::fs::read_to_string(&daemon.stdout_path)
                .unwrap_or_else(|e| format!("<unreadable: {e}>")),
            stderr_tail(&daemon.stderr_path),
        );

        assert_eq!(stored["configured"], serde_json::json!(true));
        assert_eq!(stored["timeout_ms"], serde_json::json!(IN_BAND_TIMEOUT_MS));
        assert_eq!(stored["n_ctx"], serde_json::json!(MOCK_N_CTX));
        assert_eq!(
            stored["timeout_basis"],
            serde_json::json!("operator"),
            "an in-band pin folds to the bare `operator` token -- the two out-of-band \
             bands are the ones that qualify their spelling"
        );
        // The point of this leg. `null`, and PRESENT: a reader querying
        // `coverage_finding IS NOT NULL` must find the key on a routine
        // row too, or the query silently selects on key presence.
        assert!(
            stored
                .get("coverage_finding")
                .is_some_and(serde_json::Value::is_null),
            "an in-band pin is the operator's own number and earns no finding; \
             got {:?}",
            stored.get("coverage_finding")
        );

        assert_probe_derived_keys_are_null(stored);
    });
}

/// An operator pin never probes, so all four probe-derived numbers are
/// absent — and absent must reach the row as JSON `null`, not as a
/// fabricated 0 that reads like an observation (a wedged backend really
/// can measure 0.0 tok/s).
///
/// The keys must still be PRESENT: the key set never shrinks, so a
/// reader querying `tok_per_s` finds it on every row rather than only on
/// the hosts that happened to probe.
///
/// Shared by both legs because it is a property of the `Operator` basis,
/// not of either band.
fn assert_probe_derived_keys_are_null(stored: &serde_json::Value) {
    for key in [
        "tok_per_s",
        "slowest_tok_per_s",
        "measured_samples",
        "attempted_samples",
    ] {
        assert!(
            stored.get(key).is_some_and(serde_json::Value::is_null),
            "{key} must be present and null on an operator pin; got {:?}",
            stored.get(key)
        );
    }
}
