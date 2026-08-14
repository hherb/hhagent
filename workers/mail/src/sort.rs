//! Ordering resolution and planner-facing ordering advice for `mail.search`.
//!
//! # Why this module exists
//!
//! `mail.search` ranks by relevance unless asked otherwise, and relevance order
//! is emphatically not date order. Measured against the live 37k-message archive
//! (issue #559), a `Qantas` search returns hits dated 2019 → 2025 interleaved,
//! with marketing mail scoring above actual booking confirmations. Two live runs
//! asked "what was my *most recent* Qantas flight booking?", neither passed
//! `sort`, and both answered from a relevance ranking — giving two different
//! wrong answers.
//!
//! The capability was never missing: `sort: "date"` exists, works, and was
//! already advertised. What was missing is anything that makes the *default's
//! unsuitability* visible at the moment it matters.
//!
//! # Why the advice rides in the response, not only in the parameter docs
//!
//! Hardening a parameter description is the cheap fix, and this change makes it
//! too (see `core/src/workers/mail.rs`). But on this exact tool that remedy has
//! a measured failure: #536 rewrote `mail.get_message`'s `message_id`
//! description to say "use the literal value from the previous step's output,
//! **not a placeholder**", shipped 2026-08-09 — and the planner still invented a
//! 16-hex `message_id` in both later runs. What *did* work in one of those runs
//! was [`crate::ids`]'s error-time `explain` text: the planner read it and
//! repaired itself on the next plan.
//!
//! So the lever that has actually moved this planner is text delivered **where
//! it reads results**, not text in the advertisement. Hence [`annotate`].
//!
//! # Two constraints the wording and the key name have to satisfy
//!
//! A successful step's output does not reach the planner verbatim. `core`'s
//! `injection_guard::extract_scannable_text` walks the JSON and emits **only
//! string values, with their keys discarded**, newline-separated, capped at
//! `STEP_OK_SUMMARY_MAX` (4 KiB) for this method. Therefore:
//!
//! 1. **The advice must be a self-describing sentence.** A tidy
//!    `"sort_applied": "rank"` would reach the planner as the bare word `rank`
//!    on a line of its own, with nothing saying what it describes.
//! 2. **The key must sort before `results`.** `serde_json::Map` is a `BTreeMap`
//!    here (no `preserve_order` feature in this workspace), so keys serialize
//!    alphabetically, and a 50-hit `results` array exhausts the 4 KiB budget on
//!    its own. `ordering_note` < `results`; `sort_applied` would have sorted
//!    *after* it and been silently clipped — the same trap that swallowed #536's
//!    repair advice. [`ordering_key_sorts_before_results`] pins this, and
//!    `core`'s `mail_ordering_note_survives_the_planner_head_cap` proves it
//!    end-to-end against the real extractor.

use serde_json::Value;

/// The sort this worker asks for when the planner names none.
///
/// localmail's own default is already `rank`, so sending it explicitly changes
/// no result. It is sent anyway so that what we *advertise* as the default and
/// what we *request* are the same fact, established here rather than inherited
/// from another service's unpinned behaviour — and so [`annotate`] can describe
/// the ordering truthfully without assuming anything about the server.
pub const DEFAULT_SORT: &str = "rank";

/// Response key carrying [`ordering_note`]'s sentence.
///
/// **Must sort lexicographically before `"results"`** — see the module docs.
pub const ORDERING_KEY: &str = "ordering_note";

/// The sort to request, given what the planner asked for.
///
/// Pure. Unknown values are passed through untouched rather than corrected:
/// localmail validates the field server-side (a bogus sort is a 422, measured),
/// and silently rewriting an unrecognised value would answer a question the
/// planner did not ask.
pub fn resolve_sort(requested: Option<&str>) -> &str {
    match requested {
        Some(s) if !s.is_empty() => s,
        _ => DEFAULT_SORT,
    }
}

/// One sentence telling the planner what order it is looking at, and what to do
/// about it if that is the wrong order for the question.
///
/// Written for the planner rather than for a log reader, in the same spirit as
/// [`crate::ids`]'s `explain`. The `rank` arm is the load-bearing one: it is the
/// default, so it is the arm that fires on the query that has already gone wrong
/// twice, and it names both the parameter and the value to pass.
pub fn ordering_note(sort: &str) -> String {
    match sort {
        "date" => "These results are in date order, newest first.".to_string(),
        "rank" => "These results are in rank order (best match first), NOT date order: \
                   hits may be from any year and the first hit is not the most recent. \
                   To answer a 'most recent' or 'latest' question, call mail.search \
                   again with sort: \"date\"."
            .to_string(),
        other => format!(
            "These results use the requested sort {other:?}, whose ordering this worker \
             cannot describe. If the question is about recency, call mail.search again \
             with sort: \"date\"."
        ),
    }
}

/// Attach [`ordering_note`] to a `/v1/search` response under [`ORDERING_KEY`].
///
/// A no-op unless `response` is a JSON object, and it never overwrites a key
/// localmail already serves: if the service one day describes its own ordering,
/// that answer is authoritative and this worker's inference is not. Everything
/// else in the response is passed through untouched — the worker reshapes no
/// part of `results`, which is what keeps a fabricated id attributable to the
/// planner rather than to us.
pub fn annotate(response: &mut Value, sort: &str) {
    let Some(map) = response.as_object_mut() else { return };
    if map.contains_key(ORDERING_KEY) {
        return;
    }
    map.insert(ORDERING_KEY.to_string(), Value::String(ordering_note(sort)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn absent_or_empty_sort_resolves_to_the_advertised_default() {
        assert_eq!(resolve_sort(None), "rank");
        assert_eq!(resolve_sort(Some("")), "rank");
    }

    #[test]
    fn an_explicit_sort_is_passed_through() {
        assert_eq!(resolve_sort(Some("date")), "date");
        // Not corrected here — localmail 422s an unknown sort (measured live).
        assert_eq!(resolve_sort(Some("newest")), "newest");
    }

    /// The default's note has to be *actionable*, not merely accurate: #559's
    /// whole point is that `'rank' (default) or 'date'` was already true and
    /// still left the planner with no reason to change anything.
    #[test]
    fn the_rank_note_names_both_the_parameter_and_the_value_to_pass() {
        let note = ordering_note("rank");
        assert!(note.contains("sort"), "note must name the parameter: {note}");
        assert!(note.contains("\"date\""), "note must name the value: {note}");
        assert!(note.contains("NOT date order"), "note must state the consequence: {note}");
    }

    #[test]
    fn the_date_note_states_newest_first() {
        let note = ordering_note("date");
        assert!(note.contains("newest first"), "{note}");
    }

    /// An unrecognised sort must not silently claim an ordering this worker
    /// cannot vouch for.
    #[test]
    fn an_unknown_sort_note_admits_it_cannot_describe_the_order() {
        let note = ordering_note("sideways");
        assert!(note.contains("cannot describe"), "{note}");
        assert!(note.contains("sideways"), "note must quote the sort it got: {note}");
    }

    /// The placement invariant, pinned locally. `serde_json::Map` is a
    /// `BTreeMap` in this workspace, so serialization order is key order, and
    /// anything sorting after `results` is clipped by the planner's 4 KiB head
    /// cap before it is ever read. `core` proves the same thing end-to-end
    /// against the real extractor; this test is what fails first, in the crate
    /// where someone would rename the key.
    #[test]
    fn ordering_key_sorts_before_results() {
        assert!(
            ORDERING_KEY < "results",
            "{ORDERING_KEY} must sort before `results` or the head cap clips it"
        );
    }

    #[test]
    fn annotate_adds_a_self_describing_sentence() {
        let mut v = json!({"results": [], "next_cursor": null});
        annotate(&mut v, "rank");
        let note = v[ORDERING_KEY].as_str().expect("note must be a string");
        assert_eq!(note, ordering_note("rank"));
    }

    /// If localmail ever describes its own ordering, the service wins.
    #[test]
    fn annotate_never_overwrites_a_key_the_service_already_served() {
        let mut v = json!({"results": [], ORDERING_KEY: "served by localmail"});
        annotate(&mut v, "rank");
        assert_eq!(v[ORDERING_KEY], json!("served by localmail"));
    }

    #[test]
    fn annotate_leaves_a_non_object_response_alone() {
        let mut v = json!(["not", "an", "object"]);
        annotate(&mut v, "rank");
        assert_eq!(v, json!(["not", "an", "object"]));
    }

    /// The whole point of the sentence form: the planner sees values with their
    /// keys stripped, so the text has to carry its own subject.
    #[test]
    fn the_note_is_intelligible_with_its_key_removed() {
        let note = ordering_note("rank");
        assert!(
            note.starts_with("These results"),
            "note must name its own subject, since the key is discarded: {note}"
        );
    }
}
