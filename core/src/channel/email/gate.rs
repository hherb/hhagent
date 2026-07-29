//! Pure inbound gate for the email channel. No I/O, no DB, no clock.
//!
//! Two independent checks, neither sufficient alone:
//! * [`trusted_dmarc_pass`] — did OUR MX say DMARC passed? Anyone can write
//!   `Authentication-Results` lines into a message they send, so only the
//!   topmost header bearing our configured authserv-id is evidence.
//! * [`extract_token`] — did the sender include the per-pairing shared secret?
//!   Defence in depth against a misconfigured or compromised MX.
//!
//! Everything here is deliberately paranoid about the input: an
//! `Authentication-Results` header is attacker-controlled wire data (RFC 8601
//! §5 spells out exactly this risk), and an email body is attacker-controlled
//! Unicode text of arbitrary shape. Parse defensively; never trust a byte
//! offset to line up with a char boundary; never assume ASCII.

/// Line prefix carrying the per-pairing token, e.g.
/// `kastellan-token: 9f2a…`. Matched case-insensitively.
pub const TOKEN_PREFIX: &str = "kastellan-token:";

/// Header the MX writes its authentication verdict into (RFC 8601).
const AUTH_RESULTS: &str = "authentication-results";

/// True iff the **topmost** `Authentication-Results` header whose authserv-id
/// equals `authserv_id` reports `dmarc=pass`.
///
/// Fails closed: no matching header (or no headers at all) ⇒ `false`. Only the
/// first match is consulted — a sender may prepend a header claiming our
/// authserv-id, but our own MX prepends its header on receipt, so ours is the
/// topmost one. `headers` must be in wire order, topmost first.
pub fn trusted_dmarc_pass(headers: &[(String, String)], authserv_id: &str) -> bool {
    let want = authserv_id.trim().to_ascii_lowercase();
    if want.is_empty() {
        return false; // Unconfigured authserv-id must never admit.
    }
    for (name, value) in headers {
        if !name.trim().eq_ignore_ascii_case(AUTH_RESULTS) {
            continue;
        }
        // Everything up to the first ';' is the authserv-id (optionally
        // followed by a version number / CFWS comment, handled below); the
        // rest is the ';'-separated resinfo list.
        let (id_part, rest) = match value.split_once(';') {
            Some((id_part, rest)) => (id_part, rest),
            None => continue, // No resinfo at all — not a usable verdict.
        };
        if authserv_id_of(id_part) != want {
            continue; // Not our MX — a forged or upstream header. Ignore it.
        }
        // Topmost match decides, pass or fail. Do NOT keep looking: falling
        // through to a later header is exactly how a forged "dmarc=pass"
        // beneath our MX's "dmarc=fail" would win.
        return has_method_result(rest, "dmarc", "pass");
    }
    false
}

/// Extracts the authserv-id from the text preceding the first `;` of an
/// `Authentication-Results` value, lower-cased for comparison.
///
/// Per RFC 8601 §2.2, the authserv-id may legally be followed by CFWS (which
/// includes a `(comment)`) and/or a version number before the first `;` —
/// e.g. `mx.example.net (amavisd-new)` or `mx.example.net 1`. Both must still
/// identify as `mx.example.net`: comparing the whole pre-`;` text verbatim
/// would silently reject a compliant header, and the loop above would then
/// fall through to whatever forged header comes next. So: strip any
/// (possibly nested) parenthesised comments first, then take the FIRST
/// whitespace-delimited token as the id. This stays an EXACT match (not a
/// prefix) — `mx.example.net.evil.com` must still not match `mx.example.net`.
fn authserv_id_of(id_part: &str) -> String {
    let mut without_comments = String::with_capacity(id_part.len());
    let mut depth = 0u32;
    for c in id_part.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => without_comments.push(c),
            _ => {} // Inside a comment — drop it.
        }
    }
    without_comments
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Whether the `;`-separated resinfo list `resinfo` contains a
/// `method=result` pair (e.g. `dmarc=pass`), as a whole token.
///
/// Per RFC 8601 §2.2 each resinfo segment is `method=result` FIRST,
/// optionally followed by whitespace-separated `ptype.property=value`
/// pairs (e.g. `dmarc=pass header.from=example.com`) or a trailing
/// `(comment)`. Only the FIRST whitespace-delimited token of each segment is
/// ever a method=result pair — anything after it is a property spec or
/// comment and must never be mistaken for one. Splitting the whole segment on
/// whitespace (instead of taking only its first token) would let an attacker
/// smuggle `dmarc=pass` into a *property value*, e.g.
/// `spf=pass smtp.mailfrom="x dmarc=pass y"@evil.com`, and have it read back
/// as a real verdict — this is exactly the bug that must not recur.
fn has_method_result(resinfo: &str, method: &str, result: &str) -> bool {
    resinfo
        .split(';')
        .filter_map(|segment| segment.split_whitespace().next())
        .filter_map(|token| token.split_once('='))
        .any(|(k, v)| {
            k.trim().eq_ignore_ascii_case(method)
                // The result may carry a directly-attached comment with no
                // separating space, e.g. `pass(policy)`.
                && v.trim()
                    .split('(')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case(result)
        })
}

/// Strips leading whitespace and any run of `>` quote markers (with
/// whitespace between them), e.g. `"> > kastellan-token: x"` →
/// `"kastellan-token: x"`.
///
/// A mail client quotes the original message in a reply, so a token line the
/// gate itself sent (or an attacker's decoy) can come back prefixed with `>`.
/// Detection must see through that prefix, or a quoted token line is neither
/// recognised NOR stripped — leaking the secret straight into the LLM
/// instruction, contradicting the whole point of this function.
fn strip_quote_markers(mut s: &str) -> &str {
    loop {
        s = s.trim_start();
        match s.strip_prefix('>') {
            Some(rest) => s = rest,
            None => return s,
        }
    }
}

/// Split a body into `(presented_token, body_without_any_token_line)`.
///
/// The FIRST token line supplies the presented token; **every** token line is
/// removed — quoted or not — so the shared secret never reaches a task
/// payload, an LLM prompt, or a quoted reply, including a decoy second line
/// an attacker might add.
///
/// `body` is untrusted, arbitrary Unicode: an attacker chooses every byte,
/// including where the multi-byte characters fall. `TOKEN_PREFIX` is ASCII,
/// but a line need not be — indexing `line[..TOKEN_PREFIX.len()]` panics
/// whenever that byte offset lands inside a multi-byte character, which an
/// attacker can trivially arrange. `str::get` returns `None` instead of
/// panicking when the range isn't a valid char boundary (or the string is
/// shorter than it), so it is the only safe way to inspect a fixed-width
/// prefix of untrusted text.
pub fn extract_token(body: &str) -> (Option<String>, String) {
    let mut token: Option<String> = None;
    let mut kept: Vec<&str> = Vec::new();
    for line in body.lines() {
        let candidate = strip_quote_markers(line);
        let is_token_line = match candidate.get(..TOKEN_PREFIX.len()) {
            Some(prefix) if prefix.eq_ignore_ascii_case(TOKEN_PREFIX) => {
                let value = candidate[TOKEN_PREFIX.len()..].trim();
                if token.is_none() && !value.is_empty() {
                    token = Some(value.to_string());
                }
                true
            }
            _ => false,
        };
        if !is_token_line {
            kept.push(line); // Not a token line — keep verbatim (incl. any '>').
        }
    }
    (token, kept.join("\n").trim().to_string())
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
    fn forged_header_from_another_authserv_is_ignored() {
        // THE attack: an attacker's own mail server can write ANY
        // Authentication-Results header it likes, including one naming a
        // different authserv-id. Put the attacker's header ON TOP (the worst
        // case — don't rely on ordering assumptions to save us) and ours,
        // correctly reporting failure, below: only a header whose
        // authserv-id truly equals ours may ever decide the verdict.
        let headers = vec![
            h("Authentication-Results", "evil.example.com; dmarc=pass"),
            h("Authentication-Results", "mx.example.net; dmarc=fail"),
        ];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn only_the_topmost_matching_header_counts() {
        // A sender can prepend a header claiming our authserv-id, but our MX
        // prepends ITS header last, so ours is topmost. Index 0 wins.
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

    // --- F1: dmarc=fail must not be smuggled as pass via a property value ---

    #[test]
    fn dmarc_fail_is_not_smuggled_as_pass_via_a_quoted_property_value() {
        // The exploit: splitting the whole resinfo blob on whitespace (not
        // just the first token per ';'-segment) lets a `dmarc=pass` sitting
        // inside an unrelated, attacker-controlled property value (here,
        // inside a quoted smtp.mailfrom local-part) get read back as if it
        // were a real method=result pair — even though our MX's real verdict,
        // later in the same segment, is dmarc=fail.
        let headers = vec![h(
            "Authentication-Results",
            "mx.example.net; spf=pass smtp.mailfrom=\"x dmarc=pass y\"@evil.com; dmarc=fail",
        )];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    // --- F2: a legal authserv-id form must not be skipped ---

    #[test]
    fn authserv_id_with_trailing_version_number_is_still_matched_and_wins_topmost() {
        // RFC 8601 allows an optional version number after the authserv-id.
        let headers = vec![
            h("Authentication-Results", "mx.example.net 1; dmarc=fail"),
            h("Authentication-Results", "mx.example.net; dmarc=pass"),
        ];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    #[test]
    fn authserv_id_with_trailing_comment_is_still_matched_and_wins_topmost() {
        // RFC 8601 allows a CFWS comment after the authserv-id.
        let headers = vec![
            h("Authentication-Results", "mx.example.net (amavisd-new); dmarc=fail"),
            h("Authentication-Results", "mx.example.net; dmarc=pass"),
        ];
        assert!(!trusted_dmarc_pass(&headers, "mx.example.net"));
    }

    // --- F7: additional properties with no prior coverage ---

    #[test]
    fn unconfigured_authserv_id_fails_closed_even_against_a_header_with_no_id() {
        // If the empty-authserv_id guard were removed, a header with an
        // empty/malformed authserv-id (nothing before the ';') would
        // spuriously "match" an unconfigured gate and its dmarc=pass would be
        // trusted. A plain "authserv_id is empty" check with a normal header
        // wouldn't catch this — the header below is what makes it load-bearing.
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
        // ARC-Authentication-Results is a DIFFERENT header (RFC 8617) that
        // relays use to record what THEY saw; it is not our own MX's verdict
        // and any sender/relay can write one.
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
        // No space before the comment — legal, and only caught by stripping
        // a trailing "(...)" from the result value itself.
        let headers = vec![h("Authentication-Results", "mx.example.net; dmarc=pass(policy)")];
        assert!(trusted_dmarc_pass(&headers, "mx.example.net"));
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

    // --- F5: a quoted reply must not resurrect the secret ---

    #[test]
    fn quoted_token_line_is_detected_and_stripped() {
        let (tok, body) = extract_token("> kastellan-token: S3CRET\nwhat is 17*23?");
        assert_eq!(tok.as_deref(), Some("S3CRET"));
        assert_eq!(body.trim(), "what is 17*23?");
        assert!(!body.contains("S3CRET"), "the secret must not survive a quoted reply");
    }

    #[test]
    fn doubly_quoted_token_line_is_also_stripped() {
        let (tok, body) = extract_token("> > kastellan-token: S3CRET\nhi");
        assert_eq!(tok.as_deref(), Some("S3CRET"));
        assert!(!body.contains("S3CRET"));
    }

    // --- F6: a multi-byte body must never panic ---

    #[test]
    fn extract_token_does_not_panic_on_a_cjk_only_body() {
        // 6 x U+65E5 = 18 bytes; byte offset TOKEN_PREFIX.len() (16) lands
        // mid-character, not on a char boundary — the exact shape that made
        // the old `line[..TOKEN_PREFIX.len()]` slice panic.
        let (tok, body) = extract_token("日日日日日日");
        assert_eq!(tok, None);
        assert_eq!(body, "日日日日日日");
    }

    #[test]
    fn extract_token_does_not_panic_on_an_emoji_body() {
        // "abc" (3 bytes) + 4 x 4-byte emoji = 19 bytes; offset 16 again
        // lands inside the 4th emoji's UTF-8 encoding, not on a boundary.
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
