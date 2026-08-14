//! Resolving an **absent** `data_ceiling` against the task's classification
//! floor, and recording which of the two it was.
//!
//! ## Why this module exists
//!
//! `Plan::data_ceiling` is a **ceiling**, so the most *sensitive* `DataClass`
//! is the most *permissive* one. Filling an omitted field with the constant
//! `Secret` (rank 3, the maximum) therefore made a defaulted plan
//! **unconstrained**: `deterministic`'s I1 (`ceiling >= floor`) cannot fire at
//! rank 3 for any floor, and I3 (`step.classification <= ceiling`) cannot fire
//! because no step class outranks it. That was shipped described as
//! fail-closed and is the opposite ([#506]).
//!
//! `Public` (rank 0) is the tightest ceiling but is **not** the fix: it trips
//! I1 for any task whose floor is above `Public`, which re-blocks the exact
//! terminal plan #505 shipped to unblock — `classification_inference` infers a
//! `Personal` floor from `"my email"`, so a `Public` ceiling would block a
//! correct answer all over again, just at a different stage.
//!
//! The resolution is therefore **the floor itself**: the lowest ceiling that
//! cannot spuriously trip I1, while leaving I3 real teeth for any step the
//! model declared *above* the floor. Serde cannot express this — a field
//! default sees neither the floor nor sibling fields — so the absence has to
//! survive deserialization as `None` and be resolved here, once, by a caller
//! that holds the floor.
//!
//! ## Pure on purpose
//!
//! No logging, no clock, no I/O: the resolution is a total function of
//! `(declared, floor)`, which is what lets the whole truth table below be a
//! unit test rather than a live observation. The caller does the logging and
//! the audit write, because only it knows the task id.
//!
//! [#506]: https://github.com/hherb/kastellan/issues/506

use super::types::DataClass;

/// Where a plan's effective `data_ceiling` came from.
///
/// Recorded in the `plan.formulate` audit row because the serialized plan
/// cannot distinguish the two: once an absent ceiling is resolved, the row
/// shows a concrete `DataClass` that reads exactly like a model decision. The
/// `warn!` on the old constant default reached the daemon log only — never the
/// oversight record — which is half of what [#506] fixes.
///
/// Mirrors the existing `tasks.payload.classification_floor_source`
/// precedent, deliberately: an operator who has learned to read one provenance
/// key should not have to learn a second shape for the other.
///
/// [#506]: https://github.com/hherb/kastellan/issues/506
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataCeilingSource {
    /// The model emitted `data_ceiling` explicitly. Its value is used as-is,
    /// including when it is *lower* than the floor — that is a real I1
    /// violation and the reviewer must see it, not have it silently repaired.
    Declared,
    /// The model omitted the field and it was resolved to the task's
    /// classification floor. A model slip, not a policy decision — which is
    /// precisely why it is worth a distinct token in the audit row.
    FloorResolved,
}

impl DataCeilingSource {
    /// Stable snake_case token for the audit payload.
    ///
    /// Operators grep audit rows for these exact strings, so renaming a branch
    /// is a contract break — the same rule `DataClass::as_pascal_str` carries.
    pub fn as_snake_str(self) -> &'static str {
        match self {
            DataCeilingSource::Declared => "declared",
            DataCeilingSource::FloorResolved => "floor_resolved",
        }
    }
}

/// A plan's effective ceiling plus where it came from.
///
/// One struct rather than a bare `DataClass` so the value and its provenance
/// are produced by a single call and cannot drift apart — deriving them
/// separately is how an audit row ends up disagreeing with the value policy
/// actually enforced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedCeiling {
    /// What I1/I3 and L3 expansion must use.
    pub ceiling: DataClass,
    /// Which world produced `ceiling`.
    pub source: DataCeilingSource,
}

/// Resolve a possibly-absent `data_ceiling` against the task's floor.
///
/// - `Some(c)` ⇒ used verbatim, `Declared`. **Never clamped**, even when `c`
///   is below `floor`: that combination is exactly what I1 exists to catch, so
///   repairing it here would delete the signal the reviewer is meant to act on
///   and turn a model contradicting itself into a silent success.
/// - `None` ⇒ `floor`, `FloorResolved`.
///
/// Total, pure, and independent of the *number* of `DataClass` variants — it
/// never enumerates them, so adding a class cannot leave a stale arm here.
pub fn resolve_data_ceiling(declared: Option<DataClass>, floor: DataClass) -> ResolvedCeiling {
    match declared {
        Some(ceiling) => ResolvedCeiling { ceiling, source: DataCeilingSource::Declared },
        None => ResolvedCeiling { ceiling: floor, source: DataCeilingSource::FloorResolved },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect this module exists for: an omitted ceiling must land on the
    /// floor, NOT on `Secret`.
    ///
    /// Asserted across every floor rather than one, because the old bug was
    /// invisible at the top of the lattice — a constant `Secret` is *correct*
    /// when the floor happens to be `Secret`, so a single-floor test would have
    /// passed against the broken implementation.
    #[test]
    fn an_omitted_ceiling_resolves_to_the_floor_at_every_floor() {
        for floor in [
            DataClass::Public,
            DataClass::Personal,
            DataClass::ClinicalConfidential,
            DataClass::Secret,
        ] {
            let got = resolve_data_ceiling(None, floor);
            assert_eq!(got.ceiling, floor, "omitted ceiling must resolve to the floor");
            assert_eq!(got.source, DataCeilingSource::FloorResolved);
        }
    }

    /// The resolved ceiling must never be the permissive constant the old
    /// default used — stated as its own test because it is the actual
    /// regression, and it only bites below the top of the lattice.
    #[test]
    fn an_omitted_ceiling_is_not_silently_the_most_permissive_class() {
        let got = resolve_data_ceiling(None, DataClass::Public);
        assert_ne!(
            got.ceiling,
            DataClass::Secret,
            "resolving to Secret is the #506 fail-open defect: rank 3 makes I1 and I3 vacuous"
        );
        // And concretely: at a Public floor the ceiling is rank 0, so a step
        // declared above Public now trips I3 where it previously could not.
        assert_eq!(got.ceiling.rank(), 0);
    }

    /// A declared ceiling is authoritative, including the *below-floor* case.
    #[test]
    fn a_declared_ceiling_is_used_verbatim_and_never_clamped() {
        // Below the floor: must survive so I1 can fire on it. Clamping here
        // would repair the model's self-contradiction into a silent pass.
        let got = resolve_data_ceiling(Some(DataClass::Public), DataClass::Secret);
        assert_eq!(got.ceiling, DataClass::Public, "a below-floor ceiling must reach I1 intact");
        assert_eq!(got.source, DataCeilingSource::Declared);

        // At and above the floor: equally verbatim.
        for (declared, floor) in [
            (DataClass::Secret, DataClass::Public),
            (DataClass::Personal, DataClass::Personal),
        ] {
            let got = resolve_data_ceiling(Some(declared), floor);
            assert_eq!(got.ceiling, declared);
            assert_eq!(got.source, DataCeilingSource::Declared);
        }
    }

    /// The two sources must not share a token — the audit row's whole purpose
    /// is telling them apart.
    #[test]
    fn the_two_sources_render_distinct_stable_tokens() {
        assert_eq!(DataCeilingSource::Declared.as_snake_str(), "declared");
        assert_eq!(DataCeilingSource::FloorResolved.as_snake_str(), "floor_resolved");
        assert_ne!(
            DataCeilingSource::Declared.as_snake_str(),
            DataCeilingSource::FloorResolved.as_snake_str()
        );
    }
}
