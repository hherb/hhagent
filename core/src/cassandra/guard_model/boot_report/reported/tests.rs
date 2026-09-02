//! Unit tests for [`ReportedRates`] (issue #643).
//!
//! Every fixture uses **four distinct values**. That is not tidiness:
//! the defect being guarded is a transposition between two adjacent
//! `Option<f32>` fields, and any fixture where two of them are equal
//! lets the swap through.

use super::ReportedRates;
use crate::cassandra::guard_model::boot_report::BootRates;

/// A `BootRates` whose four values cannot be confused with one another.
///
/// `slowest < fastest` as production guarantees, so this is also a
/// *legal* set rather than one that only a test could produce.
fn distinct_rates() -> BootRates {
    BootRates {
        fastest_tok_per_s: Some(6073.0),
        slowest_tok_per_s: Some(269.6),
        measured_samples: Some(2),
        attempted_samples: Some(3),
    }
}

/// **The transposition test.**
///
/// The fastest sample must arrive as `tok_per_s` and the slowest as
/// `slowest_tok_per_s`. Swapping the two lines in `from_rates` fails
/// here — and, because all three reporting sites now go through this
/// function, that is the only place the swap can still be made.
///
/// The two numbers are the real ones from #624's DGX boots (6 073 and
/// 269.6, a 22.5x spread), so a reader can see what a swapped pair
/// would claim: that a 22x-contended boot ran at 269.6 tok/s flat.
#[test]
fn the_fastest_rate_is_reported_as_tok_per_s_and_the_slowest_as_slowest() {
    let r = ReportedRates::from_rates(&distinct_rates());
    assert_eq!(r.tok_per_s, Some(6073.0));
    assert_eq!(r.slowest_tok_per_s, Some(269.6));
}

/// The two counts must not swap either.
///
/// `measured_samples` is the numerator and `attempted_samples` the
/// denominator; both are `Option<u32>` and adjacent, so they are the
/// same hazard one type over. A swap here inverts the ratio that says
/// how much evidence is behind a finding — `2/3` measured becomes `3/2`,
/// which is not merely wrong but impossible, and nothing downstream
/// checks that it is.
#[test]
fn the_measured_count_is_the_numerator_and_the_attempted_the_denominator() {
    let r = ReportedRates::from_rates(&distinct_rates());
    assert_eq!(r.measured_samples, Some(2));
    assert_eq!(r.attempted_samples, Some(3));
}

/// Absence stays absent.
///
/// The type renames; it must never substitute. A `0` standing in for
/// "never measured" would be indistinguishable from a wedged backend
/// that genuinely measured `0.0` tok/s — the confusion the `Option`
/// exists to prevent, and the one a well-meaning `unwrap_or(0.0)` in
/// `from_rates` would reintroduce for all three consumers at once.
#[test]
fn a_missing_rate_is_reported_as_missing_not_as_zero() {
    let empty = BootRates {
        fastest_tok_per_s: None,
        slowest_tok_per_s: None,
        measured_samples: None,
        attempted_samples: None,
    };
    let r = ReportedRates::from_rates(&empty);
    assert_eq!(r.tok_per_s, None);
    assert_eq!(r.slowest_tok_per_s, None);
    assert_eq!(r.measured_samples, None);
    assert_eq!(r.attempted_samples, None);
}

/// A half-populated basis must not have its present values shifted into
/// the absent slots.
///
/// The mixed case is the one a pure all-`Some` or all-`None` fixture
/// cannot reach: a `from_rates` that assigned positionally rather than
/// by name would pass both tests above and fail here.
#[test]
fn a_partly_populated_set_keeps_each_value_in_its_own_slot() {
    let partial = BootRates {
        fastest_tok_per_s: Some(1137.0),
        slowest_tok_per_s: None,
        measured_samples: Some(1),
        attempted_samples: Some(3),
    };
    let r = ReportedRates::from_rates(&partial);
    assert_eq!(r.tok_per_s, Some(1137.0));
    assert_eq!(r.slowest_tok_per_s, None);
    assert_eq!(r.measured_samples, Some(1));
    assert_eq!(r.attempted_samples, Some(3));
}

/// `from_basis` is the composition it claims to be, not a second reader.
///
/// If it ever grew its own traversal of `TimeoutBasis`, it could
/// disagree with `BootRates::from_basis` — the exact divergence this
/// module was created to remove. Comparing the two paths over a real
/// basis is what keeps the convenience honest.
#[test]
fn from_basis_agrees_with_reading_the_rates_first() {
    use crate::cassandra::guard_model::timeout::{Clamped, TimeoutBasis};

    let basis = TimeoutBasis::Probed {
        fastest_tok_per_s: 6073.0,
        slowest_tok_per_s: 269.6,
        measured_samples: 2,
        attempted_samples: 3,
        derived_ms: 91_400,
        clamped: Clamped::No,
    };
    assert_eq!(
        ReportedRates::from_basis(&basis),
        ReportedRates::from_rates(&BootRates::from_basis(&basis)),
    );
}
