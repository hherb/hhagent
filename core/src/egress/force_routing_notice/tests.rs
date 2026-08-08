use super::*;

#[test]
fn the_action_and_actor_are_stable_audit_contract() {
    // Renaming either breaks operator queries against audit_log.
    assert_eq!(ACTION_FORCE_ROUTING_DISABLED, "egress.force_routing_disabled");
    assert_eq!(DAEMON_ACTOR, "daemon");
}

#[test]
fn the_payload_names_the_env_var_that_controls_it() {
    let p = force_routing_disabled_payload();
    assert_eq!(p["env_var"], "KASTELLAN_EGRESS_FORCE_ROUTING");
    // The operator-visible phrase travels WITH the row, so an audit reader and
    // a log grepper are looking at the same string.
    assert_eq!(p["phrase"], FORCE_ROUTING_DISABLED_LOG_PHRASE);
    // …and so does the consequence, which used to be typed twice.
    assert_eq!(p["consequence"], CONSEQUENCE);
}

#[test]
fn the_env_var_const_tracks_the_module_that_reads_it() {
    // Aliased rather than retyped: a rename in `force_route` must move this,
    // or the notice would name a variable nothing consults.
    assert_eq!(ENV_VAR, crate::worker_lifecycle::force_route::ENV_ENABLE);
}

#[test]
fn the_message_carries_the_phrase_the_consequence_and_the_remedy() {
    // The previous version of this test asserted that the const contains a
    // substring of ITSELF, which cannot observe the call site inlining a
    // literal — precisely the #516/#524/#525 drift it cited. Asserting on the
    // assembled message means `main.rs` has nothing left to hand-type.
    let msg = force_routing_disabled_message();
    assert!(msg.contains(FORCE_ROUTING_DISABLED_LOG_PHRASE), "{msg}");
    assert!(msg.contains(CONSEQUENCE), "{msg}");
    assert!(msg.contains(ENV_VAR), "{msg}");
    assert!(
        msg.contains("kastellan.env.local"),
        "the remedy must name the file that survives a reinstall: {msg}"
    );
}
