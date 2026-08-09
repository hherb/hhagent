//! Ids in the two shapes localmail actually puts on the wire.
//!
//! localmail serialises **every** id as a JSON string — `"message_id":"37477"`
//! on `/v1/search` and `/v1/messages`, `"id":"1"` on `/v1/accounts` — while this
//! worker interpolates ids into URL paths and so must keep them strictly
//! numeric. Before this type the params took a bare `i64`, so the planner
//! copying an id straight out of a search hit (which is what it does, and the
//! only sane thing for it to do) produced `invalid type: string "17817",
//! expected i64`: 7 of the 14 live `mail.get_message` failures.
//!
//! `LocalmailId` widens the *accepted JSON types* while keeping the *validated
//! output* an `i64`, so the set of values that can reach a URL path is exactly
//! what it was before — this is not a loosening of the traversal guard.
//!
//! The widening stops at the two forms localmail emits. No trimming, no sign,
//! no floats: the type stays a validator, not a repair layer. A value that is
//! not an id gets [`explain`], which is written for the planner rather than for
//! a log reader — `inner_loop` feeds a failed step's error back on the next
//! iteration, so this text is the only chance to correct the mistake.

use std::fmt;

/// A localmail row id, accepted as `37477` or `"37477"`, always yielded as `i64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalmailId(i64);

impl LocalmailId {
    /// The validated numeric id.
    pub fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for LocalmailId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<'de> serde::Deserialize<'de> for LocalmailId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        parse_id(&v).map(LocalmailId).map_err(serde::de::Error::custom)
    }
}

/// Pure: a JSON value → a validated non-negative id, or the planner-facing
/// reason it is not one.
///
/// Accepts a non-negative integer JSON number, or a string of ASCII digits.
/// Everything else — signs, surrounding whitespace, the empty string, floats,
/// negatives, `i64` overflow, and every non-scalar — is refused.
pub fn parse_id(v: &serde_json::Value) -> Result<i64, String> {
    match v {
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) if i >= 0 => Ok(i),
            _ => Err(explain(v)),
        },
        serde_json::Value::String(s) => {
            if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                return Err(explain(v));
            }
            // Digits-only and non-empty, so the only remaining failure is
            // overflowing i64.
            s.parse::<i64>().map_err(|_| explain(v))
        }
        _ => Err(explain(v)),
    }
}

/// What to tell the **planner** when a value is not a usable id.
///
/// Each arm names a mistake the live `audit_log` actually recorded, because
/// this string is fed back into the next planning iteration and a generic
/// "expected i64" has demonstrably not been enough: the same three mistakes
/// recurred across two months.
pub fn explain(v: &serde_json::Value) -> String {
    const WANT: &str = "expected the numeric message_id of a mail.search / \
                        mail.list_messages hit, e.g. 37477 (a number, or a string of digits)";
    match v {
        serde_json::Value::String(s) if s.starts_with("{{") && s.ends_with("}}") => format!(
            "{WANT}. Got the placeholder {:?}: there is NO template substitution in this \
             system — write the literal id from the previous step's output.",
            head(s)
        ),
        serde_json::Value::String(s) => format!(
            "{WANT}. Got {:?}, which is not a number. If this came from next_cursor, that is \
             an opaque paging token and not an id — re-read the message_id field of the hit.",
            head(s)
        ),
        _ => format!("{WANT}. Got {v}."),
    }
}

/// Keep a rejected value short. The cursor cases are long base64/hex blobs and
/// this text goes into the planner's next prompt, where tokens are the budget.
fn head(s: &str) -> String {
    const MAX: usize = 48;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(MAX).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- the accepted grammar, one test per row of the spec's table ---

    #[test]
    fn a_json_number_is_accepted() {
        assert_eq!(parse_id(&json!(37477)), Ok(37477));
    }

    #[test]
    fn a_digit_string_is_accepted() {
        // The shape localmail actually emits, and the 7-of-14 failure case.
        assert_eq!(parse_id(&json!("37477")), Ok(37477));
    }

    #[test]
    fn leading_zeros_are_accepted() {
        // localmail never emits this, but it is unambiguous.
        assert_eq!(parse_id(&json!("0037")), Ok(37));
    }

    #[test]
    fn a_signed_string_is_rejected() {
        // Sign characters are not digits, and no row id is negative.
        assert!(parse_id(&json!("-1")).is_err());
        assert!(parse_id(&json!("+1")).is_err());
    }

    #[test]
    fn surrounding_whitespace_is_rejected_not_trimmed() {
        // Not trimming is deliberate: whitespace means the value came from
        // somewhere it should not have, and repairing it would hide that.
        assert!(parse_id(&json!(" 37477")).is_err());
        assert!(parse_id(&json!("37477 ")).is_err());
    }

    #[test]
    fn the_empty_string_is_rejected() {
        assert!(parse_id(&json!("")).is_err());
    }

    #[test]
    fn a_float_is_rejected() {
        assert!(parse_id(&json!(37.0)).is_err());
        assert!(parse_id(&json!(3.5)).is_err());
    }

    #[test]
    fn a_negative_number_is_rejected() {
        assert!(parse_id(&json!(-1)).is_err());
    }

    #[test]
    fn an_overflowing_digit_string_is_rejected() {
        assert!(parse_id(&json!("99999999999999999999999")).is_err());
    }

    #[test]
    fn non_scalars_and_null_are_rejected() {
        assert!(parse_id(&json!(null)).is_err());
        assert!(parse_id(&json!(true)).is_err());
        assert!(parse_id(&json!([1])).is_err());
        assert!(parse_id(&json!({"id": 1})).is_err());
    }

    // --- the planner-facing text: one test per live failure class ---

    #[test]
    fn a_template_placeholder_is_told_there_is_no_substitution() {
        // 4 of the 14 live failures were exactly this value.
        let m = explain(&json!("{{message_id}}"));
        assert!(m.contains("NO template substitution"), "got: {m}");
        assert!(m.contains("literal id"), "got: {m}");
    }

    #[test]
    fn a_paging_cursor_is_named_as_a_cursor() {
        // 3 of the 14 live failures pasted next_cursor in here.
        let m = explain(&json!("ZHwyMDI2LTA4LTA4VDIyOjAxOjU4KzAwOjAwfDM3NDc0"));
        assert!(m.contains("next_cursor"), "got: {m}");
        assert!(m.contains("paging token"), "got: {m}");
    }

    #[test]
    fn every_explanation_names_the_field_and_gives_an_example() {
        for v in [json!("{{x}}"), json!("ZHwy"), json!(null), json!(-1)] {
            let m = explain(&v);
            assert!(m.contains("message_id"), "must name the field; got: {m}");
            assert!(m.contains("37477"), "must give a concrete example; got: {m}");
        }
    }

    #[test]
    fn a_long_rejected_value_is_truncated() {
        // This text rides into the planner's next prompt, so it must not grow
        // with the offending value. The bound is generous on purpose: the fixed
        // prose is already ~280 chars, and pinning it tighter would make an
        // ordinary wording edit fail this test for no reason. What is being
        // asserted is that 500 chars of input do NOT reach the output.
        let m = explain(&json!("x".repeat(500)));
        assert!(m.len() < 400, "explanation must not grow with the value; got {} chars", m.len());
        assert!(m.contains('…'), "the value should be visibly truncated; got: {m}");
    }
}
