//! Pure inbound gate for the email channel. No I/O, no DB, no clock.
//!
//! Two independent checks, neither sufficient alone:
//! * [`trusted_dmarc_pass`] — did OUR MX say DMARC passed? Anyone can write
//!   `Authentication-Results` lines into a message they send, so only the
//!   TOPMOST such header decides — see that function's docs for why looking
//!   any further is unsafe even when the topmost header's authserv-id looks
//!   wrong.
//! * [`extract_token`] — did the sender include the per-pairing shared secret?
//!   Defence in depth against a misconfigured or compromised MX.
//!
//! Everything here is deliberately paranoid about the input: an
//! `Authentication-Results` header is attacker-controlled wire data (RFC 8601
//! §5 spells out exactly this risk) that can legally embed RFC 5322 quoted
//! strings and comments — including a literal `;`, `(`, or `)` INSIDE one —
//! so naive delimiter splitting is unsafe. [`top_level_segments`] (in the
//! sibling `authres_parse` module — split out purely to keep this file
//! under the project's LOC guidance) is the ONE scanner both the authserv-id
//! check and the verdict lookup in [`trusted_dmarc_pass`] are built on,
//! specifically so they can never disagree about what counts as a segment
//! boundary: an earlier version used two different, inconsistent splits for
//! the two purposes, and that inconsistency was itself an exploitable bug (a
//! `;` inside a comment attached to the authserv-id broke the id/resinfo
//! split). An email body is attacker-controlled Unicode text of arbitrary
//! shape: parse defensively, never trust a byte offset to line up with a
//! char boundary, never assume ASCII.

use super::authres_parse::{authserv_id_of, dmarc_verdict, top_level_segments};

/// Line prefix carrying the per-pairing token, e.g.
/// `kastellan-token: 9f2a…`. Matched case-insensitively.
pub const TOKEN_PREFIX: &str = "kastellan-token:";

/// Header the MX writes its authentication verdict into (RFC 8601).
const AUTH_RESULTS: &str = "authentication-results";

/// True iff the FIRST `Authentication-Results` header (wire order) has an
/// authserv-id that exactly (case-insensitively) equals `authserv_id` AND
/// its first `dmarc=` verdict is `pass`.
///
/// **Only the first `Authentication-Results` header is ever consulted** —
/// not "the first one whose id matches", the very first one, full stop. If
/// its authserv-id does not match `authserv_id` (a typo in configuration, an
/// intermediate relay's own header arriving before ours, anything at all),
/// this returns `false` immediately; it does NOT keep looking further down
/// for a header that happens to match. Two facts make this safe rather than
/// merely paranoid: (1) the receiving MX ALWAYS prepends its own verdict on
/// receipt, so in a correctly configured deployment the genuine header is
/// always topmost; (2) anyone at all — the sender, a malicious relay — can
/// write an `Authentication-Results` header with any content, including one
/// naming our own authserv-id. Falling through past a topmost mismatch to
/// scan further would let a misconfigured (or simply differently-named) MX
/// verdict be silently replaced by a forgery below it — the operator would
/// never notice the gate had gone from "genuinely checking" to "trusting
/// whatever's furthest down that happens to match". **Operational
/// consequence:** `authserv_id` MUST be configured to exactly the
/// authserv-id string written by whichever mail server is the last hop
/// before this code runs. Get that wrong and every message fails closed —
/// loudly (every message rejected), not silently (some messages admitted on
/// a forged basis).
///
/// Also fails closed: no `Authentication-Results` header at all, an
/// empty/unconfigured `authserv_id`, a malformed header value (unterminated
/// quoted-string or unbalanced comment — see [`top_level_segments`]), no
/// `dmarc=` verdict anywhere in the header, or MORE THAN ONE `dmarc=`
/// verdict in the header (see [`dmarc_verdict`] — never legitimate, always
/// refused rather than guessed at, regardless of which one looks real).
///
/// **Operational note for callers:** `headers` must be supplied in wire
/// order, topmost first — this function has no other way to know which
/// header the receiving MX actually wrote. A multi-milter MX can legitimately
/// emit two `Authentication-Results` headers with the same authserv-id; if
/// the topmost one lacks a `dmarc=` result, every message fails closed under
/// this rule (a deployment/ordering problem, not one this function can
/// safely route around) — see `task-4-report.md` for the full operational
/// note.
pub fn trusted_dmarc_pass(headers: &[(String, String)], authserv_id: &str) -> bool {
    let want = authserv_id.trim().to_ascii_lowercase();
    if want.is_empty() {
        return false; // Unconfigured authserv-id must never admit.
    }
    let value = match headers
        .iter()
        .find(|(name, _)| name.trim().eq_ignore_ascii_case(AUTH_RESULTS))
    {
        Some((_, value)) => value,
        None => return false,
    };
    let segments = match top_level_segments(value) {
        Some(segments) => segments,
        None => return false, // Unterminated quote / unbalanced comment.
    };
    // top_level_segments always yields at least one segment (see its docs),
    // so indexing segment 0 here never panics.
    if authserv_id_of(segments[0]) != want {
        return false;
    }
    dmarc_verdict(&segments[1..]).unwrap_or(false)
}

/// Split a body into `(presented_token, body_with_every_occurrence_removed)`.
///
/// Finds EVERY case-insensitive occurrence of [`TOKEN_PREFIX`] anywhere in
/// the body — not just at the start of a line — and removes from the start
/// of each occurrence through the end of that line. The FIRST occurrence
/// supplies the presented token. This deliberately does not special-case
/// `>`, `|`, `}`, `:`, or any other quote marker, and does not require the
/// prefix to be the first thing on its line: one rule subsumes a quoted
/// reply, a bulleted/braced quote style, and an inline "On Tue, you wrote:
/// ..." quotation, instead of enumerating markers one at a time.
///
/// A leading UTF-8 BOM (U+FEFF) is stripped before scanning: it is not
/// Unicode whitespace, so nothing downstream can be relied on to remove it
/// as a side effect.
///
/// GUARANTEE, precisely stated: every COMPLETE occurrence of the prefix,
/// together with the rest of its line, is removed from the returned body.
/// This is deliberately narrower than "the secret can never appear in the
/// returned body" — that stronger claim cannot be made in general. A token
/// split across two lines by mail transport line-wrapping is not a single
/// occurrence and is not detected. Treat `TOKEN_PREFIX` as sensitive at
/// every layer above this one too; this function is defence in depth, not a
/// proof.
///
/// Implementation note on safety: the scan below indexes `body` at BYTE
/// offsets found by comparing raw bytes against the (pure-ASCII)
/// `TOKEN_PREFIX`. A match can only succeed where every compared byte is
/// itself ASCII (`eq_ignore_ascii_case` never turns a non-ASCII byte into an
/// ASCII one, so a non-ASCII byte can never equal one), and ASCII bytes
/// never occur as UTF-8 continuation bytes — so every offset this function
/// ever slices `body` at is provably a valid char boundary. The byte-scan
/// loop itself never slices `&str` at all — only the boundary-agnostic
/// `&[u8]` — so intermediate positions never risk a panic either.
pub fn extract_token(body: &str) -> (Option<String>, String) {
    let body = body.strip_prefix('\u{FEFF}').unwrap_or(body);
    let prefix_len = TOKEN_PREFIX.len();
    let prefix_bytes = TOKEN_PREFIX.as_bytes();
    let bytes = body.as_bytes();
    let mut token: Option<String> = None;
    let mut removed: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i + prefix_len <= bytes.len() {
        if bytes[i..i + prefix_len].eq_ignore_ascii_case(prefix_bytes) {
            let line_end = body[i..].find('\n').map(|off| i + off).unwrap_or(body.len());
            if token.is_none() {
                let value = body[i + prefix_len..line_end].trim();
                if !value.is_empty() {
                    token = Some(value.to_string());
                }
            }
            removed.push((i, line_end));
            i = line_end;
        } else {
            i += 1;
        }
    }
    let mut kept = String::with_capacity(body.len());
    let mut cursor = 0usize;
    for (start, end) in removed {
        kept.push_str(&body[cursor..start]);
        cursor = end;
    }
    kept.push_str(&body[cursor..]);
    (token, kept.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(name: &str, value: &str) -> (String, String) {
        (name.to_string(), value.to_string())
    }

    #[test]
    fn dmarc_pass_from_our_own_mx_is_accepted() {
        let headers = vec![h("Authentication-Results", "mx.example.net; spf=pass; dkim=pass; dmarc=pass")];
        assert!(trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn dmarc_fail_is_rejected() {
        let headers = vec![h("Authentication-Results", "mx.example.net; dmarc=fail")];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn only_the_first_header_decides_not_a_later_matching_one() {
        // Only the FIRST Authentication-Results header is ever consulted.
        // The second header here has BOTH a matching authserv-id AND a pass
        // verdict — a "first header whose id matches" rule would find it and
        // return true; the actual rule ("only the very first header, full
        // stop") must never reach it, so this must still be false.
        let headers = vec![
            h("Authentication-Results", "evil.example.com; dmarc=pass"),
            h("Authentication-Results", "mx.example.net; dmarc=pass"),
        ];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn only_the_first_header_decides_even_if_a_correct_one_follows() {
        // The F2 residual scenario: a misconfigured (e.g. typo'd)
        // authserv_id must fail CLOSED and loud, not silently fall through
        // to a header below that happens to have the right id and a
        // pass verdict.
        let headers = vec![
            h("Authentication-Results", "typo.mx.example.net; dmarc=fail"),
            h("Authentication-Results", "mx.example.net; dmarc=pass"),
        ];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn only_the_topmost_header_counts_when_both_share_the_configured_id() {
        let headers = vec![
            h("Authentication-Results", "mx.example.net; dmarc=fail"),
            h("Authentication-Results", "mx.example.net; dmarc=pass"),
        ];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn no_matching_authserv_fails_closed() {
        let headers = vec![h("Authentication-Results", "other.mx; dmarc=pass")];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
        assert!(!trusted_dmarc_pass(&[], "mx.example.net"), "no headers at all must fail closed");
    }

    #[test]
    fn authserv_id_match_is_exact_not_prefix() {
        let headers = vec![h("Authentication-Results", "mx.example.net.evil.com; dmarc=pass")];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn header_name_match_is_case_insensitive() {
        let headers = vec![h("authentication-results", "mx.example.net; dmarc=pass")];
        assert!(trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn dmarc_token_must_not_match_a_substring() {
        // "dmarc=pass" must not be satisfied by e.g. "xdmarc=pass".
        let headers = vec![h("Authentication-Results", "mx.example.net; xdmarc=pass; dmarc=fail")];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    // --- F1 (residual): the ';' segmenter must be quote- and comment-aware,
    // and the dmarc lookup must be first-match-wins, not `.any()`. These are
    // the review's exact acceptance-criteria bypass strings. ---

    #[test]
    fn dmarc_fail_is_not_smuggled_via_a_semicolon_inside_a_quoted_property_value() {
        let headers = vec![h(
            "Authentication-Results",
            "mx.example.net; spf=pass smtp.mailfrom=\"a; dmarc=pass b\"@evil.com; dmarc=fail",
        )];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn dmarc_fail_is_not_smuggled_via_a_semicolon_inside_a_quoted_reason() {
        let headers = vec![h(
            "Authentication-Results",
            "mx.example.net; dmarc=fail reason=\"x; dmarc=pass y\"",
        )];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn dmarc_fail_is_not_smuggled_via_a_semicolon_inside_a_comment() {
        let headers = vec![h(
            "Authentication-Results",
            "mx.example.net; dmarc=fail (p=reject; dmarc=pass ok)",
        )];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn a_second_dmarc_segment_fails_closed_even_though_the_first_says_fail() {
        // Fails closed via dmarc_verdict's "> 1 methodspec" rule, not literal
        // first-match-wins — see the test below for the case (first = pass)
        // where only that rule, not luck, saves it.
        let headers = vec![h("Authentication-Results", "mx.example.net; dmarc=fail; dmarc=pass")];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    // --- hardening round 3: two more bypass strings the reviewer found ---

    #[test]
    fn dmarc_pass_is_not_smuggled_via_a_well_formed_second_dmarc_segment() {
        // No malformed quoting needed: an MX echoing an unescaped '"' back
        // into a property value can leave TWO well-formed top-level dmarc
        // segments — forged "pass" first, real "fail" second. Only the
        // more-than-one-dmarc-methodspec rule saves this.
        let headers = vec![h(
            "Authentication-Results",
            "mx.example.net; spf=pass smtp.mailfrom=\"a\"; dmarc=pass; x=\"b\"@evil.com; dmarc=fail",
        )];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn dmarc_pass_is_not_smuggled_when_the_real_verdict_has_space_before_equals() {
        // RFC 8601 permits CFWS around '='; "dmarc =fail" is legal. Before
        // whitespace-tolerant parsing this segment was invisible to the
        // dmarc lookup entirely, so the forged "dmarc=pass" below decided.
        let headers = vec![h("Authentication-Results", "mx.example.net; dmarc =fail; dmarc=pass")];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn dmarc_fail_is_not_smuggled_via_a_semicolon_inside_the_authserv_id_comment() {
        // The regression the F2 comment-stripping fix introduced: the
        // id/resinfo split must ALSO be comment-aware, or this ';' (inside
        // the authserv-id's own comment) is mistaken for the boundary.
        let headers = vec![h(
            "Authentication-Results",
            "mx.example.net (a; dmarc=pass b); dmarc=fail",
        )];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    // --- F2 (fixed earlier): a legal authserv-id form must not be skipped ---

    #[test]
    fn authserv_id_with_trailing_version_number_is_still_matched() {
        let headers = vec![h("Authentication-Results", "mx.example.net 1; dmarc=pass")];
        assert!(trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn authserv_id_with_trailing_comment_is_still_matched() {
        let headers = vec![h("Authentication-Results", "mx.example.net (amavisd-new); dmarc=pass")];
        assert!(trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    // --- additional coverage ---

    #[test]
    fn unconfigured_authserv_id_fails_closed_even_against_a_header_with_no_id() {
        let headers = vec![h("Authentication-Results", "; dmarc=pass")];
        assert!(!trusted_dmarc_pass(&headers, ""));
    }

    #[test]
    fn header_value_without_a_semicolon_is_ignored() {
        let headers = vec![h("Authentication-Results", "mx.example.net dmarc=pass")];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn arc_authentication_results_header_is_not_treated_as_authentication_results() {
        let headers = vec![h("ARC-Authentication-Results", "mx.example.net; dmarc=pass")];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn dmarc_pass_with_a_spaced_trailing_comment_is_still_recognised() {
        let headers = vec![h("Authentication-Results", "mx.example.net; dmarc=pass (policy)")];
        assert!(trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn dmarc_pass_with_a_directly_attached_trailing_comment_is_still_recognised() {
        let headers = vec![h("Authentication-Results", "mx.example.net; dmarc=pass(policy)")];
        assert!(trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn unbalanced_comment_in_the_header_fails_closed() {
        let headers = vec![h("Authentication-Results", "mx.example.net; dmarc=pass (unterminated")];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn unterminated_quote_in_the_header_fails_closed() {
        let headers = vec![h(
            "Authentication-Results",
            "mx.example.net; reason=\"unterminated; dmarc=fail",
        )];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn token_is_extracted_and_stripped_from_the_body() {
        let (tok, body) = extract_token("kastellan-token: abc123\nwhat is 17*23?");
        assert_eq!(tok.as_deref(), Some("abc123"));
        assert_eq!(body, "what is 17*23?");
        assert!(!body.contains("abc123"), "the secret must not survive into the instruction");
    }

    #[test]
    fn token_may_appear_anywhere_in_the_body() {
        let (tok, body) = extract_token("what is 17*23?\n\nkastellan-token: abc123\n");
        assert_eq!(tok.as_deref(), Some("abc123"));
        assert_eq!(body.trim(), "what is 17*23?");
    }

    #[test]
    fn every_token_line_is_stripped_even_when_repeated() {
        let (tok, body) = extract_token("kastellan-token: aaa\nhi\nkastellan-token: bbb");
        assert_eq!(tok.as_deref(), Some("aaa"), "the first token is the presented one");
        assert!(!body.contains("aaa") && !body.contains("bbb"),
                "no token line may survive into the instruction");
    }

    #[test]
    fn absent_token_yields_none_and_an_unchanged_body() {
        let (tok, body) = extract_token("just a question");
        assert_eq!(tok, None);
        assert_eq!(body, "just a question");
    }

    #[test]
    fn token_prefix_match_is_case_insensitive_and_tolerates_spacing() {
        let (tok, _) = extract_token("Kastellan-Token:   abc123  ");
        assert_eq!(tok.as_deref(), Some("abc123"));
    }

    // --- F5 (residual): occurrence-anywhere, not line-anchored / marker list ---

    #[test]
    fn quoted_token_line_is_detected_and_stripped() {
        let (tok, body) = extract_token("> kastellan-token: S3CRET\nwhat is 17*23?");
        assert_eq!(tok.as_deref(), Some("S3CRET"));
        assert!(!body.contains("S3CRET"), "the secret must not survive a quoted reply");
        assert!(body.contains("what is 17*23?"));
    }

    #[test]
    fn doubly_quoted_token_line_is_also_stripped() {
        let (tok, body) = extract_token("> > kastellan-token: S3CRET\nhi");
        assert_eq!(tok.as_deref(), Some("S3CRET"));
        assert!(!body.contains("S3CRET"));
    }

    #[test]
    fn pipe_prefixed_token_line_no_longer_leaks() {
        let (tok, body) = extract_token("| kastellan-token: S3CRET\nhi");
        assert_eq!(tok.as_deref(), Some("S3CRET"));
        assert!(!body.contains("S3CRET"));
    }

    #[test]
    fn brace_prefixed_token_line_no_longer_leaks() {
        let (tok, body) = extract_token("} kastellan-token: S3CRET\nhi");
        assert_eq!(tok.as_deref(), Some("S3CRET"));
        assert!(!body.contains("S3CRET"));
    }

    #[test]
    fn colon_prefixed_token_line_no_longer_leaks() {
        let (tok, body) = extract_token(": kastellan-token: S3CRET\nhi");
        assert_eq!(tok.as_deref(), Some("S3CRET"));
        assert!(!body.contains("S3CRET"));
    }

    #[test]
    fn inline_mid_line_token_no_longer_leaks() {
        let (tok, body) = extract_token("On Tue, you wrote: kastellan-token: S3CRET\nhi");
        assert_eq!(tok.as_deref(), Some("S3CRET"));
        assert!(!body.contains("S3CRET"));
    }

    #[test]
    fn leading_bom_is_stripped_and_does_not_defeat_detection() {
        let (tok, body) = extract_token("\u{FEFF}kastellan-token: S3CRET\nhello");
        assert_eq!(tok.as_deref(), Some("S3CRET"));
        assert!(!body.contains("S3CRET"));
        assert!(!body.contains('\u{FEFF}'), "a leading BOM must not survive into the instruction");
    }

    // --- F6: a multi-byte body must never panic ---

    #[test]
    fn extract_token_does_not_panic_on_a_cjk_only_body() {
        let (tok, body) = extract_token("日日日日日日");
        assert_eq!(tok, None);
        assert_eq!(body, "日日日日日日");
    }

    #[test]
    fn extract_token_does_not_panic_on_an_emoji_body() {
        let body_text = "abc\u{1F600}\u{1F600}\u{1F600}\u{1F600}";
        let (tok, body) = extract_token(body_text);
        assert_eq!(tok, None);
        assert_eq!(body, body_text);
    }

    #[test]
    fn extract_token_survives_a_multibyte_first_line_and_still_finds_the_token() {
        let (tok, body) = extract_token("日日日日日日\nkastellan-token: abc123");
        assert_eq!(tok.as_deref(), Some("abc123"));
        assert_eq!(body, "日日日日日日");
    }
}
