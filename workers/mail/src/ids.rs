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

/// Mirrors `core::scheduler::inner_loop::summary::STEP_ERR_DETAIL_MAX` (200).
/// Not imported: that const is `pub(crate)` inside the `kastellan-core` crate,
/// and this worker is a separate process linked against neither `core` nor its
/// internal modules — the only channel between the two is the JSON-RPC wire.
/// `core`'s `plans_so_far_summary` clamps a failed step's `detail` — which is
/// exactly `"bad params: " + explain(...)` (see `handler::parse_params`) — to
/// `STEP_ERR_DETAIL_MAX` chars before it ever reaches the planner prompt, so
/// [`explain`]'s wording was hand-fitted to put its repair advice inside that
/// budget. `#[cfg(test)]`-only: nothing at runtime consults this number —
/// `the_repair_phrase_survives_the_core_side_planner_clamp` (below) is what
/// actually pins the guarantee, so the const would be dead weight in the real
/// binary.
/// Keep this in sync by hand if the upstream const ever moves.
#[cfg(test)]
const PLANNER_DETAIL_CLAMP: usize = 200;

/// What to tell the **planner** when a value is not a usable id.
///
/// Each arm names a mistake the live `audit_log` actually recorded, because
/// this string is fed back into the next planning iteration and a generic
/// "expected i64" has demonstrably not been enough: the same three mistakes
/// recurred across two months.
///
/// The class-specific diagnosis comes FIRST in every arm and the generic
/// "expected a numeric id" boilerplate is demoted to the tail: `core` clamps
/// this text (see `PLANNER_DETAIL_CLAMP`'s doc comment above) before it
/// reaches the planner, so whatever is placed after the clamp point is
/// silently dropped and never seen. Putting the repair advice first is what
/// makes it survive that clamp — see `the_repair_phrase_survives_the_core_side_planner_clamp`.
pub fn explain(v: &serde_json::Value) -> String {
    // Kept short deliberately: this is the part most at risk of being clamped
    // away, so it does not get to spend the budget the repair advice needs.
    const WANT: &str = "Expected the numeric message_id of a hit, e.g. 37477.";
    match v {
        serde_json::Value::String(s) if s.starts_with("{{") && s.ends_with("}}") => format!(
            "NO template substitution — write the literal id from the previous step's output, \
             not the placeholder {:?}. {WANT}",
            head(s)
        ),
        serde_json::Value::String(s) => format!(
            "{:?} is not a number; if it came from next_cursor, that is an opaque paging \
             token, not an id — re-read the message_id field of the hit instead. {WANT}",
            head(s)
        ),
        _ => format!("Got {}, not a valid message_id. {WANT}", head(&v.to_string())),
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

    /// Pins the actual guarantee, not just that the phrase appears *somewhere*
    /// in the full string: `core::scheduler::inner_loop::summary` clamps a step
    /// error's `detail` (`"bad params: " + explain(...)`) to
    /// `STEP_ERR_DETAIL_MAX = 200` chars before it ever reaches the planner
    /// prompt, so only the first `PLANNER_DETAIL_CLAMP - "bad params: ".len()`
    /// chars of `explain`'s output are ever seen. A phrase that shows up after
    /// that point is dropped just as surely as if it were never written.
    ///
    /// The budget-window checks alone are NOT an ordering guard: with the
    /// current (short) `WANT`, both the cursor and placeholder messages fit
    /// inside the budget regardless of which half comes first, so a future
    /// edit that reverts the ordering (while leaving `WANT` short) would pass
    /// those assertions and regress silently. The `.find(...) < .find(...)`
    /// checks below are what actually pin the ordering, independent of how
    /// long `WANT` is; the budget checks pin the fit. Neither alone is
    /// sufficient — see the review round 2 note in the task report.
    #[test]
    fn the_repair_phrase_survives_the_core_side_planner_clamp() {
        let budget = PLANNER_DETAIL_CLAMP - "bad params: ".len();
        // Unique to the generic tail — "Expected" never appears in either
        // class-specific arm, only inside `WANT`.
        const GENERIC_MARKER: &str = "Expected the numeric message_id";

        let cursor = explain(&json!("ZHwyMDI2LTA4LTA4VDIyOjAxOjU4KzAwOjAwfDM3NDc0"));
        let cursor_head: String = cursor.chars().take(budget).collect();
        assert!(cursor_head.contains("next_cursor"), "clamped to: {cursor_head:?}");
        assert!(cursor_head.contains("paging token"), "clamped to: {cursor_head:?}");
        let repair = cursor.find("paging token").expect("repair phrase present");
        let generic = cursor.find(GENERIC_MARKER).expect("generic text present");
        assert!(
            repair < generic,
            "the class-specific diagnosis must come FIRST, before the generic text, \
             or it is what core's clamp eats: {cursor}"
        );

        let placeholder = explain(&json!("{{message_id}}"));
        let placeholder_head: String = placeholder.chars().take(budget).collect();
        assert!(placeholder_head.contains("NO template substitution"), "clamped to: {placeholder_head:?}");
        assert!(placeholder_head.contains("literal id"), "clamped to: {placeholder_head:?}");
        // The placeholder arm is short enough that even the concrete example
        // survives the clamp too, not just the repair phrase.
        assert!(placeholder_head.contains("37477"), "clamped to: {placeholder_head:?}");
        let repair = placeholder.find("NO template substitution").expect("repair phrase present");
        let generic = placeholder.find(GENERIC_MARKER).expect("generic text present");
        assert!(
            repair < generic,
            "the class-specific diagnosis must come FIRST, before the generic text, \
             or it is what core's clamp eats: {placeholder}"
        );
    }

    #[test]
    fn a_long_rejected_string_is_truncated() {
        // This text rides into the planner's next prompt, so it must not grow
        // with the offending value. Bound on chars, not bytes: `head` truncates
        // by `chars().count()` and `{:?}` Debug-escapes the value, so a
        // byte-based bound could be tripped by escaping/non-ASCII width
        // unrelated to the truncation this test is pinning.
        let m = explain(&json!("x".repeat(500)));
        assert!(
            m.chars().count() < 300,
            "explanation must not grow with the value; got {} chars: {m}",
            m.chars().count()
        );
        assert!(m.contains('…'), "the value should be visibly truncated; got: {m}");
    }

    #[test]
    fn a_long_rejected_non_scalar_is_also_truncated() {
        // The catch-all arm renders `v` itself (via `Display`/`to_string`), not
        // just the two string arms — an array or object must be bounded too,
        // not just formatted unbounded onto the wire and into the audit log.
        let big = serde_json::Value::Array(vec![json!("x".repeat(500))]);
        let m = explain(&big);
        assert!(
            m.chars().count() < 300,
            "explanation must not grow with the value; got {} chars: {m}",
            m.chars().count()
        );
        assert!(m.contains('…'), "the value should be visibly truncated; got: {m}");
    }
}
