//! Unit tests for [`DaemonSpec`] (issue #634).
//!
//! Hermetic and pure — no directory is created, no unit is installed, no
//! socket is opened. That matters more here than it usually does:
//! `linux-check.yml` runs `cargo test -p kastellan-tests-common` on every
//! PR and nothing else, while the four daemon e2es these values configure
//! are DGX-gated and run on no PR at all. So this file is the only CI
//! coverage the bring-up contract has.

use super::*;

/// A spec with every default left alone.
fn base_spec() -> DaemonSpec {
    DaemonSpec::new(
        "gboot",
        "1234-5678",
        Path::new("/tmp/pgdata"),
        "tester",
        LlmEndpoint::Base("http://127.0.0.1:9999".into()),
    )
}

fn built(spec: &DaemonSpec) -> ServiceSpec {
    spec.service_spec(
        Path::new("/nonexistent/kastellan"),
        Path::new("/nonexistent/logs"),
        Path::new("/nonexistent/state"),
    )
}

/// The value the supervisor materialises for `key`: the LAST entry,
/// because both backends render `spec.env` in order and later wins
/// (systemd emits one `Environment=` line per entry; launchd emits
/// duplicate plist dict keys).
fn effective(spec: &ServiceSpec, key: &str) -> Option<String> {
    spec.env
        .iter()
        .rfind(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

/// `Base` is a base: the compat segment is appended.
#[test]
fn an_llm_base_url_gains_the_compat_segment() {
    let s = built(&base_spec());
    assert_eq!(
        effective(&s, "KASTELLAN_LLM_LOCAL_URL").as_deref(),
        Some("http://127.0.0.1:9999/v1"),
    );
}

/// `Verbatim` is complete: **nothing** is appended.
///
/// This is the one migration hazard in #634 that fails *silently*.
/// `observation_capture` reads `KASTELLAN_LLM_LOCAL_URL` from the
/// operator's environment, whose documented value already ends in `/v1`
/// (`http://127.0.0.1:8000/v1`, per that test's own panic message).
/// Routing it through the `Base` arm yields `.../v1/v1`, and because
/// that test drives a *real* LLM the failure surfaces as an unreachable
/// backend rather than as anything naming the URL.
///
/// Deliberately asserts the whole string rather than
/// `!url.ends_with("/v1/v1")`: the second form passes for a URL that
/// gained any other suffix, and the property under test is "used
/// verbatim", not "did not gain this one mistake".
#[test]
fn a_verbatim_llm_url_gains_nothing() {
    let spec = DaemonSpec::new(
        "obs",
        "1",
        Path::new("/tmp/pgdata"),
        "tester",
        LlmEndpoint::Verbatim("http://127.0.0.1:8000/v1".into()),
    );
    assert_eq!(
        effective(&built(&spec), "KASTELLAN_LLM_LOCAL_URL").as_deref(),
        Some("http://127.0.0.1:8000/v1"),
    );
}

/// A spec that sets nothing extra reproduces what the shared helper did
/// before this type existed — the migration's whole no-behaviour-change
/// claim, in one test.
///
/// ⚠️ **Deliberately LITERALS, not the constants.** Asserting
/// `Some(DEFAULT_LLM_MODEL)` would put the constant on both sides, so
/// editing it moves the expectation with it and the test passes at any
/// value — the transposition-shaped blind spot #633 recorded one crate
/// over. The values pinned here are the ones the pre-#634 helper
/// hardcoded, which is exactly what "behaves identically" means, so a
/// change to any of them is a change to five e2es' boot config and
/// should have to argue for itself here.
///
/// The `assert_eq!`s against the constants that follow are the other
/// half of that pair: they keep the constants and the literals from
/// diverging, so a reader of either finds one answer.
#[test]
fn the_defaults_are_the_values_the_helper_used_to_hardcode() {
    let s = built(&base_spec());
    assert_eq!(
        effective(&s, "KASTELLAN_LLM_LOCAL_MODEL").as_deref(),
        Some("test-local-model"),
    );
    assert_eq!(effective(&s, "KASTELLAN_LLM_TIMEOUT_MS").as_deref(), Some("5000"));
    assert_eq!(base_spec().ready_timeout_value(), Duration::from_secs(10));

    assert_eq!(DEFAULT_LLM_MODEL, "test-local-model");
    assert_eq!(DEFAULT_LLM_TIMEOUT_MS, "5000");
    assert_eq!(DEFAULT_READY_TIMEOUT, Duration::from_secs(10));
}

/// Each override reaches the unit, and only the one asked for moves.
#[test]
fn each_override_replaces_exactly_its_own_default() {
    let s = built(&base_spec().llm_model("gemma4:26b").llm_timeout_ms("240000"));
    assert_eq!(
        effective(&s, "KASTELLAN_LLM_LOCAL_MODEL").as_deref(),
        Some("gemma4:26b"),
    );
    assert_eq!(
        effective(&s, "KASTELLAN_LLM_TIMEOUT_MS").as_deref(),
        Some("240000"),
    );
    // Untouched by either setter.
    assert_eq!(
        effective(&s, "KASTELLAN_LLM_LOCAL_URL").as_deref(),
        Some("http://127.0.0.1:9999/v1"),
    );
    assert_eq!(
        base_spec().ready_timeout(Duration::from_secs(20)).ready_timeout_value(),
        Duration::from_secs(20),
    );
}

/// `extra_env` is applied LAST, so an entry naming a key the spec
/// already set wins.
///
/// `mail_daemon_e2e` depends on this to point a live-LLM run at a real
/// model, and until #634 the guarantee was a comment at that call site
/// with nothing testing it. Asserted through `effective` — i.e. through
/// the same last-wins rule both supervisor backends implement — rather
/// than by counting entries, because what matters is the value the
/// daemon's process environment ends up with.
#[test]
fn extra_env_wins_over_a_default_it_names() {
    let s = built(
        &base_spec()
            .llm_model("ignored-by-the-override")
            .env("KASTELLAN_LLM_LOCAL_MODEL", "the-real-model"),
    );
    assert_eq!(
        effective(&s, "KASTELLAN_LLM_LOCAL_MODEL").as_deref(),
        Some("the-real-model"),
    );
    // Both entries are present — the override is an ordering property,
    // not a de-duplication one, and asserting the count keeps a future
    // "tidy up duplicates" change honest about what it would break.
    assert_eq!(
        s.env
            .iter()
            .filter(|(k, _)| k == "KASTELLAN_LLM_LOCAL_MODEL")
            .count(),
        2,
    );
}

/// `envs` adds in order, and composes with `env`.
#[test]
fn envs_adds_every_entry_in_order() {
    let s = built(&base_spec().envs(vec![
        ("A".to_string(), "1".to_string()),
        ("B".to_string(), "2".to_string()),
    ]).env("A", "3"));
    assert_eq!(effective(&s, "B").as_deref(), Some("2"));
    // Last wins within extra_env too.
    assert_eq!(effective(&s, "A").as_deref(), Some("3"));
}

/// The unit name carries both the label and the suffix, in that order.
///
/// Both are `&str` and adjacent in `new`, so this is the one place a
/// transposition would show. It is also what keeps co-running tests from
/// installing over each other.
#[test]
fn the_service_name_carries_the_label_then_the_suffix() {
    assert_eq!(
        base_spec().service_name(),
        "kastellan-supervisor-test-core-gboot-1234-5678",
    );
    assert_eq!(built(&base_spec()).name, base_spec().service_name());
}

/// The log file names follow the RENAMED unit, not `core_service_spec`'s
/// `CORE_SERVICE_NAME` default.
///
/// `core_service_spec` derives both paths from the production service
/// name; every hand-rolled copy overwrote them after renaming the unit,
/// and a migration that renamed the unit and forgot the logs would leave
/// four co-running daemons appending to one pair of files.
#[test]
fn the_log_paths_follow_the_renamed_unit() {
    let s = built(&base_spec());
    let name = base_spec().service_name();
    assert_eq!(
        s.stdout_log,
        Some(Path::new("/nonexistent/logs").join(format!("{name}.out"))),
    );
    assert_eq!(
        s.stderr_log,
        Some(Path::new("/nonexistent/logs").join(format!("{name}.err"))),
    );
}

/// Every key a daemon needs to boot is present.
///
/// Spelled out rather than derived, so *dropping* one during the
/// migration fails here. The daemon's own failure for a missing
/// `KASTELLAN_PROMPTS_DIR` is a fail-closed abort with an empty stdout,
/// which is the shape that costs a session to diagnose.
#[test]
fn the_common_key_set_is_complete() {
    let s = built(&base_spec());
    for key in [
        "KASTELLAN_DATA_DIR",
        "USER",
        "KASTELLAN_STATE_DIR",
        "KASTELLAN_PROMPTS_DIR",
        "KASTELLAN_LLM_LOCAL_URL",
        "KASTELLAN_LLM_LOCAL_MODEL",
        "KASTELLAN_LLM_TIMEOUT_MS",
    ] {
        assert!(
            effective(&s, key).is_some(),
            "a booting daemon needs {key}; the spec did not set it",
        );
    }
    // The two the caller supplied, read back by value rather than by
    // presence — `data_dir` and `user` are both `String`-shaped and
    // adjacent in `new`, so presence alone would survive a swap.
    assert_eq!(effective(&s, "KASTELLAN_DATA_DIR").as_deref(), Some("/tmp/pgdata"));
    assert_eq!(effective(&s, "USER").as_deref(), Some("tester"));
    assert_eq!(effective(&s, "KASTELLAN_STATE_DIR").as_deref(), Some("/nonexistent/state"));
}

/// `core_service_spec`'s own env survives — the force-routing default in
/// particular, which is a containment control rather than a convenience.
#[test]
fn the_inherited_force_routing_default_is_not_dropped() {
    assert_eq!(
        effective(&built(&base_spec()), "KASTELLAN_EGRESS_FORCE_ROUTING").as_deref(),
        Some("1"),
    );
}

/// A label long enough to breach the supervisor's 200-char name cap is
/// refused loudly rather than installed and rejected by the backend.
#[test]
#[should_panic(expected = "200-char cap")]
fn an_overlong_service_name_panics_rather_than_installing() {
    let spec = DaemonSpec::new(
        "x".repeat(250),
        "1",
        Path::new("/tmp/pgdata"),
        "tester",
        LlmEndpoint::Base("http://127.0.0.1:1".into()),
    );
    let _ = built(&spec);
}
