//! RFC 5322 quote/comment-aware parsing helpers for `Authentication-Results`
//! header values (RFC 8601). Split out of `gate.rs` purely to keep that file
//! under the project's LOC guidance — this is still part of the same pure,
//! no-I/O security boundary, just the low-level tokenizer half of it.
//!
//! An `Authentication-Results` value can legally embed an RFC 5322 §3.2.2
//! quoted-string or a §3.2.4 comment, and either can contain a literal `;`,
//! `(`, or `)` that must NOT be treated as structural. [`top_level_segments`]
//! is the one function that understands this; [`gate::trusted_dmarc_pass`]
//! builds BOTH its authserv-id check and its dmarc-verdict lookup on it, so
//! the two can never disagree about where one segment ends and the next
//! begins — an earlier version used two different splits for the two
//! purposes, and that inconsistency was itself an exploitable bug (a `;`
//! inside a comment attached to the authserv-id broke the id/resinfo split).

/// Splits `value` into top-level segments on `;`, where "top-level" means
/// outside any RFC 5322 §3.2.2 quoted-string and outside any RFC 5322 §3.2.4
/// comment (comments nest; quoted-strings do not). `\` escapes the very next
/// character everywhere — inside a quoted-string OR a comment — so an
/// escaped `"`, `(`, or `)` never toggles state.
///
/// Fails closed: an unterminated quoted-string or an unbalanced `(` makes
/// the whole value unusable (`None`) rather than falling back to a lenient
/// split — a malformed header proves nothing about what a real verdict was.
///
/// Always yields at least one segment when it returns `Some` (even for an
/// empty or `;`-free `value`), so callers may index result `[0]` without a
/// bounds check.
pub(super) fn top_level_segments(value: &str) -> Option<Vec<&str>> {
    let mut segments = Vec::new();
    let mut seg_start = 0usize;
    let mut in_quotes = false;
    let mut comment_depth: usize = 0;
    let mut chars = value.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if in_quotes {
            match c {
                '\\' => {
                    chars.next(); // escape: skip the next char entirely
                }
                '"' => in_quotes = false,
                _ => {}
            }
            continue;
        }
        if comment_depth > 0 {
            match c {
                '\\' => {
                    chars.next();
                }
                '(' => comment_depth += 1,
                ')' => comment_depth -= 1,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_quotes = true,
            '(' => comment_depth += 1,
            ';' => {
                segments.push(&value[seg_start..i]);
                seg_start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    if in_quotes || comment_depth > 0 {
        return None; // Fail closed rather than guess what was meant.
    }
    segments.push(&value[seg_start..]);
    Some(segments)
}

/// Removes every RFC 5322 quoted-string and `(comment)` — delimiters
/// included — from `segment`, using the identical quote/comment rules as
/// [`top_level_segments`]. Because `segment` always comes from that
/// function, any quote/comment opened within it also closes within it (a
/// split only ever happens in a balanced state), so this never itself needs
/// to fail closed.
///
/// Each removed run is replaced by a single SPACE, never by nothing. RFC 5322
/// §3.2.2 makes a comment a form of CFWS — i.e. it *separates* the tokens
/// around it — so deleting one outright can WELD two tokens into a third that
/// appears nowhere in the input. Concretely, `mx(x)example.net` collapsed to
/// `mxexample.net` and so matched a configured authserv-id of
/// `mxexample.net` (found in review). Substituting a space cannot invent a
/// token: it can only ever split one that was already adjacent, so it is the
/// strictly safer direction for a comparison this
/// [`crate::channel::email::gate::trusted_dmarc_pass`] trusts. A
/// quoted-string is substituted identically — it is a single token to
/// whatever consumes it, so it must not fuse with its neighbours either.
fn strip_quotes_and_comments(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    let mut in_quotes = false;
    let mut comment_depth: usize = 0;
    let mut chars = segment.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if in_quotes {
            match c {
                '\\' => {
                    chars.next();
                }
                // Closing delimiter: emit the separator that stands in for the
                // whole removed run (see this fn's docs on token welding).
                '"' => {
                    in_quotes = false;
                    out.push(' ');
                }
                _ => {}
            }
            continue; // Quoted content, and its delimiters, are never kept.
        }
        if comment_depth > 0 {
            match c {
                '\\' => {
                    chars.next();
                }
                '(' => comment_depth += 1,
                ')' => {
                    comment_depth -= 1;
                    if comment_depth == 0 {
                        out.push(' '); // Outermost comment closed — one separator.
                    }
                }
                _ => {}
            }
            continue; // Comment content, and its delimiters, are never kept.
        }
        match c {
            '"' => in_quotes = true,
            '(' => comment_depth += 1,
            _ => out.push(c),
        }
    }
    out
}

/// Extracts the authserv-id from segment 0 of a header value (the text
/// before the first top-level `;`), lower-cased for comparison.
///
/// Per RFC 8601 §2.2 the authserv-id may legally be followed by CFWS (which
/// includes a `(comment)`) and/or a version number before the first `;` —
/// e.g. `mx.example.net (amavisd-new)` or `mx.example.net 1`. Both must
/// still identify as `mx.example.net`: strip comments/quotes first, then
/// take the FIRST whitespace-delimited token. This stays an EXACT match, not
/// a prefix — `mx.example.net.evil.com` must not match `mx.example.net`.
pub(super) fn authserv_id_of(id_segment: &str) -> String {
    strip_quotes_and_comments(id_segment)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Extracts `(method, result)` from a resinfo segment, lower-cased, or
/// `None` if it doesn't contain one.
///
/// Per RFC 8601 §2.2 a resinfo segment is `method [CFWS] "=" [CFWS] result`
/// FIRST, optionally followed by whitespace-separated `ptype.property=value`
/// pairs or a trailing comment — e.g. `dmarc=pass header.from=example.com`.
/// Comments and quoted-strings are stripped BEFORE tokenizing (so a
/// directly-attached comment like `pass(policy)` still reads as `pass`, the
/// same as a spaced one).
///
/// The returned method is the bare **Keyword**, with any RFC 8601
/// `method-version` suffix consumed and discarded:
///
/// ```text
/// methodspec     = [CFWS] method [CFWS] "=" [CFWS] result
/// method         = Keyword [ [CFWS] "/" [CFWS] method-version ]
/// method-version = 1*DIGIT [CFWS]
/// ```
///
/// so `dmarc/1=fail`, `dmarc / 1 = fail`, and `dmarc=fail` all read as
/// `("dmarc", "fail")`. That is load-bearing, not tidiness: a methodspec this
/// function fails to recognise is INVISIBLE to [`dmarc_verdict`]'s dmarc
/// count, so a genuine versioned `dmarc/1=fail` written by the MX would leave
/// a forged, unversioned `dmarc=pass` smuggled into the same header value as
/// the only counted verdict — the more-than-one-dmarc rule never fires and the
/// forgery decides. (Verified as a live bypass in review; the CFWS-around-`=`
/// tolerance below was originally added for the identical reason, and this is
/// the same defect one production of the grammar over.) It also stops an
/// honest `dmarc/1=pass` from failing closed and looking like a delivery bug.
///
/// CFWS is likewise legal around the `=`, so `dmarc =fail`, `dmarc= fail`, and
/// `dmarc = fail` must all parse too. To support all of this without
/// reopening the property-value-smuggling hole this guards against, method and
/// result are read strictly POSITIONALLY — the Keyword is the run of
/// characters at the very START of what remains after stripping, ending at the
/// first whitespace, `=`, or version-introducing `/`, i.e. still the FIRST
/// token position, never found by scanning further into the segment — rather
/// than by taking a single whitespace-delimited token and requiring the `=` to
/// be inside it. Anything that is not a well-formed methodspec at that first
/// position still yields `None`.
pub(super) fn method_result_of(segment: &str) -> Option<(String, String)> {
    let stripped = strip_quotes_and_comments(segment);
    let rest = stripped.trim_start();
    // The Keyword ends at the first whitespace, the '=', or the '/' that
    // introduces a method-version — never later in the segment.
    let method_end = rest.find(|c: char| c.is_whitespace() || c == '=' || c == '/')?;
    let method = &rest[..method_end];
    if method.is_empty() {
        return None;
    }
    let mut rest = rest[method_end..].trim_start();
    // Optional `"/" [CFWS] 1*DIGIT` version. It qualifies the method; it is
    // not part of the Keyword and must not defeat the `== "dmarc"` compare.
    if let Some(after_slash) = rest.strip_prefix('/') {
        let digits = after_slash.trim_start();
        let digits_end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
        if digits_end == 0 {
            return None; // e.g. `dmarc/=pass` — not a legal methodspec.
        }
        rest = digits[digits_end..].trim_start();
    }
    let rest = rest.strip_prefix('=')?.trim_start();
    let result_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let result = &rest[..result_end];
    if result.is_empty() {
        return None;
    }
    Some((method.to_ascii_lowercase(), result.to_ascii_lowercase()))
}

/// Scans resinfo segments (i.e. `segments[1..]` of a [`top_level_segments`]
/// result) for `dmarc=` verdicts.
///
/// * `Some(true)` — exactly one `dmarc` methodspec is present and its result
///   is `pass`.
/// * `Some(false)` — exactly one `dmarc` methodspec is present and its
///   result is anything else.
/// * `None` — zero `dmarc` methodspecs are present, OR more than one is.
///
/// More than one `dmarc` methodspec in a single header is never legitimate:
/// a compliant MX computes and emits a DMARC verdict exactly once. A second
/// one is either a forgery attempt or a parse ambiguity, and in both cases
/// refusing (treating it the same as "no verdict found") is the safe
/// choice — this is what stops a forged `dmarc=pass` segment that is
/// syntactically well-formed on its own (e.g. because an upstream MX echoed
/// an unescaped `"` back into a property value, self-closing a quote early
/// and making the forged segment parse cleanly) from being chosen over a real
/// verdict that is ALSO visible, regardless of which of the two comes first.
///
/// # Residual this rule does NOT close
///
/// It cannot help when the real verdict is not visible *at all*. An MX that
/// echoes attacker-controlled text into its own header value **unescaped**
/// hands the attacker two injection points either side of the verdict it
/// writes (`smtp.mailfrom=…` before it, `header.from=…` after it), and a quote
/// opened in the first and closed in the second swallows the genuine
/// `dmarc=fail` into a stripped quoted-string, leaving a forged `dmarc=pass`
/// after the closing quote as the *only* methodspec this function ever sees —
/// count 1, verdict pass. Raising the count rule to "refuse unless exactly one
/// is present" cannot detect that, because from here the swallowed text is
/// indistinguishable from an MX that simply never ran DMARC.
///
/// This is a property of the MX, not of this parser: RFC 8601 §2.2 requires an
/// embedded `"` inside a quoted-string to be escaped, so a compliant MX never
/// creates the primitive. It is why the DMARC verdict is **not** the gate —
/// `DbPeerAuthorizer` additionally requires the per-pairing token
/// (`gate::extract_token`), which the attacker cannot obtain by manipulating
/// headers at all. Treat `trusted_dmarc_pass` as one of two independent
/// factors, exactly as `channel::email`'s module docs frame it, and never as a
/// standalone authentication decision.
pub(super) fn dmarc_verdict(segments: &[&str]) -> Option<bool> {
    let mut verdict: Option<bool> = None;
    let mut count = 0usize;
    for segment in segments {
        if let Some((method, result)) = method_result_of(segment) {
            if method == "dmarc" {
                count += 1;
                if verdict.is_none() {
                    verdict = Some(result == "pass");
                }
            }
        }
    }
    if count > 1 {
        return None; // Ambiguous: refuse rather than guess which one is real.
    }
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_segments_ignores_a_semicolon_inside_quotes() {
        let segments = top_level_segments("a; b=\"x; y\"; c").unwrap();
        assert_eq!(segments, vec!["a", " b=\"x; y\"", " c"]);
    }

    #[test]
    fn top_level_segments_ignores_a_semicolon_inside_a_comment() {
        let segments = top_level_segments("a; b (x; y); c").unwrap();
        assert_eq!(segments, vec!["a", " b (x; y)", " c"]);
    }

    #[test]
    fn top_level_segments_handles_nested_comments() {
        let segments = top_level_segments("a; b (x (nested; y) z); c").unwrap();
        assert_eq!(segments, vec!["a", " b (x (nested; y) z)", " c"]);
    }

    #[test]
    fn top_level_segments_fails_closed_on_unterminated_quote() {
        assert_eq!(top_level_segments("a; b=\"unterminated"), None);
    }

    #[test]
    fn top_level_segments_fails_closed_on_unbalanced_comment() {
        assert_eq!(top_level_segments("a; b (unterminated"), None);
    }

    #[test]
    fn top_level_segments_respects_a_backslash_escape_inside_quotes() {
        // The escaped '"' does not end the quoted string, so the ';' right
        // after it is still inside quotes and must not split.
        let segments = top_level_segments("a; b=\"x\\\"; y\"; c").unwrap();
        assert_eq!(segments, vec!["a", " b=\"x\\\"; y\"", " c"]);
    }

    #[test]
    fn authserv_id_of_strips_a_nested_comment() {
        assert_eq!(authserv_id_of("mx.example.net (a (b) c)"), "mx.example.net");
    }

    // --- a removed comment/quoted-string separates tokens, never welds them ---

    #[test]
    fn a_comment_inside_the_authserv_id_does_not_weld_the_two_halves() {
        // `mx(x)example.net` must NOT read as the single token
        // `mxexample.net` — deleting the comment outright used to invent an
        // authserv-id that appears nowhere in the header (review finding).
        assert_eq!(authserv_id_of("mx(x)example.net"), "mx");
        assert_ne!(authserv_id_of("mx(x)example.net"), "mxexample.net");
    }

    #[test]
    fn a_quoted_string_inside_the_authserv_id_does_not_weld_either() {
        assert_ne!(authserv_id_of("mx\"x\"example.net"), "mxexample.net");
    }

    #[test]
    fn a_nested_comment_yields_exactly_one_separator_not_one_per_depth() {
        // Only the OUTERMOST close emits the stand-in space, so the token
        // count either side of a nested comment is unchanged.
        let stripped = strip_quotes_and_comments("a(b(c)d)e");
        assert_eq!(stripped.split_whitespace().collect::<Vec<_>>(), vec!["a", "e"]);
    }

    // --- RFC 8601 §2.2 `method = Keyword [ [CFWS] "/" [CFWS] method-version ]` ---

    #[test]
    fn method_result_of_reads_through_a_method_version() {
        assert_eq!(
            method_result_of(" dmarc/1=fail"),
            Some(("dmarc".to_string(), "fail".to_string())),
            "a versioned methodspec must still be recognised as method `dmarc`"
        );
    }

    #[test]
    fn method_result_of_reads_through_a_method_version_with_cfws_around_the_slash() {
        assert_eq!(
            method_result_of(" dmarc / 1 = fail"),
            Some(("dmarc".to_string(), "fail".to_string()))
        );
    }

    #[test]
    fn method_result_of_reads_through_a_multi_digit_method_version() {
        assert_eq!(
            method_result_of(" dmarc/12=pass"),
            Some(("dmarc".to_string(), "pass".to_string()))
        );
    }

    #[test]
    fn method_result_of_reads_through_a_commented_method_version() {
        assert_eq!(
            method_result_of(" dmarc/(why)1=pass"),
            Some(("dmarc".to_string(), "pass".to_string()))
        );
    }

    #[test]
    fn method_result_of_rejects_a_slash_with_no_version_digits() {
        // Not legal RFC 8601, so it must not be silently read as `dmarc`.
        assert_eq!(method_result_of(" dmarc/=pass"), None);
        assert_eq!(method_result_of(" dmarc/x=pass"), None);
    }

    #[test]
    fn method_result_of_still_rejects_a_dotted_prefix_which_is_a_different_method() {
        // The version fix must not turn a propspec-looking token into `dmarc`.
        assert_eq!(
            method_result_of(" policy.dmarc=pass"),
            Some(("policy.dmarc".to_string(), "pass".to_string()))
        );
    }

    #[test]
    fn dmarc_verdict_counts_a_versioned_and_an_unversioned_dmarc_as_two() {
        // The whole point: a genuine `dmarc/1=fail` must be VISIBLE to the
        // count, so a forged unversioned `dmarc=pass` in the same header value
        // can never be the only verdict found.
        assert_eq!(dmarc_verdict(&[" dmarc/1=fail", " dmarc=pass"]), None);
        assert_eq!(dmarc_verdict(&[" dmarc=pass", " dmarc/1=fail"]), None);
    }

    #[test]
    fn dmarc_verdict_honours_a_lone_versioned_verdict() {
        assert_eq!(dmarc_verdict(&[" dmarc/1=pass"]), Some(true));
        assert_eq!(dmarc_verdict(&[" dmarc/1=fail"]), Some(false));
    }

    #[test]
    fn method_result_of_ignores_a_quoted_property_value() {
        assert_eq!(
            method_result_of(" spf=pass smtp.mailfrom=\"weird value\"@evil.com"),
            Some(("spf".to_string(), "pass".to_string()))
        );
    }

    #[test]
    fn method_result_of_none_for_a_segment_with_no_equals_sign() {
        assert_eq!(method_result_of(" not-a-pair"), None);
    }

    #[test]
    fn method_result_of_tolerates_space_before_equals() {
        assert_eq!(
            method_result_of(" dmarc =fail"),
            Some(("dmarc".to_string(), "fail".to_string()))
        );
    }

    #[test]
    fn method_result_of_tolerates_space_after_equals() {
        assert_eq!(
            method_result_of(" dmarc= fail"),
            Some(("dmarc".to_string(), "fail".to_string()))
        );
    }

    #[test]
    fn method_result_of_tolerates_space_around_equals() {
        assert_eq!(
            method_result_of(" dmarc = fail"),
            Some(("dmarc".to_string(), "fail".to_string()))
        );
    }

    #[test]
    fn dmarc_verdict_is_none_when_more_than_one_dmarc_methodspec_is_present() {
        assert_eq!(dmarc_verdict(&[" dmarc=fail", " dmarc=pass"]), None);
    }

    #[test]
    fn dmarc_verdict_is_none_when_no_dmarc_methodspec_is_present() {
        assert_eq!(dmarc_verdict(&[" spf=pass"]), None);
    }

    #[test]
    fn dmarc_verdict_is_some_for_exactly_one_dmarc_methodspec() {
        assert_eq!(dmarc_verdict(&[" spf=pass", " dmarc=pass"]), Some(true));
        assert_eq!(dmarc_verdict(&[" dmarc=fail"]), Some(false));
    }
}
