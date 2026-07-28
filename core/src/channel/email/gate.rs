//! Pure inbound gate for the email channel. No I/O, no DB, no clock.
//!
//! Two independent checks, neither sufficient alone:
//! * [`trusted_dmarc_pass`] — did OUR MX say DMARC passed? Anyone can write
//!   `Authentication-Results` lines into a message they send, so only the
//!   topmost header bearing our configured authserv-id is evidence.
//! * [`extract_token`] — did the sender include the per-pairing shared secret?
//!   Defence in depth against a misconfigured or compromised MX.

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
        // authserv-id is the first token, up to the first ';'.
        let (id, rest) = match value.split_once(';') {
            Some((id, rest)) => (id, rest),
            None => continue,
        };
        if !id.trim().to_ascii_lowercase().eq(&want) {
            continue; // Not our MX — a forged or upstream header. Ignore it.
        }
        // Topmost match decides, pass or fail. Do NOT keep looking: falling
        // through to a later header is exactly how a forged "dmarc=pass"
        // beneath our MX's "dmarc=fail" would win.
        return has_method_result(rest, "dmarc", "pass");
    }
    false
}

/// Whether `ptypes` contains `method=result` as a whole token, so `dmarc=pass`
/// is not satisfied by `xdmarc=pass`.
fn has_method_result(ptypes: &str, method: &str, result: &str) -> bool {
    ptypes
        .split(|c: char| c == ';' || c.is_whitespace())
        .filter_map(|kv| kv.split_once('='))
        .any(|(k, v)| {
            k.trim().eq_ignore_ascii_case(method)
                // The value may carry a comment, e.g. `pass (policy)`.
                && v.trim()
                    .split(|c: char| c.is_whitespace() || c == '(')
                    .next()
                    .unwrap_or("")
                    .eq_ignore_ascii_case(result)
        })
}

/// Split a body into `(presented_token, body_without_any_token_line)`.
///
/// The FIRST token line supplies the presented token; **every** token line is
/// removed, so the shared secret never reaches a task payload, an LLM prompt,
/// or a quoted reply — including a decoy second line an attacker might add.
pub fn extract_token(body: &str) -> (Option<String>, String) {
    let mut token: Option<String> = None;
    let mut kept: Vec<&str> = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.len() >= TOKEN_PREFIX.len()
            && trimmed[..TOKEN_PREFIX.len()].eq_ignore_ascii_case(TOKEN_PREFIX)
        {
            let value = trimmed[TOKEN_PREFIX.len()..].trim();
            if token.is_none() && !value.is_empty() {
                token = Some(value.to_string());
            }
            continue; // Never keep a token line.
        }
        kept.push(line);
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
        // THE attack: the sender writes their own Authentication-Results line.
        // Only our MX's header counts, and ours says fail.
        let headers = vec![
            h("Authentication-Results", "mx.example.net; dmarc=fail"),
            h("Authentication-Results", "evil.example.com; dmarc=pass"),
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
}
