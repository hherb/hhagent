//! The pure half of the operator-ask path — reading a resolved ask into a
//! decision, and the resume-state codec.
//!
//! Pure and sync, so the rules the `Escalate` arm depends on have unit
//! tests rather than being reachable only through a Postgres e2e.
//!
//! Spec: `docs/superpowers/specs/2026-08-18-ask-path-slice-1b-design.md`.

use kastellan_db::asks::Ask;

use crate::cassandra::types::Plan;
use crate::scheduler::inner_loop::{PlanRecord, StepOutcome};

/// The answers a `plan_approval` ask offers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Choice {
    Approve,
    Deny,
}

/// What a resolved ask means for one specific plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AskDecision {
    /// The operator approved, and this is the plan they approved.
    Approved,
    /// The operator denied. Terminal for the task, whatever the plan.
    Denied,
    /// There is no usable decision for *this* plan — a different plan was
    /// approved, the ask binds to no plan, or the resolution is malformed.
    /// The safe arm: it leads to raising a fresh ask, never to proceeding.
    NotForThisPlan,
}

/// `asks.kind` for an escalated-plan approval. The column's CHECK accepts
/// only this value today; a new kind is a deliberate migration.
pub const ASK_KIND_PLAN_APPROVAL: &str = "plan_approval";

/// Operator override for how long an ask stays answerable.
pub const ASK_DEADLINE_ENV: &str = "KASTELLAN_ASK_DEADLINE_S";

/// 24 hours. Long enough that an escalation raised overnight is still
/// answerable in the morning, short enough that an abandoned task does not
/// sit suspended for a week.
pub const DEFAULT_ASK_DEADLINE_S: i64 = 86_400;

/// The ask's resolution as a validated [`Choice`], or `None` if there is
/// no usable one.
///
/// **Validated against the ask's own `options`, not against a constant.**
/// The closed set is per-ask: a future kind offers different options, and
/// checking against a hardcoded `{approve, deny}` would silently accept the
/// wrong vocabulary for it.
///
/// `db::asks::resolve` already enforces the closed set on the way in, so
/// every arm returning `None` here is defence in depth — against a
/// direct-SQL writer, or a resolver added later that forgets. They must
/// return `None` and never `Approve`: `None` leads to raising a fresh ask,
/// which costs one operator interaction, while a wrong `Approve` lets a
/// plan run that nobody approved.
pub fn resolution_choice(ask: &Ask) -> Option<Choice> {
    let choice = ask.resolution.as_ref()?.get("choice")?.as_str()?;
    let offered = ask.options.as_array()?;
    if !offered.iter().any(|o| o.as_str() == Some(choice)) {
        return None;
    }
    match choice {
        "approve" => Some(Choice::Approve),
        "deny" => Some(Choice::Deny),
        _ => None,
    }
}

/// What `ask` decides about the plan whose digest is `plan_digest`.
///
/// An approval binds to a plan digest (slice 1a, spec D1): the resumed task
/// replans from scratch and goes through review again, and the approval
/// carries only if the new plan is the one that was approved. A denial
/// binds to the task, not the plan — see spec D2 for why the asymmetry is
/// deliberate.
///
/// An approval whose ask carries no `plan_digest` approves nothing. The
/// column is nullable for kinds that do not bind to a plan, and defaulting
/// such an approval to "covers this plan" is the fail-open direction.
pub fn decide(ask: &Ask, plan_digest: &str) -> AskDecision {
    match resolution_choice(ask) {
        Some(Choice::Deny) => AskDecision::Denied,
        Some(Choice::Approve) if ask.plan_digest.as_deref() == Some(plan_digest) => {
            AskDecision::Approved
        }
        _ => AskDecision::NotForThisPlan,
    }
}

/// Parse an ask deadline in seconds, falling back to
/// [`DEFAULT_ASK_DEADLINE_S`] for anything unusable.
///
/// Pure, taking the raw string, so the fallback rules have a test that does
/// not mutate process environment (which is global and races Rust's
/// parallel test threads — the `KASTELLAN_WORKER_OUT` flake).
///
/// Non-positive values fall back rather than being honoured: a zero or
/// negative deadline is already in the past when `raise` runs, so the
/// `asks_deadline_after_created` CHECK rejects the INSERT and the
/// escalation fails instead of asking the human — an operator typo turning
/// into a task failure.
pub fn ask_deadline_seconds(raw: Option<&str>) -> i64 {
    raw.and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_ASK_DEADLINE_S)
}

/// [`ask_deadline_seconds`] against the live environment.
pub fn deadline_from_env() -> i64 {
    ask_deadline_seconds(std::env::var(ASK_DEADLINE_ENV).ok().as_deref())
}

// ---------------------------------------------------------------------------
// The suspended run's state: serialise at suspend, restore at resume.
//
// Pure and I/O-free, so the round trip is unit-testable without Postgres —
// which matters because the only other way to reach it is a PG e2e.
// ---------------------------------------------------------------------------

/// What a resumed run gets back from its ask's `resume_state`.
///
/// A struct rather than a `(Vec<PlanRecord>, Vec<String>, Vec<String>)`
/// tuple: `advisories` and `blocks` have the same type, so a tuple lets a
/// call site swap them silently — and the swap would feed the planner
/// blocked-content notices as advice and vice versa.
#[derive(Debug, Default)]
pub struct RestoredRun {
    /// The completed plans of the suspended run, oldest first — rebuilt by
    /// calling `PlanRecord::new` on the stored inputs, so the sink screen is
    /// re-applied here rather than trusted from storage.
    pub plans: Vec<PlanRecord>,
    /// Reviewer advisories accumulated before the suspension.
    pub advisories: Vec<String>,
    /// Reviewer block notices accumulated before the suspension.
    pub blocks: Vec<String>,
}

/// Serialise a live run's history for storage on the ask that suspends it
/// (#564 slice 1b, D11).
///
/// Shape:
/// `{"plans": [{"plan": <Plan>, "outcomes": [<StepOutcome>]}],
///   "advisories": [<String>], "blocks": [<String>]}`.
///
/// **The inputs to `PlanRecord::new`, never its renders.** A `PlanRecord`
/// also holds the screened, planner-bound render of each outcome, and that
/// render is a pure function of `(plan, outcomes)`. Storing the inputs and
/// re-running the constructor on restore means the screen is applied by the
/// same code path both times; storing the render would mean a resumed run
/// putting text from the database straight into a planner prompt on the
/// word of whatever wrote that row.
///
/// `advisories` and `blocks` travel too because they are reviewer feedback
/// the run already earned. Dropping them would let the resumed planner
/// repeat a mistake the reviewer had already corrected.
pub fn resume_state_from(
    plans: &[PlanRecord],
    advisories: &[String],
    blocks: &[String],
) -> serde_json::Value {
    let plans: Vec<serde_json::Value> = plans
        .iter()
        .map(|p| serde_json::json!({"plan": p.plan, "outcomes": p.outcomes()}))
        .collect();
    serde_json::json!({
        "plans": plans,
        "advisories": advisories,
        "blocks": blocks,
    })
}

/// Rebuild a run's history from what [`resume_state_from`] wrote.
///
/// **Anything unrecognised restores as empty rather than erroring**, and
/// that asymmetry is deliberate: a lost history costs the task a replay of
/// steps it already ran, while a failed restore would throw away the
/// operator's decision entirely and make them answer the question again.
/// `None` — an ask raised before migration 0024, or one that binds to no
/// run — takes the same path.
///
/// `plans` is **all-or-nothing**: if any entry fails to decode, the whole
/// list is dropped. A partial list would misrepresent the order and count
/// of what the run did — telling the planner it went straight from plan 1
/// to plan 3 — which is a worse input than an honest empty history.
/// `advisories` and `blocks` are independent of it and of each other, and
/// each keeps only the string elements of an array.
pub fn restore_resume_state(value: Option<&serde_json::Value>) -> RestoredRun {
    let Some(obj) = value.and_then(serde_json::Value::as_object) else {
        return RestoredRun::default();
    };

    let plans = obj
        .get("plans")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .map(|e| {
                    let plan: Plan = serde_json::from_value(e.get("plan")?.clone()).ok()?;
                    let outcomes: Vec<StepOutcome> =
                        serde_json::from_value(e.get("outcomes")?.clone()).ok()?;
                    Some(PlanRecord::new(plan, outcomes))
                })
                // Collecting into `Option<Vec<_>>` short-circuits on the
                // first `None`, which is the all-or-nothing rule above.
                .collect::<Option<Vec<PlanRecord>>>()
        })
        .unwrap_or_default();

    if plans.is_none() {
        tracing::warn!(
            "an ask's resume_state carried a plan history that would not decode; \
             restoring an empty history, so the resumed task may re-run steps it \
             already ran"
        );
    }

    RestoredRun {
        plans: plans.unwrap_or_default(),
        advisories: string_list(obj.get("advisories")),
        blocks: string_list(obj.get("blocks")),
    }
}

/// The string elements of a JSON array; empty for anything else. Non-string
/// elements are skipped rather than voiding the list — these are
/// presentation-only reviewer notes, and one odd entry is not a reason to
/// drop the rest of the feedback.
fn string_list(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_db::asks::Ask;
    use time::OffsetDateTime;

    /// A resolved `Ask` fixture. Every field is set explicitly so a new
    /// column added to `Ask` is a compile error here rather than a silent
    /// default the tests stop covering.
    fn ask(resolution: Option<serde_json::Value>, plan_digest: Option<&str>) -> Ask {
        Ask {
            id: 42,
            task_id: 7,
            kind: ASK_KIND_PLAN_APPROVAL.to_string(),
            body: "sends mail to a stranger".to_string(),
            options: serde_json::json!(["approve", "deny"]),
            plan_digest: plan_digest.map(String::from),
            state: "resolved".to_string(),
            created_at: OffsetDateTime::now_utc(),
            deadline_at: OffsetDateTime::now_utc(),
            resolved_at: Some(OffsetDateTime::now_utc()),
            resolved_by: Some("operator".to_string()),
            resolution,
            resume_state: None,
        }
    }

    /// A one-step plan whose step names `tool`, so a restored record's
    /// screen can be shown to run under the right guard profile.
    fn plan_with_tool(tool: &str) -> Plan {
        Plan {
            context: "c".to_string(),
            decision: "act".to_string(),
            rationale: "r".to_string(),
            steps: vec![crate::cassandra::types::PlannedStep {
                tool: tool.to_string(),
                method: "m".to_string(),
                parameters: serde_json::json!({"n": 1}),
                returns: "x".to_string(),
                done_when: "x".to_string(),
                classification: crate::cassandra::types::DataClass::Public,
            }],
            result: None,
            data_ceiling: Some(crate::cassandra::types::DataClass::Public),
            refused: None,
            floor_request: None,
            l1_insight: None,
            l3_skill: None,
            invoke_skill: None,
            python_skill: None,
        }
    }

    #[test]
    fn a_run_round_trips_through_its_serialized_resume_state() {
        let plans = vec![
            PlanRecord::new(
                plan_with_tool("mail"),
                vec![StepOutcome::Ok(serde_json::json!({"sent": true}))],
            ),
            PlanRecord::new(
                plan_with_tool("shell"),
                vec![StepOutcome::Err {
                    code: "E".to_string(),
                    detail: "boom".to_string(),
                }],
            ),
        ];
        let advisories = vec!["watch the tone".to_string()];
        let blocks = vec!["no shell".to_string()];

        let restored =
            restore_resume_state(Some(&resume_state_from(&plans, &advisories, &blocks)));

        assert_eq!(restored.plans.len(), 2, "both plans come back, in order");
        assert_eq!(restored.plans[0].plan, plans[0].plan);
        assert_eq!(restored.plans[1].plan, plans[1].plan);
        assert_eq!(restored.plans[0].outcomes().len(), 1);
        assert_eq!(restored.plans[1].outcomes().len(), 1);
        assert!(restored.plans[1].outcomes()[0].is_err(), "the error outcome survives as one");
        // Reviewer feedback carries: without it the resumed planner repeats
        // mistakes the reviewer already corrected.
        assert_eq!(restored.advisories, advisories);
        assert_eq!(restored.blocks, blocks);
    }

    #[test]
    fn an_empty_run_round_trips_as_an_empty_run() {
        let restored = restore_resume_state(Some(&resume_state_from(&[], &[], &[])));
        assert!(restored.plans.is_empty());
        assert!(restored.advisories.is_empty());
        assert!(restored.blocks.is_empty());
    }

    #[test]
    fn the_serialized_shape_is_the_inputs_to_plan_record_new() {
        // Pinned because migration 0024's comment, the spec, and any future
        // reader of a live `asks.resume_state` all describe THIS shape. It
        // stores `plan` + `outcomes` — the constructor's inputs — and never
        // a screened render.
        let v = resume_state_from(
            &[PlanRecord::new(
                plan_with_tool("mail"),
                vec![StepOutcome::Ok(serde_json::json!("done"))],
            )],
            &["a".to_string()],
            &["b".to_string()],
        );
        let entry = &v["plans"][0];
        assert!(entry.get("plan").is_some(), "the plan itself is stored");
        assert!(entry.get("outcomes").is_some(), "so are its raw outcomes");
        assert!(
            entry.get("rendered").is_none(),
            "a screened render must never be persisted; the restore re-screens instead",
        );
        assert_eq!(v["advisories"], serde_json::json!(["a"]));
        assert_eq!(v["blocks"], serde_json::json!(["b"]));
    }

    #[test]
    fn a_missing_or_malformed_resume_state_restores_as_empty_rather_than_failing() {
        // Absent: an ask raised before migration 0024, or one that binds to
        // no run at all.
        for (label, value) in [
            ("absent", None),
            ("not an object", Some(serde_json::json!("nope"))),
            ("null", Some(serde_json::json!(null))),
            ("an empty object", Some(serde_json::json!({}))),
            ("plans is not an array", Some(serde_json::json!({"plans": 7}))),
            (
                "an entry missing its plan",
                Some(serde_json::json!({"plans": [{"outcomes": []}]})),
            ),
            (
                "an entry missing its outcomes",
                Some(serde_json::json!({"plans": [{"plan": {"decision": "act"}}]})),
            ),
            (
                "a plan that is not a Plan",
                Some(serde_json::json!({"plans": [{"plan": "act", "outcomes": []}]})),
            ),
        ] {
            let restored = restore_resume_state(value.as_ref());
            assert!(
                restored.plans.is_empty(),
                "{label}: an unusable resume_state must restore as an empty history, never                  fail the task — a lost history costs a replay, a failed task costs the                  operator's decision",
            );
        }
    }

    #[test]
    fn one_undecodable_plan_drops_the_whole_history_rather_than_reordering_it() {
        // All-or-nothing: keeping plan 2 while dropping plan 1 would tell the
        // planner it went straight from the first plan to the third.
        let good = serde_json::json!({"plan": {
            "context": "c", "decision": "act", "rationale": "r", "steps": [],
            "result": null, "data_ceiling": "Public", "refused": null,
            "floor_request": null, "l1_insight": null, "l3_skill": null,
            "invoke_skill": null, "python_skill": null
        }, "outcomes": []});
        let v = serde_json::json!({
            "plans": [good, {"plan": 7, "outcomes": []}],
            "advisories": ["kept"],
            "blocks": [],
        });
        let restored = restore_resume_state(Some(&v));
        assert!(restored.plans.is_empty(), "one bad entry drops the list");
        assert_eq!(
            restored.advisories,
            vec!["kept".to_string()],
            "advisories are independent of the plan list and still carry",
        );
    }

    #[test]
    fn advisories_and_blocks_tolerate_junk_without_taking_each_other_down() {
        let v = serde_json::json!({
            "plans": [],
            "advisories": ["ok", 7, null],
            "blocks": "not an array",
        });
        let restored = restore_resume_state(Some(&v));
        assert_eq!(
            restored.advisories,
            vec!["ok".to_string()],
            "non-string entries are skipped, the strings are kept",
        );
        assert!(restored.blocks.is_empty(), "a non-array field yields nothing");
    }

    #[test]
    fn resolution_choice_reads_the_two_offered_answers() {
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!({"choice": "approve"})), Some("d"))),
            Some(Choice::Approve)
        );
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!({"choice": "deny"})), Some("d"))),
            Some(Choice::Deny)
        );
    }

    #[test]
    fn resolution_choice_is_none_for_every_malformed_shape() {
        // `asks::resolve` enforces the closed set on the way in, so these
        // arms are defence in depth against a direct-SQL writer or a future
        // resolver. They must answer None — never Approve — because None is
        // the arm that leads to raising a fresh ask.
        assert_eq!(resolution_choice(&ask(None, Some("d"))), None, "no resolution");
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!("approve")), Some("d"))),
            None,
            "resolution is not an object"
        );
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!({"note": "ok"})), Some("d"))),
            None,
            "no choice key"
        );
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!({"choice": 1})), Some("d"))),
            None,
            "choice is not a string"
        );
        assert_eq!(
            resolution_choice(&ask(Some(serde_json::json!({"choice": "maybe"})), Some("d"))),
            None,
            "choice is not one this ask offered"
        );
    }

    #[test]
    fn a_choice_outside_this_asks_own_options_is_refused() {
        // The closed set is per-ask, not a global {approve,deny}: a future
        // kind offers different options, and validating against a constant
        // would silently accept the wrong vocabulary.
        let mut a = ask(Some(serde_json::json!({"choice": "approve"})), Some("d"));
        a.options = serde_json::json!(["yes", "no"]);
        assert_eq!(resolution_choice(&a), None);
    }

    #[test]
    fn an_offered_choice_this_module_does_not_understand_is_refused() {
        // "defer" is one of this ask's own `options`, so it clears the
        // `offered` guard and reaches the final match arm — unlike
        // "maybe" above, which never offered and is rejected earlier.
        // That arm must land on `None`, never on an unpinned `Approve`,
        // since `options` is free-form JSONB and a future ask kind is
        // exactly the case that would offer a third choice string.
        let mut a = ask(Some(serde_json::json!({"choice": "defer"})), Some("digest-a"));
        a.options = serde_json::json!(["approve", "deny", "defer"]);
        assert_eq!(resolution_choice(&a), None);
        assert_eq!(decide(&a, "digest-a"), AskDecision::NotForThisPlan);
    }

    #[test]
    fn decide_approves_only_the_digest_that_was_approved() {
        let a = ask(Some(serde_json::json!({"choice": "approve"})), Some("digest-a"));
        assert_eq!(decide(&a, "digest-a"), AskDecision::Approved);
        assert_eq!(decide(&a, "digest-b"), AskDecision::NotForThisPlan);
    }

    #[test]
    fn decide_denies_regardless_of_digest() {
        let a = ask(Some(serde_json::json!({"choice": "deny"})), Some("digest-a"));
        assert_eq!(decide(&a, "digest-a"), AskDecision::Denied);
        assert_eq!(decide(&a, "digest-b"), AskDecision::Denied);
    }

    #[test]
    fn an_approval_bound_to_no_digest_approves_nothing() {
        // plan_digest is nullable because a future kind need not bind to a
        // plan. Such an approval must never cover a plan by default.
        let a = ask(Some(serde_json::json!({"choice": "approve"})), None);
        assert_eq!(decide(&a, "digest-a"), AskDecision::NotForThisPlan);
    }

    #[test]
    fn a_malformed_resolution_never_reaches_the_approved_arm() {
        let a = ask(Some(serde_json::json!({"choice": "maybe"})), Some("digest-a"));
        assert_eq!(decide(&a, "digest-a"), AskDecision::NotForThisPlan);
    }

    #[test]
    fn ask_deadline_seconds_defaults_and_rejects_unusable_values() {
        assert_eq!(ask_deadline_seconds(None), DEFAULT_ASK_DEADLINE_S);
        assert_eq!(ask_deadline_seconds(Some("3600")), 3600);
        assert_eq!(ask_deadline_seconds(Some("  3600  ")), 3600);
        // A non-positive deadline would be in the past by the time `raise`
        // runs and the asks_deadline_after_created CHECK would reject the
        // INSERT, failing the escalation instead of asking the human.
        assert_eq!(ask_deadline_seconds(Some("0")), DEFAULT_ASK_DEADLINE_S);
        assert_eq!(ask_deadline_seconds(Some("-1")), DEFAULT_ASK_DEADLINE_S);
        assert_eq!(ask_deadline_seconds(Some("later")), DEFAULT_ASK_DEADLINE_S);
        assert_eq!(ask_deadline_seconds(Some("")), DEFAULT_ASK_DEADLINE_S);
    }
}
