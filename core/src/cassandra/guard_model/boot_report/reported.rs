//! The probe rates under the names they are **reported** by (issue [#643]).
//!
//! # The gap this closes
//!
//! The same four numbers reach an operator through three channels — the
//! `info!` boot line, the `warn!` coverage finding, and the durable
//! `guard_tier.boot` row — and each channel renamed them itself:
//!
//! ```text
//! tok_per_s         = rates.fastest_tok_per_s,   // main.rs, twice
//! slowest_tok_per_s = rates.slowest_tok_per_s,
//! ```
//!
//! Two adjacent `Option<f32>` fields, mapped by hand in three places.
//! The *payload* copy was guarded; the two `tracing` copies were not,
//! and `tracing` fields cannot be read back after the fact without a
//! subscriber, so no test could have caught a swap in them.
//!
//! Transposing that pair does not produce a visibly wrong row. Since
//! `slowest <= fastest` always holds, a swapped pair reports a
//! **contended** boot as a quiet one — which is exactly the diagnostic
//! [#624] was filed to make visible, silenced by the line meant to
//! carry it.
//!
//! # Why a struct rather than a subscriber test
//!
//! [#643] offered two shapes: capture the events with a
//! `tracing_subscriber` layer, or move the mapping somewhere a unit test
//! can reach. This is the second, and it is the stronger of the two —
//! a subscriber test would *detect* a divergence between the three
//! sites, whereas moving the mapping here leaves no second site to
//! diverge.
//!
//! `tracing`'s field syntax cannot spread a struct, so each macro still
//! names its four fields. What changed is what sits on the right-hand
//! side: `tok_per_s = reported.tok_per_s` is name-for-name identity, so
//! a transposition is visible on the line itself — the self-evidence
//! the code had before [#632] renamed the struct field, recovered
//! without giving the rename back.
//!
//! # The reporting vocabulary is frozen
//!
//! These names are **not** [`BootRates`]'s. `BootRates::fastest_tok_per_s`
//! says which sample it is; the reported name is `tok_per_s`, because
//! the durable wire key cannot move — live rows carry it and the
//! operator query `slowest_tok_per_s < tok_per_s / 2` is written against
//! it. This type is where those two vocabularies meet, once.
//!
//! [#624]: https://github.com/hherb/kastellan/issues/624
//! [#632]: https://github.com/hherb/kastellan/issues/632
//! [#643]: https://github.com/hherb/kastellan/issues/643

use super::BootRates;
use crate::cassandra::guard_model::timeout::TimeoutBasis;

/// The four probe numbers named as an operator sees them.
///
/// Every field is an `Option` for the same reason [`BootRates`]'s are:
/// absence must reach a log line and a row as "not measured", never as
/// a fabricated `0`. This type only renames — it never substitutes a
/// default, and it must not start.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct ReportedRates {
    /// [`BootRates::fastest_tok_per_s`] under its reporting name.
    ///
    /// The rename is the whole point of the type: the *fastest* sample
    /// is the one that measures the host, but the key an operator reads
    /// and queries is `tok_per_s`.
    pub tok_per_s: Option<f32>,
    /// [`BootRates::slowest_tok_per_s`], unchanged.
    pub slowest_tok_per_s: Option<f32>,
    /// [`BootRates::measured_samples`], unchanged.
    pub measured_samples: Option<u32>,
    /// [`BootRates::attempted_samples`], unchanged.
    pub attempted_samples: Option<u32>,
}

impl ReportedRates {
    /// Rename one [`BootRates`] into the reporting vocabulary.
    ///
    /// The **only** place the fastest sample becomes `tok_per_s`. Three
    /// consumers now share it, so a transposition here fails a unit test
    /// instead of quietly reporting a busy boot as a quiet one.
    pub fn from_rates(rates: &BootRates) -> Self {
        Self {
            tok_per_s: rates.fastest_tok_per_s,
            slowest_tok_per_s: rates.slowest_tok_per_s,
            measured_samples: rates.measured_samples,
            attempted_samples: rates.attempted_samples,
        }
    }

    /// The whole read, basis to reportable names, in one call.
    ///
    /// Convenience for the three call sites, all of which start from a
    /// basis. Deliberately a thin composition of two tested functions
    /// rather than a second reader of `TimeoutBasis`: a separate
    /// traversal here could disagree with [`BootRates::from_basis`],
    /// which is the class of defect this module exists to remove.
    pub fn from_basis(basis: &TimeoutBasis) -> Self {
        Self::from_rates(&BootRates::from_basis(basis))
    }
}

#[cfg(test)]
mod tests;
