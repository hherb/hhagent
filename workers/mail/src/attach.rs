//! Naming an attachment without retyping its sha256.
//!
//! `mail.get_attachment_text` originally took one parameter — the 64-char
//! sha256 from a `mail.get_message` attachment list — and that turned out to be
//! a parameter the planner cannot reliably supply. Live failure, task 160
//! (2026-08-17): the correct hash was the 6th string in the 4 KiB head the
//! planner received, at roughly byte 120, and the model still emitted a
//! *different* 64 hex chars. localmail answered `404 no extracted text for
//! attachment <hash>`, and the agent reported to the user that PDF extraction
//! had failed — while the extracted text (28 594 chars, containing the GST
//! figure that was asked for) sat in the database the whole time.
//!
//! The core-side head the planner reads is built by
//! `extract_scannable_text`: string values only, **keys discarded**. So a hash
//! arrives as an unlabelled 64-char hex blob that the model must first identify
//! by shape and then transcribe exactly. A `filename` beside a `message_id` is
//! shorter, self-describing, and — unlike a hash — a value a reader can *check*
//! against the message it came from.
//!
//! So the tool now accepts either form, and this module owns the pure half:
//! which form was named ([`choose`]), which attachment a filename picks
//! ([`pick`]), and what to tell the planner when the answer is none of them.
//!
//! Every string here is planner-facing. `core` clamps a failed step's detail to
//! [`kastellan_protocol::STEP_ERR_DETAIL_MAX`] chars before the planner sees it
//! (`core::scheduler::inner_loop::summary`), so advice past the cut is dropped
//! as surely as if it were never written — the same budget `ids::explain` is
//! written against, and the same reason its tests import the constant rather
//! than mirroring it.

use crate::ids::LocalmailId;

/// Which attachment the planner named, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// A sha256 copied verbatim by the planner. May well be wrong — see the
    /// module docs — so a 404 on this form gets [`missing_text_advice`]'s
    /// hash-suspicion arm.
    Sha(String),
    /// A message plus (optionally) a filename within it. The sha256 is
    /// resolved from the message itself, so it cannot be mistyped.
    InMessage { message_id: LocalmailId, filename: Option<String> },
}

/// Longest filename this module will quote back. Filenames in the live archive
/// run to ~40 chars (`Download 470989752-e-ticket-DQXK68.pdf` is 38); the cap
/// bounds a hostile or generated name so the advice cannot grow with its input.
const NAME_HEAD: usize = 44;

/// How many filenames a repair message lists. Three names at [`NAME_HEAD`] is
/// the most that fits beside the prose inside the planner's clamp; beyond that
/// the list is elided with `…`, which still tells the planner the parameter
/// exists and that more values are available via `mail.get_message`.
const MAX_LISTED: usize = 3;

/// One attachment, as localmail itself names it.
///
/// The filename comes back beside the hash because `mail.get_attachment` writes
/// the file to disk and needs a name for it — and the *requested* filename is
/// not that name: substring matching means the planner may have asked for
/// `e-ticket-DQXK68.pdf` and been given
/// `Download 470989752-e-ticket-DQXK68.pdf`. Saving under what was typed rather
/// than what was found would put a file on disk under a name the archive does
/// not use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    pub sha256: String,
    /// localmail's own filename for the blob; empty when the entry carried none.
    pub filename: String,
}

impl Picked {
    fn new(sha256: &str, filename: &str) -> Self {
        Self { sha256: sha256.to_string(), filename: filename.to_string() }
    }

    /// The filename to save under, or `None` when the archive had no name for
    /// it (so the caller falls back to what the planner asked for, then to a
    /// sha-derived stem).
    pub fn save_name(&self) -> Option<&str> {
        (!self.filename.is_empty()).then_some(self.filename.as_str())
    }
}

/// Is this a sha256 this worker will interpolate into a URL path?
///
/// The single rule, shared by `handler::validate_sha256` (which turns a `false`
/// into the planner's repair text) and by [`pick`] (which merely skips an
/// entry). Two copies of a traversal guard is one copy too many.
pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Decide which form the planner used.
///
/// `Err` is the planner-facing repair text for a params object that names no
/// attachment, or names two.
///
/// A `sha256` **with** a `filename` is accepted, and the filename ignored: the
/// two cannot contradict each other without fetching the message, the sha
/// already addresses one attachment unambiguously, and `mail.get_attachment`
/// takes a `filename` meaning something else entirely (the output name), so a
/// planner carrying it across from that tool is confused about the parameter,
/// not about which file it wants.
pub fn choose(
    sha256: Option<String>,
    message_id: Option<LocalmailId>,
    filename: Option<String>,
) -> Result<Selector, String> {
    match (sha256, message_id) {
        (Some(_), Some(_)) => Err(
            "Pass EITHER `message_id` (+ optional `filename`) OR `sha256` — not both.".to_string(),
        ),
        (None, Some(message_id)) => Ok(Selector::InMessage { message_id, filename }),
        (Some(sha256), None) => Ok(Selector::Sha(sha256)),
        (None, None) if filename.is_some() => Err(
            "`filename` alone cannot address an attachment — add the `message_id` of the \
             mail.get_message step that listed it."
                .to_string(),
        ),
        (None, None) => Err(
            "Name the attachment: pass the `message_id` of a mail.get_message step plus its \
             `filename`, or the attachment's `sha256`."
                .to_string(),
        ),
    }
}

/// Pick one attachment's sha256 out of a `mail.get_message` `attachments`
/// array, given the filename the planner asked for (or none).
///
/// Matching is a ladder — exact, then case-insensitive, then case-insensitive
/// substring — and the rule is **the first tier with exactly one match wins**.
/// The substring tier is what lets a planner name `e-ticket-DQXK68.pdf` for the
/// archive's `Download 470989752-e-ticket-DQXK68.pdf`, which is the right file
/// by any reading. Ambiguity is refused rather than guessed: serving text from a
/// document the planner did not ask for is the failure this module exists to
/// end, and a wrong answer is worse than a repairable error.
///
/// The tier *order* reads strictest-first, but the result does not depend on it,
/// and saying otherwise would be prose asserting what the code does not do: a
/// stricter tier's match is always also in every looser tier, so whenever a
/// stricter tier holds exactly one name that name is the looser tiers' match
/// too. Reordering the array is a mutation no test can kill — deliberately, and
/// recorded here so the next reader does not go hunting for the test that pins
/// it. What *is* pinned is the behaviour that order was mistakenly credited
/// with: `receipt.pdf` beside `flight-receipt.pdf` resolves rather than being
/// refused, because the exact tier holds one name while the substring tier holds
/// two.
///
/// `message_id` is quoted in the repair text only — the planner has to be able
/// to match the advice to the step it wrote.
pub fn pick(
    attachments: &[serde_json::Value],
    filename: Option<&str>,
    message_id: LocalmailId,
) -> Result<Picked, String> {
    // An entry is usable only if it carries a sha256 of the right shape. A
    // missing one is real (localmail emits `"sha256": null` for a part whose
    // blob was never stored); a malformed one would reach a URL path.
    let usable: Vec<(&str, &str)> = attachments
        .iter()
        .filter_map(|a| {
            let sha = a.get("sha256").and_then(serde_json::Value::as_str)?;
            is_sha256(sha).then(|| {
                let name = a.get("filename").and_then(serde_json::Value::as_str).unwrap_or("");
                (name, sha)
            })
        })
        .collect();

    if usable.is_empty() {
        return Err(format!(
            "message {message_id} has no attachment this tool can read — re-check the \
             mail.get_message output for one with a sha256."
        ));
    }

    let Some(want) = filename else {
        if let [(name, sha)] = usable[..] {
            return Ok(Picked::new(sha, name));
        }
        return Err(with_names(
            &format!(
                "message {message_id} has {} attachments — add `filename`, exactly one of:",
                usable.len()
            ),
            &usable,
        ));
    };

    let lower = want.to_lowercase();
    let exact: Vec<_> = usable.iter().filter(|(n, _)| *n == want).collect();
    let ci: Vec<_> = usable.iter().filter(|(n, _)| n.to_lowercase() == lower).collect();
    let sub: Vec<_> = usable.iter().filter(|(n, _)| n.to_lowercase().contains(&lower)).collect();
    for tier in [&exact, &ci, &sub] {
        if let [(name, sha)] = tier[..] {
            return Ok(Picked::new(sha, name));
        }
    }
    if sub.len() > 1 {
        let candidates: Vec<(&str, &str)> = sub.into_iter().copied().collect();
        return Err(with_names(
            &format!(
                "`filename` matches {} attachments in message {message_id} — copy one exactly:",
                candidates.len()
            ),
            &candidates,
        ));
    }
    Err(with_names(
        &format!("No attachment in message {message_id} has that `filename` — copy one exactly:"),
        &usable,
    ))
}

/// What to tell the planner when localmail has no extracted text for a sha256.
///
/// localmail returns the *same* 404 whether the blob is unknown or merely has
/// no `attachment_text` row yet (`api/attachments.py::get_attachment_text`), so
/// the two cases cannot be told apart from the response. What *can* be told
/// apart is where the hash came from: one this worker resolved out of a message
/// is right by construction, so suggesting the planner mistyped it would send
/// it to repair a parameter that was never wrong — the #536 defect, in a new
/// costume. Hence `planner_supplied`.
///
/// The sha is quoted **last** and as a 12-char prefix — the `ids::explain`
/// convention, so every word of advice sits at a fixed offset from the start
/// and the fit stops being an arithmetic property to re-check.
pub fn missing_text_advice(sha256: &str, planner_supplied: bool) -> String {
    let prefix: String = sha256.chars().take(12).collect();
    if planner_supplied {
        format!(
            "No extracted text for that sha256 — most often a mistyped hash. Retry with \
             `message_id` + `filename` from the mail.get_message step rather than copying a \
             hash. Got: {prefix}"
        )
    } else {
        format!(
            "That attachment has no extracted text (scanned image, or extraction has not run). \
             Use mail.get_attachment to save the original file instead. Got: {prefix}"
        )
    }
}

/// Append as many attachment names to `prose` as the planner's clamp will
/// carry, eliding the rest with `…`.
///
/// The budget is *derived* from [`kastellan_protocol::STEP_ERR_DETAIL_MAX`]
/// rather than arithmetic done once in a comment, so lengthening the prose or
/// raising [`NAME_HEAD`] cannot silently push the list — the actionable half of
/// the message — past the cut. Room for the elision marker is reserved before
/// each name is admitted, which is what keeps the result inside the budget in
/// every branch.
fn with_names(prose: &str, names: &[(&str, &str)]) -> String {
    const ELISION: &str = ", …";
    let budget = kastellan_protocol::STEP_ERR_DETAIL_MAX;
    let mut out = prose.to_string();
    let mut listed = 0usize;
    for (name, _) in names.iter().take(MAX_LISTED) {
        let shown = head(name);
        let sep = if listed == 0 { " " } else { ", " };
        let need = sep.chars().count() + shown.chars().count() + ELISION.chars().count();
        if out.chars().count() + need > budget {
            break;
        }
        out.push_str(sep);
        out.push_str(&shown);
        listed += 1;
    }
    if listed < names.len() {
        out.push_str(if listed == 0 { " …" } else { ELISION });
    }
    out
}

/// Truncate a rendered value so a message cannot grow with its input.
fn head(s: &str) -> String {
    if s.chars().count() <= NAME_HEAD {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(NAME_HEAD).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// What the planner actually receives. These strings are handed to
    /// `RpcError::new` unprefixed, so the whole budget is theirs — unlike
    /// `ids::explain`, which pays for `parse_params`' `"bad params: "`.
    fn as_the_planner_sees_it(s: &str) -> String {
        s.chars().take(kastellan_protocol::STEP_ERR_DETAIL_MAX).collect()
    }

    #[track_caller]
    fn assert_survives_the_clamp(message: &str, phrases: &[&str]) {
        let seen = as_the_planner_sees_it(message);
        for p in phrases {
            assert!(seen.contains(p), "clamped away {p:?}; planner sees: {seen:?}");
        }
    }

    /// A `LocalmailId` for tests. The type deliberately has no public
    /// constructor (see `ids`), so tests go through the wire form it validates.
    fn id(n: i64) -> LocalmailId {
        #[derive(serde::Deserialize)]
        struct P {
            #[serde(deserialize_with = "crate::ids::message_id")]
            message_id: LocalmailId,
        }
        let p: P = serde_json::from_value(json!({ "message_id": n })).unwrap();
        p.message_id
    }

    fn att(filename: &str, sha: &str) -> serde_json::Value {
        json!({ "filename": filename, "sha256": sha, "content_type": "application/pdf", "size": 1 })
    }

    const SHA_A: &str = "71aac4580932cffe7649dda9c4cc10e2997de81d80105eafd448a64763f4a73b";
    const SHA_B: &str = "322baed1a46322785c6cb46395ff9975ea99424cb844afd42ec8b2726604f2cc";
    /// The filename that carried `SHA_A` in the live failure.
    const LIVE_NAME: &str = "Download 470989752-e-ticket-DQXK68.pdf";

    // --- choose: which form was named ---

    #[test]
    fn a_message_id_and_filename_select_the_in_message_form() {
        let got = choose(None, Some(id(37413)), Some(LIVE_NAME.into())).unwrap();
        assert_eq!(
            got,
            Selector::InMessage { message_id: id(37413), filename: Some(LIVE_NAME.into()) }
        );
    }

    #[test]
    fn a_message_id_alone_is_accepted_and_defers_the_choice_to_pick() {
        // The single-attachment case — which is what the live failure was — needs
        // no filename at all.
        let got = choose(None, Some(id(37413)), None).unwrap();
        assert_eq!(got, Selector::InMessage { message_id: id(37413), filename: None });
    }

    #[test]
    fn a_sha256_alone_still_works() {
        // Backward compatibility: the original form is not withdrawn, and the
        // e2e suites still drive it.
        assert_eq!(choose(Some(SHA_A.into()), None, None).unwrap(), Selector::Sha(SHA_A.into()));
    }

    #[test]
    fn a_sha256_beside_a_filename_keeps_the_sha_and_ignores_the_name() {
        // `mail.get_attachment`'s `filename` means the OUTPUT name, so a planner
        // carrying it across is confused about the parameter, not about which
        // file it wants — and the sha already addresses one attachment.
        let got = choose(Some(SHA_A.into()), None, Some("whatever.pdf".into())).unwrap();
        assert_eq!(got, Selector::Sha(SHA_A.into()));
    }

    #[test]
    fn naming_no_attachment_at_all_says_how_to_name_one() {
        let e = choose(None, None, None).unwrap_err();
        assert_survives_the_clamp(&e, &["message_id", "filename", "sha256"]);
    }

    #[test]
    fn naming_both_forms_is_refused_rather_than_silently_preferring_one() {
        // Preferring one would make the *other* argument a lie the planner never
        // learns about: if they disagree, the answer is text from a document it
        // did not ask for, which is worse than an error it can repair.
        let e = choose(Some(SHA_A.into()), Some(id(37413)), None).unwrap_err();
        assert_survives_the_clamp(&e, &["sha256", "message_id"]);
    }

    #[test]
    fn a_filename_without_a_message_id_is_told_which_argument_is_missing() {
        // A filename alone cannot be resolved — attachments are addressed within
        // a message. Naming the *missing* parameter is the whole point (#536).
        let e = choose(None, None, Some(LIVE_NAME.into())).unwrap_err();
        assert_survives_the_clamp(&e, &["message_id"]);
    }

    // --- pick: which attachment a filename names ---

    #[test]
    fn a_lone_attachment_needs_no_filename() {
        let atts = [att(LIVE_NAME, SHA_A)];
        assert_eq!(pick(&atts, None, id(37413)).unwrap().sha256, SHA_A);
    }

    #[test]
    fn an_exact_filename_picks_its_attachment() {
        let atts = [att("other.pdf", SHA_B), att(LIVE_NAME, SHA_A)];
        assert_eq!(pick(&atts, Some(LIVE_NAME), id(37413)).unwrap().sha256, SHA_A);
    }

    #[test]
    fn filename_matching_ignores_case() {
        let atts = [att(LIVE_NAME, SHA_A)];
        assert_eq!(pick(&atts, Some(&LIVE_NAME.to_lowercase()), id(37413)).unwrap().sha256, SHA_A);
    }

    #[test]
    fn a_unique_substring_picks_its_attachment() {
        // The live filename is `Download 470989752-e-ticket-DQXK68.pdf`; a model
        // that writes the meaningful tail rather than the archive's download
        // prefix is naming the right file, and refusing it would spend an
        // iteration on a repair that adds no information.
        let atts = [att("boarding-pass.pdf", SHA_B), att(LIVE_NAME, SHA_A)];
        assert_eq!(pick(&atts, Some("e-ticket-DQXK68.pdf"), id(37413)).unwrap().sha256, SHA_A);
    }

    #[test]
    fn an_exact_match_beats_a_substring_match_of_another_attachment() {
        // `receipt.pdf` is a substring of `flight-receipt.pdf`, so a tier order
        // that ran substring first would have two candidates and refuse a request
        // that names one of them exactly.
        let atts = [att("flight-receipt.pdf", SHA_B), att("receipt.pdf", SHA_A)];
        assert_eq!(pick(&atts, Some("receipt.pdf"), id(37413)).unwrap().sha256, SHA_A);
    }

    /// Two parts of one message really can share a filename (`image001.png` is
    /// the canonical case), and they have different shas. Taking the first would
    /// be a silent wrong answer; the planner is told to disambiguate instead.
    #[test]
    fn two_attachments_sharing_a_filename_are_refused_rather_than_guessed() {
        let atts = [att("image001.png", SHA_A), att("image001.png", SHA_B)];
        let e = pick(&atts, Some("image001.png"), id(37413)).unwrap_err();
        assert_survives_the_clamp(&e, &["image001.png"]);
    }

    #[test]
    fn an_ambiguous_substring_is_refused_and_lists_only_the_candidates() {
        // The third attachment is what gives this test teeth: an implementation
        // that fell through to the generic "copy one exactly" arm would list
        // every attachment in the message, which is both longer and wrong — the
        // planner would be offered a name that does not match what it asked for.
        let atts = [
            att("receipt-jan.pdf", SHA_A),
            att("receipt-feb.pdf", SHA_B),
            att("itinerary.pdf", &"c".repeat(64)),
        ];
        let e = pick(&atts, Some("receipt"), id(37413)).unwrap_err();
        assert_survives_the_clamp(&e, &["receipt-jan.pdf", "receipt-feb.pdf"]);
        assert!(
            !e.contains("itinerary.pdf"),
            "only the attachments that actually match belong in the list: {e}"
        );
    }

    #[test]
    fn several_attachments_and_no_filename_lists_what_to_choose_from() {
        let atts = [att("a.pdf", SHA_A), att("b.pdf", SHA_B)];
        let e = pick(&atts, None, id(37413)).unwrap_err();
        assert_survives_the_clamp(&e, &["filename", "a.pdf", "b.pdf"]);
    }

    #[test]
    fn a_filename_that_matches_nothing_lists_what_is_there() {
        let atts = [att("a.pdf", SHA_A)];
        let e = pick(&atts, Some("nope.pdf"), id(37413)).unwrap_err();
        assert_survives_the_clamp(&e, &["a.pdf"]);
    }

    #[test]
    fn a_message_with_no_attachments_says_so_rather_than_listing_nothing() {
        let e = pick(&[], Some("a.pdf"), id(37413)).unwrap_err();
        assert_survives_the_clamp(&e, &["37413"]);
        assert!(e.contains("no attachment"), "got: {e}");
    }

    #[test]
    fn an_entry_without_a_usable_sha256_is_not_selected() {
        // Defensive: localmail's `_attachment_entry` can emit `"sha256": null`
        // for a part whose blob was never stored. Selecting it would build
        // `/v1/attachments/null/text`; skipping it means the lone *usable*
        // attachment is still found without a filename.
        let atts = [json!({ "filename": "ghost.pdf", "sha256": null }), att("real.pdf", SHA_A)];
        assert_eq!(pick(&atts, None, id(37413)).unwrap().sha256, SHA_A);
    }

    #[test]
    fn an_entry_whose_sha256_is_not_a_hash_is_not_selected() {
        // The sha reaching `pick` is interpolated into a URL path by the
        // caller. Filtering here keeps that guard structural rather than relying
        // on the caller to re-validate what a trusted service sent.
        let atts = [json!({ "filename": "evil.pdf", "sha256": "../../etc/passwd" })];
        let e = pick(&atts, None, id(37413)).unwrap_err();
        assert!(!e.contains("etc/passwd"), "must not echo the rejected path: {e}");
    }

    // --- the repair text for a 404 ---

    #[test]
    fn a_404_on_a_planner_supplied_hash_points_at_the_hash() {
        let m = missing_text_advice(SHA_B, true);
        assert_survives_the_clamp(&m, &["message_id", "filename"]);
    }

    #[test]
    fn a_404_on_a_resolved_hash_does_not_blame_the_hash() {
        // The hash came out of the message, so it is right by construction. The
        // planner must not be sent to re-copy a parameter it never supplied.
        let m = missing_text_advice(SHA_A, false);
        assert!(
            !m.contains("message_id"),
            "a resolved hash is not repaired by re-naming the message: {m}"
        );
        assert_survives_the_clamp(&m, &["mail.get_attachment"]);
    }

    // --- the property that makes every message above fit ---

    #[test]
    fn no_message_grows_past_the_planner_clamp_for_any_input() {
        // The worst case is derived, not hardcoded: raising `NAME_HEAD` or
        // lengthening the prose fails here rather than silently clipping the
        // advice in production. This is the `ids` lesson — its `e.g. 374` defect
        // got in because the probes were shorter than the live values.
        let long = "x".repeat(NAME_HEAD * 4);
        let many: Vec<serde_json::Value> =
            (0..12).map(|i| att(&format!("{long}-{i}.pdf"), SHA_A)).collect();

        let mut messages = vec![
            choose(None, None, None).unwrap_err(),
            choose(Some(SHA_A.into()), Some(id(37413)), None).unwrap_err(),
            choose(None, None, Some(long.clone())).unwrap_err(),
            pick(&many, None, id(37413)).unwrap_err(),
            pick(&many, Some(&long), id(37413)).unwrap_err(),
            pick(&[], None, id(37413)).unwrap_err(),
            missing_text_advice(SHA_A, true),
            missing_text_advice(SHA_A, false),
        ];
        messages.push(pick(&many[..2], Some("x"), id(37413)).unwrap_err());

        for m in &messages {
            assert!(
                m.chars().count() <= kastellan_protocol::STEP_ERR_DETAIL_MAX,
                "{} chars exceeds the planner clamp, so its tail is dropped: {m}",
                m.chars().count()
            );
        }
    }
}
