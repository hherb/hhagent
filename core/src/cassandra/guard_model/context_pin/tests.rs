//! Unit tests for the guard context pin (wiring-spec D8, issue #604).
//!
//! Everything here is pure: no server, no Postgres, no fixture file.
//! That is the point — the accepting arm of [`context_verdict`] is
//! reachable only because `required` is a parameter, so these tests can
//! prove the check *passes* a good server as well as refusing a bad
//! one.

use super::*;
use serde_json::json;

/// The exact `/props` shape the DGX guard server serves, trimmed to the
/// fields this module reads. Captured 2026-08-23 from
/// `llama-server -m Shieldstral-1.0-3B-Q8_0.gguf -c 131072 -ngl 99`.
fn dgx_props() -> serde_json::Value {
    json!({
        "default_generation_settings": {
            "params": {"seed": 4_294_967_295_u64, "temperature": 0.8},
            "n_ctx": 131_072
        },
        "total_slots": 4,
        "model_alias": "shieldstral",
        "model_ftype": "Q8_0",
        "model_path": "/home/hherb/models/shieldstral/upstream/Shieldstral-1.0-3B-Q8_0.gguf",
        "modalities": {"vision": false, "video": false, "audio": false}
    })
}

/// The requirement is DERIVED from the scan cap, not written twice.
///
/// This is the mutation guard for the whole module: replacing
/// `SCAN_BYTE_CAP + GUARD_PROMPT_OVERHEAD_TOKENS` with the literal
/// 66048 passes every other test in this file and then silently sizes
/// the check for the old cap the day `SCAN_BYTE_CAP` moves — a
/// fail-open, because an under-sized requirement admits a server that
/// will 400 at runtime.
#[test]
fn context_pin_required_tracks_the_scan_cap() {
    assert_eq!(
        REQUIRED_GUARD_N_CTX,
        SCAN_BYTE_CAP as u64 + GUARD_PROMPT_OVERHEAD_TOKENS,
        "REQUIRED_GUARD_N_CTX must be derived from SCAN_BYTE_CAP, never a literal"
    );
    // The value today, so a change to either input is a visible diff
    // rather than a silent one.
    assert_eq!(REQUIRED_GUARD_N_CTX, 66_048);
}

#[test]
fn n_ctx_is_read_from_the_nested_generation_settings() {
    assert_eq!(n_ctx_from_props(&dgx_props()), Some(131_072));
}

/// A build that reports the size at the top level instead.
#[test]
fn n_ctx_falls_back_to_the_top_level_field() {
    assert_eq!(n_ctx_from_props(&json!({"n_ctx": 32_768})), Some(32_768));
}

/// When both are present the NESTED one wins, because it is the
/// per-request number: a build with `total_slots > 1` can report a
/// larger total up top while each request is confined to a slot.
/// Preferring the top-level figure there would accept a server that
/// then 400s — the exact fail-open D8 exists to close.
#[test]
fn the_nested_field_wins_over_a_top_level_one() {
    let props = json!({
        "default_generation_settings": {"n_ctx": 32_768},
        "n_ctx": 131_072
    });
    assert_eq!(
        n_ctx_from_props(&props),
        Some(32_768),
        "the per-request size must win over a larger aggregate"
    );
}

/// Every shape that is not a positive integer reads as "did not tell
/// us". Coercing any of them would produce a size the server never
/// claimed.
#[test]
fn non_numeric_and_non_positive_context_shapes_are_absent() {
    let cases = [
        json!({}),
        json!({"default_generation_settings": {}}),
        json!({"n_ctx": null}),
        json!({"n_ctx": "131072"}),
        json!({"n_ctx": {"value": 131_072}}),
        json!({"n_ctx": [131_072]}),
        json!({"n_ctx": 0}),
        json!({"n_ctx": -1}),
        json!({"n_ctx": 1.5}),
        json!({"default_generation_settings": {"n_ctx": "big"}}),
        json!({"default_generation_settings": null, "n_ctx": null}),
    ];
    for props in cases {
        assert_eq!(n_ctx_from_props(&props), None, "must be absent for {props}");
    }
}

/// A nested field of the wrong shape must fall through to the
/// top-level one rather than poisoning the lookup.
#[test]
fn an_unusable_nested_field_falls_through_to_the_top_level() {
    let props = json!({
        "default_generation_settings": {"n_ctx": null},
        "n_ctx": 131_072
    });
    assert_eq!(n_ctx_from_props(&props), Some(131_072));
}

/// The boundary, in all three directions.
///
/// The middle case is the one that matters and the one a `<=` mutation
/// breaks: `required` already *is* the worst case plus its overhead, so
/// a server reporting exactly that number is correctly sized and must
/// boot.
#[test]
fn context_verdict_accepts_exactly_the_requirement_and_refuses_below_it() {
    const REQUIRED: u64 = 66_048;

    let err = context_verdict(Some(REQUIRED - 1), REQUIRED)
        .expect_err("one token short must be refused");
    assert_eq!(err, GuardContextError::TooSmall { reported: REQUIRED - 1, required: REQUIRED });

    assert_eq!(
        context_verdict(Some(REQUIRED), REQUIRED),
        Ok(REQUIRED),
        "exactly the requirement is enough -- it already includes the overhead"
    );
    assert_eq!(context_verdict(Some(REQUIRED + 1), REQUIRED), Ok(REQUIRED + 1));
}

/// The success arm returns what was VERIFIED, not what was wanted, so a
/// boot line can say `n_ctx=131072` rather than reciting the constant.
#[test]
fn context_verdict_returns_the_reported_size_on_success() {
    assert_eq!(context_verdict(Some(131_072), 66_048), Ok(131_072));
}

#[test]
fn an_absent_context_size_is_refused_and_never_assumed() {
    assert_eq!(context_verdict(None, 66_048), Err(GuardContextError::NoContextSize));
}

/// End to end over the real `/props` body: the DGX server passes the
/// production check today. This is the accepting arm of the *wrapper*,
/// which the parameterised form alone would leave unexercised.
#[test]
fn the_dgx_guard_server_satisfies_the_production_requirement() {
    let reported = n_ctx_from_props(&dgx_props());
    assert_eq!(
        verify_guard_context(reported),
        Ok(131_072),
        "the host measurement 3 ran on must still boot the tier"
    );
}

/// The `-c 32768` server that produced #604 is refused, and the
/// refusal names the flag to change.
#[test]
fn the_server_that_produced_604_is_refused_with_an_actionable_message() {
    let err = verify_guard_context(Some(32_768)).expect_err("32768 is below the requirement");
    assert_eq!(
        err,
        GuardContextError::TooSmall { reported: 32_768, required: REQUIRED_GUARD_N_CTX }
    );
    let msg = err.to_string();
    assert!(msg.contains("-c 66048"), "must name the flag and value: {msg}");
    assert!(msg.contains("#604"), "must cite the issue that measured this: {msg}");
    assert!(
        msg.contains("OPEN"),
        "must state the consequence, not just the fact: {msg}"
    );
}

/// `kind()` is a single whitespace-free token in every variant — it
/// goes in a log field, where a `Display` paragraph would wrap.
#[test]
fn every_kind_is_one_short_token() {
    let all = [
        GuardContextError::NoContextSize,
        GuardContextError::TooSmall { reported: 1, required: 2 },
    ];
    for e in all {
        let k = e.kind();
        assert!(!k.is_empty(), "kind must not be empty");
        assert!(
            !k.chars().any(char::is_whitespace),
            "kind must be one token, got {k:?}"
        );
        assert!(k.len() <= 32, "kind must fit a log field, got {k:?}");
    }
}
