//! What an operator approval binds to (#564, spec D1/D2).
//!
//! When CASSANDRA escalates a plan, the ask records a **digest** of that
//! plan rather than the plan itself. On resume the agent replans from
//! scratch — `run_one` rebuilds `TaskContext` from the task payload with
//! `plan_count: 0`, so the escalated plan is gone — and goes through review
//! again as normal. If the new plan's digest matches the approved one, the
//! `Escalate` arm consults the resolved ask instead of raising a second one.
//! A *different* plan escalates afresh.
//!
//! That keeps "every plan is reviewed" intact with no bypass carve-out, and
//! closes the approve-plan-P-run-plan-P′ gap by construction.
//!
//! # What the digest covers, and why it is written as an exclusion
//!
//! **Excluded** — exactly four fields: plan-level `context` and
//! `rationale`, per-step `returns` and `done_when`. These are narration the
//! planner regenerates differently on every call, and none is read by
//! anything that acts: `dispatch_step` uses `tool`/`method`/`parameters`,
//! and `cassandra::deterministic` reads `classification` and
//! `data_ceiling`.
//!
//! **Included** — everything else, including fields whose relevance is not
//! obvious: `floor_request` (feeds `apply_floor_raise`, so it changes the
//! floor the whole plan is reviewed against), `result` (on a terminal plan
//! with no steps, the result IS what the operator approved), `decision`,
//! `refused`, and the four agent-emitted candidate fields `l1_insight`,
//! `l3_skill`, `invoke_skill`, and `python_skill` — none of these is
//! narration either, and a replan that keeps the same steps but swaps in a
//! different crystallisation candidate or skill invocation is not the plan
//! that was approved.
//!
//! **Stating it as an exclusion list is load-bearing.** An earlier draft
//! named the included fields and had already silently dropped
//! `floor_request`. An inclusion list makes *forgetting* the failure mode,
//! and forgetting fails unsafely — an approval carrying to a plan that
//! differs in the forgotten field. As an exclusion list, a new `Plan` field
//! defaults to counted, so a future omission merely re-escalates a plan
//! that did not need it.
//!
//! The trade-off still cuts both ways. Digest everything including prose
//! and it never matches on replan, so approvals never carry and the binding
//! is decorative. Digest too little and an approval covers a plan that does
//! something else.
//!
//! ⚠️ **This selection is PROVISIONAL and has to prove itself in real use.**
//! The revisit trigger is the first real escalation that re-escalates on a
//! semantically identical replan — boundary too wide, and with an exclusion
//! list that is the expected direction to be wrong in. The opposite signal,
//! an approval carrying to a plan the operator would not recognise, is far
//! more serious. Whichever fires first, re-derive this list from what
//! `dispatch_step` and `cassandra::deterministic` read *at that time*, not
//! from this comment.
//!
//! # Canonicality
//!
//! The digest is SHA-256 over `serde_json`'s serialization of a reduced
//! `Value`. This is canonical **because `serde_json::Map` is a `BTreeMap`**
//! — the `preserve_order` feature is not enabled anywhere in this workspace
//! — so object keys serialize in sorted order regardless of how the planner
//! happened to emit them. `parameter_key_insertion_order_does_not_change_the_digest`
//! is the tripwire that fires if anyone ever turns that feature on.

use sha2::{Digest, Sha256};

use super::types::Plan;

/// Lowercase hex SHA-256 (64 chars) over the plan's executable surface.
///
/// See the module docs for exactly which fields count and why. Two plans
/// that would execute identically produce the same digest even if their
/// prose differs entirely.
pub fn plan_digest(plan: &Plan) -> String {
    // `to_vec` on a Value built from owned data cannot fail: there are no
    // non-string map keys and no NaN/Inf floats can reach here from a
    // parsed plan (serde_json rejects them at parse time). `expect` rather
    // than a silent fallback — a digest that quietly became a constant
    // would make every approval match every plan.
    let bytes = serde_json::to_vec(&canonical_form(plan))
        .expect("canonical_form yields plain JSON values, which always serialize");

    let mut h = Sha256::new();
    h.update(&bytes);
    let digest = h.finalize();

    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        write!(s, "{b:02x}").expect("write to String cannot fail");
    }
    s
}

/// Reduce a plan to what the digest covers: everything except the four
/// narration fields.
///
/// Written as an explicit construction rather than a serialize-then-delete
/// so the compiler names any new `Plan` field here — a missing field is a
/// compile error at the destructuring below, not a silent exclusion.
fn canonical_form(plan: &Plan) -> serde_json::Value {
    // Destructured, so adding a field to `Plan` fails to compile until
    // someone decides whether it counts. `..` is deliberately NOT used.
    let Plan {
        context: _,   // narration — excluded
        rationale: _, // narration — excluded
        decision,
        steps,
        result,
        data_ceiling,
        refused,
        floor_request,
        l1_insight,
        l3_skill,
        invoke_skill,
        python_skill,
    } = plan;

    let steps: Vec<serde_json::Value> = steps
        .iter()
        .map(|s| {
            // Same treatment for PlannedStep.
            let crate::cassandra::types::PlannedStep {
                tool,
                method,
                parameters,
                returns: _,   // narration — excluded
                done_when: _, // narration — excluded
                classification,
            } = s;
            serde_json::json!({
                "tool":           tool,
                "method":         method,
                "parameters":     parameters,
                "classification": classification,
            })
        })
        .collect();

    serde_json::json!({
        // Every Option serializes to `null` when absent, deliberately:
        // absence must not digest the same as any present value (#506's
        // "absence is not a value" lesson, applied here to `data_ceiling`
        // and `floor_request` alike).
        "decision":      decision,
        "steps":         steps,
        "result":        result,
        "data_ceiling":  data_ceiling,
        "refused":       refused,
        "floor_request": floor_request,
        "l1_insight":    l1_insight,
        "l3_skill":      l3_skill,
        "invoke_skill":  invoke_skill,
        "python_skill":  python_skill,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::{
        DataClass, InvokeDirective, L3Param, L3SkillCandidate, L3TemplateStep, Plan,
        PlannedStep, PythonSkillCandidate, RefusedReason,
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    /// A two-step plan used as the baseline across these tests.
    fn base_plan() -> Plan {
        Plan {
            context: "the user asked about flights".into(),
            decision: "continue".into(),
            rationale: "search mail first, then read the hit".into(),
            steps: vec![
                PlannedStep {
                    tool: "mail".into(),
                    method: "mail.search".into(),
                    parameters: json!({"query": "Qantas", "sort": "date"}),
                    returns: "a list of hits".into(),
                    done_when: "hits are non-empty".into(),
                    classification: DataClass::Personal,
                },
                PlannedStep {
                    tool: "mail".into(),
                    method: "mail.get_message".into(),
                    parameters: json!({"message_id": "20973"}),
                    returns: "the message body".into(),
                    done_when: "body is present".into(),
                    classification: DataClass::Personal,
                },
            ],
            result: None,
            data_ceiling: Some(DataClass::Personal),
            refused: None,
            floor_request: None,
            l1_insight: None,
            l3_skill: None,
            invoke_skill: None,
            python_skill: None,
        }
    }

    #[test]
    fn identical_plans_digest_identically() {
        assert_eq!(plan_digest(&base_plan()), plan_digest(&base_plan()));
    }

    #[test]
    fn digest_is_64_lowercase_hex_chars() {
        let d = plan_digest(&base_plan());
        assert_eq!(d.len(), 64, "sha256 hex is 64 chars: {d}");
        assert!(
            d.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "digest must be lowercase hex: {d}",
        );
    }

    // ---- narration is EXCLUDED (spec D2) ----------------------------------
    //
    // These four fields are regenerated differently by the planner on every
    // call and none of them is read by `dispatch_step`. If they counted, the
    // digest would essentially never match on replan and the whole binding
    // would be decorative.

    #[test]
    fn plan_level_narration_does_not_change_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.context = "totally different framing of the same request".into();
        p.rationale = "a completely rewritten rationale".into();
        assert_eq!(plan_digest(&p), before, "context/rationale must not count");
    }

    #[test]
    fn step_level_narration_does_not_change_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.steps[0].returns = "some other prose".into();
        p.steps[0].done_when = "some other condition prose".into();
        assert_eq!(plan_digest(&p), before, "returns/done_when must not count");
    }

    // ---- the executable surface is INCLUDED -------------------------------
    //
    // One test per field, so a future refactor that drops a field from the
    // canonical form fails on exactly that field rather than on a blanket
    // assertion that names nothing.

    #[test]
    fn changing_a_step_tool_changes_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.steps[0].tool = "web-fetch".into();
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn changing_a_step_method_changes_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.steps[0].method = "mail.list_folders".into();
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn changing_a_step_parameter_value_changes_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.steps[0].parameters = json!({"query": "Jetstar", "sort": "date"});
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn changing_a_step_classification_changes_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.steps[0].classification = DataClass::Secret;
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn changing_the_data_ceiling_changes_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.data_ceiling = Some(DataClass::Secret);
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn an_absent_data_ceiling_differs_from_a_present_one() {
        // #506's lesson: absence is not a value. A plan that omits the
        // ceiling must not digest the same as one that declares the
        // floor-resolved value, or an approval could carry across the
        // resolution boundary.
        let mut p = base_plan();
        p.data_ceiling = None;
        assert_ne!(plan_digest(&p), plan_digest(&base_plan()));
    }

    #[test]
    fn changing_the_floor_request_changes_the_digest() {
        // `floor_request` feeds `apply_floor_raise`, which changes the
        // classification floor the whole plan is reviewed against — so a
        // plan that drops one is materially different from the plan that
        // was approved. This is the field an inclusion-list formulation
        // silently omitted; see spec D2.
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.floor_request = Some(DataClass::ClinicalConfidential);
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn changing_the_refusal_changes_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.refused = Some(RefusedReason {
            principle: 3,
            reason: "would_disclose_clinical_data_without_consent".into(),
        });
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn changing_the_l1_insight_changes_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.l1_insight = Some("the user always books outbound flights before return legs".into());
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn changing_the_l3_skill_candidate_changes_the_digest() {
        // l3_skill crystallises a new reusable tool-call template into the
        // skill layer. An approval must not carry to a replan that
        // proposes a DIFFERENT template than the one the operator saw —
        // that would mean approving one skill and storing another.
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.l3_skill = Some(L3SkillCandidate {
            name: "flight_confirmation_lookup_skill".into(),
            description: "look up a flight confirmation email and read it".into(),
            parameters: vec![L3Param {
                name: "airline_name".into(),
                description: "which airline to search mail for".into(),
            }],
            steps: vec![L3TemplateStep {
                tool: "calendar".into(),
                method: "calendar.search".into(),
                parameters: json!({"query": "{{airline_name}}"}),
            }],
        });
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn changing_the_invoke_directive_changes_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.invoke_skill = Some(InvokeDirective {
            name: "flight_confirmation_invoke_directive".into(),
            args: BTreeMap::from([("carrier".to_string(), "Emirates".to_string())]),
            params: serde_json::Value::Null,
        });
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn changing_the_python_skill_candidate_changes_the_digest() {
        // python_skill is stored and later executed byte-for-byte
        // unchanged, so an approval must not carry to a replan that
        // crystallises DIFFERENT code than what the operator reviewed.
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.python_skill = Some(PythonSkillCandidate {
            name: "flight_confirmation_python_skill".into(),
            description: "python snippet that finds and prints the newest flight confirmation"
                .into(),
            code: "print('newest flight confirmation')".into(),
        });
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn changing_the_decision_changes_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.decision = "task_complete".into();
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn changing_a_terminal_plans_result_changes_the_digest() {
        // A terminal plan has no steps, so its `result` IS what the
        // operator would be approving. Excluding it would let an approval
        // carry from "your balance is X" to "your balance is Y".
        let mut a = base_plan();
        a.steps.clear();
        a.result = Some(json!({"kind": "text", "body": "answer one"}));
        let mut b = base_plan();
        b.steps.clear();
        b.result = Some(json!({"kind": "text", "body": "answer two"}));
        assert_ne!(plan_digest(&a), plan_digest(&b));
    }

    #[test]
    fn dropping_a_step_changes_the_digest() {
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.steps.truncate(1);
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn reordering_steps_changes_the_digest() {
        // Step order is execution order, so it is part of what was approved:
        // "search then read" is not "read then search".
        let before = plan_digest(&base_plan());
        let mut p = base_plan();
        p.steps.swap(0, 1);
        assert_ne!(plan_digest(&p), before);
    }

    #[test]
    fn an_empty_step_list_digests_stably_and_differs_from_a_populated_one() {
        let mut p = base_plan();
        p.steps.clear();
        let mut q = base_plan();
        q.steps.clear();
        assert_eq!(plan_digest(&p), plan_digest(&q));
        assert_ne!(plan_digest(&p), plan_digest(&base_plan()));
    }

    // ---- canonicality ------------------------------------------------------

    #[test]
    fn parameter_key_insertion_order_does_not_change_the_digest() {
        // LOAD-BEARING, and it guards a Cargo feature rather than our code.
        // `serde_json::Map` is a BTreeMap only while the `preserve_order`
        // feature is OFF — which it is nowhere in this workspace. Enabling it
        // anywhere would make Map an IndexMap, object keys would serialize in
        // insertion order, and two logically identical plans would digest
        // differently — silently retiring every outstanding approval. This
        // test is the tripwire for that.
        let mut a = base_plan();
        a.steps[0].parameters = json!({"query": "Qantas", "sort": "date"});
        let mut b = base_plan();
        b.steps[0].parameters = json!({"sort": "date", "query": "Qantas"});
        assert_eq!(
            plan_digest(&a),
            plan_digest(&b),
            "object key order must not affect the digest — is serde_json's \
             `preserve_order` feature enabled somewhere in the workspace?",
        );
    }

    #[test]
    fn nested_parameter_key_order_does_not_change_the_digest() {
        let mut a = base_plan();
        a.steps[0].parameters = json!({"filters": {"account_ids": [1], "folder_ids": [2]}});
        let mut b = base_plan();
        b.steps[0].parameters = json!({"filters": {"folder_ids": [2], "account_ids": [1]}});
        assert_eq!(plan_digest(&a), plan_digest(&b));
    }

    #[test]
    fn array_order_inside_parameters_is_significant() {
        // Arrays are ordered data, unlike object keys. `[1,2]` and `[2,1]`
        // are different arguments and must not share an approval.
        let mut a = base_plan();
        a.steps[0].parameters = json!({"ids": [1, 2]});
        let mut b = base_plan();
        b.steps[0].parameters = json!({"ids": [2, 1]});
        assert_ne!(plan_digest(&a), plan_digest(&b));
    }
}
