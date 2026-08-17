//! Qualifying a plan step's method against the tool's advertised methods.
//!
//! A `PlannedStep` carries `tool` and `method` separately, and the method is a
//! JSON-RPC name that repeats the namespace: `{tool: "mail", method:
//! "mail.get_attachment_text"}`. The planner has to write that prefix itself,
//! and measurably does not always do it — live tasks 160 and 162 (2026-08-17)
//! both dispatched a bare `get_attachment_text`, four times between them, each
//! answered `-32601 unknown method` by a worker that implements exactly that
//! method one prefix away. Task 162 spent its entire six-iteration budget on it
//! and returned "I was unable to read the contents of the attached PDF".
//!
//! The prefix is redundant information: `step.tool` already names the tool, and
//! the registry knows which methods that tool advertises. When a bare name
//! matches exactly one of them, dispatching it is not a guess.
//!
//! **What this deliberately does not do.** It never rewrites a method that
//! already carries a namespace, so `evil.get_attachment_text` stays unknown
//! rather than being "repaired" into another tool's method — the redundancy is
//! only resolvable when the planner omitted it, not when it wrote something
//! else. And an ambiguous bare name is left alone: the worker's own
//! `METHOD_NOT_FOUND` is the honest answer when the registry cannot tell which
//! method was meant. This is a namespace-completion rule, not a spell-checker.

/// The method to dispatch in place of `requested`, or `None` to send it
/// unchanged.
///
/// Returns `Some` only when `requested` carries no namespace **and** exactly
/// one entry of `advertised` ends with `.{requested}`.
pub(super) fn qualified_method(requested: &str, advertised: &[&str]) -> Option<String> {
    // An advertised name is already correct; never touch it. Checked first so a
    // (pathological) advertised method with no dot cannot be suffix-matched
    // against a different one.
    if advertised.contains(&requested) {
        return None;
    }
    if requested.contains('.') {
        return None;
    }
    let suffix = format!(".{requested}");
    let mut matches = advertised.iter().filter(|m| m.ends_with(&suffix));
    let first = matches.next()?;
    // A second match means the bare name does not identify one method.
    if matches.next().is_some() {
        return None;
    }
    Some((*first).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIL: &[&str] = &[
        "mail.search",
        "mail.get_message",
        "mail.list_messages",
        "mail.list_accounts",
        "mail.get_attachment_text",
        "mail.get_attachment",
    ];

    /// The live failure: tasks 160 and 162 both wrote this bare name.
    #[test]
    fn a_bare_name_is_qualified_against_the_tools_own_methods() {
        assert_eq!(
            qualified_method("get_attachment_text", MAIL).as_deref(),
            Some("mail.get_attachment_text")
        );
    }

    #[test]
    fn an_already_correct_method_is_left_alone() {
        assert_eq!(qualified_method("mail.get_attachment_text", MAIL), None);
    }

    /// The security-relevant arm. A method that names a *different* namespace
    /// was not an omission, and completing it into this tool's method would let
    /// a plan reach a method it did not name.
    ///
    /// The third case is the one with teeth: with a multi-level advertised
    /// name, a *dotted* request has a suffix that really can match, so dropping
    /// the `contains('.')` guard changes the answer. The first two cases cannot
    /// expose that — `".evil.get_attachment_text"` matches nothing either way —
    /// and a mutation run proved they do not.
    #[test]
    fn a_method_naming_another_namespace_is_never_rewritten() {
        assert_eq!(qualified_method("evil.get_attachment_text", MAIL), None);
        assert_eq!(qualified_method("web.search", MAIL), None);
        let nested = ["mail.get.text"];
        assert_eq!(
            qualified_method("get.text", &nested),
            None,
            "a dotted request names its own namespace; completing it is a guess"
        );
    }

    #[test]
    fn an_unknown_bare_name_is_left_for_the_worker_to_refuse() {
        assert_eq!(qualified_method("nope", MAIL), None);
    }

    /// `web-search` advertises `web.search` and `web.search_batch`; their
    /// suffixes differ, so both still qualify. The guard is for the case where
    /// they would not.
    #[test]
    fn an_ambiguous_bare_name_is_left_alone() {
        let two = ["a.run", "b.run"];
        assert_eq!(qualified_method("run", &two), None);
        // ...while the unambiguous siblings still resolve.
        let web = ["web.search", "web.search_batch"];
        assert_eq!(qualified_method("search", &web).as_deref(), Some("web.search"));
        assert_eq!(
            qualified_method("search_batch", &web).as_deref(),
            Some("web.search_batch")
        );
    }

    #[test]
    fn a_tool_with_no_advertised_methods_qualifies_nothing() {
        assert_eq!(qualified_method("get_attachment_text", &[]), None);
    }

    /// Suffix matching is on the dotted boundary, not on raw string ends —
    /// otherwise `text` would "match" `mail.get_attachment_text`.
    #[test]
    fn matching_is_on_the_namespace_boundary_not_a_bare_suffix() {
        assert_eq!(qualified_method("text", MAIL), None);
        assert_eq!(qualified_method("message", MAIL), None);
    }

    /// An exactly-advertised method is dispatched as written even when a
    /// *different* advertised method would suffix-match it.
    ///
    /// This is what the leading `advertised.contains` check buys, and nothing
    /// else does: with `["run", "a.run"]` and a request for `run`, dropping it
    /// leaves `.run` matching `a.run` uniquely — so an exactly-correct method
    /// would be rewritten into another one. A single-entry fixture (`["run"]`)
    /// cannot show that, and a mutation run proved it does not.
    #[test]
    fn an_exactly_advertised_method_wins_over_a_suffix_match() {
        let both = ["run", "a.run"];
        assert_eq!(
            qualified_method("run", &both),
            None,
            "exact wins; must not be rewritten to a.run"
        );
        let odd = ["run"];
        assert_eq!(qualified_method("run", &odd), None, "already exact — unchanged");
        assert_eq!(qualified_method("x", &odd), None);
    }
}
