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
                '"' => in_quotes = false,
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
                ')' => comment_depth -= 1,
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
/// CFWS is legal around the `=`, so `dmarc =fail`, `dmarc= fail`, and
/// `dmarc = fail` are all legal input a compliant MX may emit and must all
/// read as `("dmarc", "fail")`, not silently fail to parse (a segment this
/// function fails to recognise is invisible to [`dmarc_verdict`]'s dmarc
/// count, which is exactly how a legally-spaced real verdict could once be
/// skipped in favour of a forged, unspaced one below it). To support that
/// without reopening the property-value-smuggling hole this guards against,
/// method and result are read positionally — method is the run of
/// non-whitespace, non-`=` characters at the very START of what remains
/// after stripping, i.e. still the FIRST token position, never found by
/// scanning further into the segment — rather than by taking a single
/// whitespace-delimited token and requiring the `=` to be inside it.
pub(super) fn method_result_of(segment: &str) -> Option<(String, String)> {
    let stripped = strip_quotes_and_comments(segment);
    let rest = stripped.trim_start();
    let method_end = rest.find(|c: char| c.is_whitespace() || c == '=')?;
    let method = &rest[..method_end];
    if method.is_empty() {
        return None;
    }
    let rest = rest[method_end..].trim_start().strip_prefix('=')?.trim_start();
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
/// and making the forged segment parse cleanly) from ever being chosen over
/// the real verdict, regardless of which of the two comes first.
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
