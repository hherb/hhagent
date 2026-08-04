//! Bounding text that is about to become a durable `audit_log` payload value.
//!
//! Audit payloads are capped as a whole by
//! [`kastellan_db::audit::truncate_payload`], but that cap replaces the
//! *entire* payload with a hash — the right backstop, and the wrong outcome
//! for a row whose other fields (which channel, which attempt) are the useful
//! part. Bounding the one unbounded field first keeps the row readable.
//!
//! Defence in depth, not belt-and-braces: the values that reach here originate
//! outside the core (an upstream HTTP error body, a transport error string),
//! and a sink must not trust the producer to keep bounding them.

/// Marker appended to a value this module shortened, so a reader can tell
/// truncation from a genuinely terse message.
pub const TRUNCATION_MARKER: &str = "...(truncated)";

/// Return `text` unchanged when it is at most `cap` **chars**, else its first
/// `cap` chars followed by [`TRUNCATION_MARKER`].
///
/// Counts and cuts by `char`, never by byte, so a multi-byte codepoint
/// straddling the cap can neither panic nor produce invalid UTF-8.
///
/// Pure: no I/O, no global state. Same input → same output, every call.
pub fn cap_chars(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let mut capped: String = text.chars().take(cap).collect();
    capped.push_str(TRUNCATION_MARKER);
    capped
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cap the email channel's skipped-id sink has always used; kept here
    /// so these tests read as the same cases they were before the lift.
    const CAP: usize = 256;

    #[test]
    fn short_input_passes_through_unchanged() {
        assert_eq!(cap_chars("no usable From address", CAP), "no usable From address");
    }

    #[test]
    fn input_at_exactly_the_cap_passes_through_unchanged() {
        let at_cap = "a".repeat(CAP);
        assert_eq!(cap_chars(&at_cap, CAP), at_cap);
    }

    /// Simulates `describe_email_error`'s `localmail {status}: {body}` shape
    /// with a body well past its own 200-char worker-side cap — a sink must
    /// not rely on that cap holding.
    #[test]
    fn oversized_input_is_truncated_and_marked() {
        let huge = format!("localmail 500: {}", "x".repeat(5_000));
        let capped = cap_chars(&huge, CAP);

        assert!(capped.chars().count() <= CAP + TRUNCATION_MARKER.len());
        assert!(capped.ends_with(TRUNCATION_MARKER), "{capped}");
        assert!(huge.len() > capped.len(), "must actually shrink an oversized value");
    }

    /// Multi-byte chars (a non-ASCII upstream error message) straddling the
    /// cap must not panic or produce an invalid `String`.
    #[test]
    fn truncation_lands_on_a_char_boundary_not_mid_utf8_codepoint() {
        let multibyte = "€".repeat(CAP + 10);
        let capped = cap_chars(&multibyte, CAP); // would panic on a mid-codepoint byte slice

        assert!(capped.starts_with('€'));
    }

    /// The degenerate cap keeps only the marker — no panic, no empty string
    /// that would read as "there was no cause".
    #[test]
    fn a_zero_cap_keeps_only_the_marker() {
        assert_eq!(cap_chars("anything", 0), TRUNCATION_MARKER);
    }
}
