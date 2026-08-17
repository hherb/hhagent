//! Getting `mail.search`'s id filters into the shape localmail accepts.
//!
//! Two independent traps, both measured live on 2026-08-17 (task 161, which
//! spent its entire six-iteration budget here and returned a 2022 flight for a
//! question about the most recent one).
//!
//! **1. Where the ids go.** `mail.list_messages` takes `account_ids` and
//! `folder_ids` at the *top level*; `/v1/search` wants them nested inside
//! `filters`. The planner carried the shape across from one tool to its
//! sibling — twice — and got `unknown field \`account_ids\`` both times. That
//! is not a planner mistake so much as an inconsistency in the surface it was
//! given, so the top-level form is now accepted here and folded inward.
//!
//! **2. What type they are.** localmail's `SearchFiltersModel` types both as
//! `list[str]` (`serve/routes/search.py`), so an integer id — the obvious thing
//! to write, and what `mail.list_messages` itself emits into a query string —
//! comes back as a raw FastAPI 422 validation envelope:
//! `{"detail":[{"type":"string_type","loc":["body","filters","account_ids",0],…}]}`.
//! That envelope is not planner-facing advice; it is barely reader-facing. Ids
//! are coerced to their canonical digit-string form here instead.
//!
//! Both forms are *validated*, not merely reshaped: an id that is not an id is
//! refused with [`crate::ids`]'s repair text, so widening the accepted shape
//! does not widen what can reach localmail.

use crate::ids::LocalmailId;

/// Fold the top-level id filters into `filters`, and normalise any already
/// nested there, returning the object to send as `filters` (or `None`).
///
/// Naming an id filter in *both* places is refused rather than resolved by
/// precedence: the two could disagree, and silently preferring one would run a
/// search the planner did not ask for while telling it nothing — the same
/// argument `attach::choose` makes about two selectors.
pub fn normalize_filters(
    filters: Option<serde_json::Value>,
    account_ids: Option<Vec<LocalmailId>>,
    folder_ids: Option<Vec<LocalmailId>>,
) -> Result<Option<serde_json::Value>, String> {
    let mut obj = match filters {
        None => serde_json::Map::new(),
        Some(serde_json::Value::Object(m)) => m,
        Some(_other) => {
            return Err("`filters` must be an object, e.g. \
                        {\"account_ids\": [\"1\"], \"has_attachment\": true}."
                .to_string())
        }
    };

    for (key, top) in [("account_ids", account_ids), ("folder_ids", folder_ids)] {
        let nested = obj.get(key);
        match (top, nested) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "`{key}` was given both at the top level and inside `filters` — \
                     pass it once. Either place works."
                ))
            }
            (Some(ids), None) => {
                let as_strings: Vec<serde_json::Value> =
                    ids.iter().map(|i| serde_json::json!(i.to_string())).collect();
                obj.insert(key.to_string(), serde_json::Value::Array(as_strings));
            }
            (None, Some(v)) => {
                // Already nested, but possibly as numbers — which localmail
                // answers with a 422 the planner cannot act on.
                let coerced = crate::ids::id_strings(key, v)?;
                let as_values: Vec<serde_json::Value> =
                    coerced.into_iter().map(serde_json::Value::String).collect();
                obj.insert(key.to_string(), serde_json::Value::Array(as_values));
            }
            (None, None) => {}
        }
    }

    Ok(if obj.is_empty() { None } else { Some(serde_json::Value::Object(obj)) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build validated ids the way the params struct does.
    fn ids(vals: &[i64]) -> Vec<LocalmailId> {
        #[derive(serde::Deserialize)]
        struct P {
            #[serde(default, deserialize_with = "crate::ids::account_ids")]
            account_ids: Option<Vec<LocalmailId>>,
        }
        let p: P = serde_json::from_value(json!({ "account_ids": vals })).unwrap();
        p.account_ids.unwrap()
    }

    fn as_the_planner_sees_it(s: &str) -> String {
        s.chars().take(kastellan_protocol::STEP_ERR_DETAIL_MAX).collect()
    }

    /// The live shape from task 161, twice: `account_ids` where
    /// `mail.list_messages` takes it. It is now folded into `filters`.
    #[test]
    fn top_level_account_ids_are_folded_into_filters() {
        let out = normalize_filters(None, Some(ids(&[1])), None).unwrap().unwrap();
        assert_eq!(out["account_ids"], json!(["1"]));
    }

    #[test]
    fn top_level_folder_ids_are_folded_into_filters() {
        let out = normalize_filters(None, None, Some(ids(&[7, 9]))).unwrap().unwrap();
        assert_eq!(out["folder_ids"], json!(["7", "9"]));
    }

    /// The other half of task 161: nested but numeric, which localmail answers
    /// with a raw 422. Strings are what its model accepts.
    #[test]
    fn numeric_ids_already_inside_filters_are_coerced_to_strings() {
        let out = normalize_filters(Some(json!({"account_ids": [1, 2]})), None, None)
            .unwrap()
            .unwrap();
        assert_eq!(out["account_ids"], json!(["1", "2"]));
    }

    #[test]
    fn string_ids_inside_filters_are_left_as_strings() {
        let out = normalize_filters(Some(json!({"account_ids": ["1"]})), None, None)
            .unwrap()
            .unwrap();
        assert_eq!(out["account_ids"], json!(["1"]));
    }

    /// Everything that is not an id filter is forwarded untouched — this
    /// function normalises two keys, it does not own the filter vocabulary.
    #[test]
    fn other_filters_are_passed_through_unchanged() {
        let out = normalize_filters(
            Some(json!({"has_attachment": true, "subject": "flight", "account_ids": [1]})),
            None,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(out["has_attachment"], json!(true));
        assert_eq!(out["subject"], json!("flight"));
        assert_eq!(out["account_ids"], json!(["1"]));
    }

    #[test]
    fn no_filters_at_all_stays_absent() {
        assert!(normalize_filters(None, None, None).unwrap().is_none());
    }

    /// An empty object must not become `filters: {}` on the wire — absent and
    /// "filter by nothing" should look the same to localmail.
    #[test]
    fn an_empty_filters_object_collapses_to_absent() {
        assert!(normalize_filters(Some(json!({})), None, None).unwrap().is_none());
    }

    #[test]
    fn naming_an_id_filter_twice_is_refused_rather_than_resolved_by_precedence() {
        let e = normalize_filters(Some(json!({"account_ids": ["1"]})), Some(ids(&[2])), None)
            .unwrap_err();
        assert!(
            as_the_planner_sees_it(&e).contains("account_ids"),
            "must name the doubled parameter: {e}"
        );
    }

    #[test]
    fn a_non_object_filters_is_refused_with_an_example() {
        let e = normalize_filters(Some(json!("account_ids=1")), None, None).unwrap_err();
        let seen = as_the_planner_sees_it(&e);
        assert!(seen.contains("object"), "{seen}");
        assert!(seen.contains("account_ids"), "must show the shape: {seen}");
    }

    /// Widening the accepted shape must not widen what reaches localmail: a
    /// nested value that is not an id is still refused, with `ids`' repair text.
    #[test]
    fn a_nested_non_id_is_still_refused_with_repair_advice() {
        let e = normalize_filters(Some(json!({"account_ids": ["{{account_id}}"]})), None, None)
            .unwrap_err();
        assert!(
            as_the_planner_sees_it(&e).contains("NO template substitution"),
            "got: {e}"
        );
        let e2 =
            normalize_filters(Some(json!({"folder_ids": [-1]})), None, None).unwrap_err();
        assert!(as_the_planner_sees_it(&e2).contains("folder_ids"), "got: {e2}");
    }

    /// A nested id filter that is not even a list is a shape error, not a panic.
    #[test]
    fn a_nested_id_filter_that_is_not_a_list_is_refused() {
        let e = normalize_filters(Some(json!({"account_ids": "1"})), None, None).unwrap_err();
        assert!(!e.is_empty());
        let e2 = normalize_filters(Some(json!({"account_ids": null})), None, None).unwrap_err();
        assert!(!e2.is_empty());
    }

    #[test]
    fn no_message_grows_past_the_planner_clamp() {
        let long = "x".repeat(400);
        let cases = vec![
            normalize_filters(Some(json!(long.clone())), None, None).unwrap_err(),
            normalize_filters(Some(json!({"account_ids": ["1"]})), Some(ids(&[2])), None)
                .unwrap_err(),
            normalize_filters(Some(json!({"account_ids": [long.clone()]})), None, None)
                .unwrap_err(),
        ];
        for m in &cases {
            assert!(
                m.chars().count() <= kastellan_protocol::STEP_ERR_DETAIL_MAX,
                "{} chars exceeds the clamp: {m}",
                m.chars().count()
            );
        }
    }
}
