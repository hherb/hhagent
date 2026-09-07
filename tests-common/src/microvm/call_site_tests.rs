//! The guard that reads the **real** `core/tests/*.rs` sources and fails on
//! any micro-VM precondition that bypasses `KASTELLAN_MICROVM_REQUIRE_E2E`
//! (#679).
//!
//! # Why this is a source-level test and not a unit test
//!
//! #667 added the knob; #680's review then found the *wiring* of that knob
//! could be replaced with `false` and the suite stayed green, because every
//! test aimed at the pure half. The defect #679 records is one step further
//! out again: the knob works perfectly and the call site simply asks
//! something else first. No unit test of [`super::require`] can see that,
//! and no Firecracker run can either — the false green only appears on a host
//! where the micro-VM preconditions are MET and a neighbouring one is not,
//! which is by definition not the host anybody is gating on.
//!
//! Reading the sources catches it, catches it for call sites nobody has
//! written yet, and runs on **both** hosts — which matters here, because
//! every file it scans is `#![cfg(target_os = "linux")]` and so is invisible
//! to a Mac `cargo test`.
//!
//! ⚠️ The scan is only as good as its file discovery. A glob that matches
//! nothing makes every assertion below vacuous, so the discovery is asserted
//! against a known-minimum roster before anything is checked.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::require::bypassed_gates;
use super::repo_root;

/// Suites that are known to gate on the micro-VM preflight, asserted present
/// before the scan is believed.
///
/// Not the full list — it is a *floor*, so adding a suite does not have to
/// touch this file, while deleting the glob's directory or breaking the
/// pattern still fails loudly. Every entry was a real `#679` call site or a
/// suite the issue's own census named.
const KNOWN_MICROVM_SUITES: [&str; 8] = [
    "browser_driver_firecracker_e2e.rs",
    "python_exec_firecracker_e2e.rs",
    "web_fetch_firecracker_egress_e2e.rs",
    "web_research_firecracker_broker_e2e.rs",
    "web_research_firecracker_egress_e2e.rs",
    "web_research_search_broker_e2e.rs",
    "web_research_vm_force_route_daemon_e2e.rs",
    "web_search_firecracker_egress_e2e.rs",
];

/// Every `core/tests/*.rs` that gates on [`super::REQUIRE_ENV`]'s preflight,
/// i.e. that calls `skip_if_no_microvm`.
///
/// Returns `(path, source)` pairs so a failure can name the file and the
/// caller does not read twice.
fn microvm_suites() -> Vec<(PathBuf, String)> {
    let dir = repo_root().join("core").join("tests");
    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));

    let mut found = Vec::new();
    for entry in entries {
        let path = entry.expect("readable dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        if src.contains("skip_if_no_microvm") {
            found.push((path, src));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// The discovery itself, asserted before it is trusted.
///
/// A `read_dir` that finds nothing, or a rename that empties the roster,
/// would leave [`every_microvm_precondition_routes_through_the_require_knob`]
/// iterating over an empty list and passing — the fail-safe-fixture shape
/// this tree has been bitten by before.
#[test]
fn the_microvm_suite_roster_is_not_empty() {
    let suites = microvm_suites();
    let names: BTreeSet<String> = suites
        .iter()
        .map(|(p, _)| p.file_name().expect("file name").to_string_lossy().into_owned())
        .collect();

    for expected in KNOWN_MICROVM_SUITES {
        assert!(
            names.contains(expected),
            "{expected} is no longer discovered as a micro-VM suite — either it was renamed \
             (update KNOWN_MICROVM_SUITES) or the scan is broken and every assertion built \
             on it is vacuous. Found: {names:?}"
        );
    }
    assert!(
        suites.len() >= KNOWN_MICROVM_SUITES.len(),
        "expected at least {} micro-VM suites, found {}",
        KNOWN_MICROVM_SUITES.len(),
        suites.len()
    );
}

/// **The #679 guard.** No micro-VM suite may ask a precondition that the
/// REQUIRE knob cannot reach.
///
/// When this fails, the fix is at the call site, not here: replace the named
/// helper with its `*_or_reason` sibling routed through
/// [`super::require::skip_unless_ready`] (a `bool` precondition) or
/// [`super::require::dep_or_skip`] (one that yields a value). A hand-written
/// `[SKIP]` that genuinely is an opt-in enablement flag rather than a host
/// precondition takes an inline `// REQUIRE-EXEMPT: <why>` marker instead.
#[test]
fn every_microvm_precondition_routes_through_the_require_knob() {
    let mut violations = Vec::new();
    for (path, src) in microvm_suites() {
        for gate in bypassed_gates(&src) {
            violations.push(format!(
                "{}:{} — {} bypasses {}",
                path.display(),
                gate.line,
                gate.what,
                super::REQUIRE_ENV
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "these micro-VM preconditions report a green run when {} demands a real one \
         (#679):\n  {}\n\nFix at the call site: route the reason through \
         microvm::skip_unless_ready / microvm::dep_or_skip, or mark a genuine opt-in \
         enablement flag with `// REQUIRE-EXEMPT: <why>`.",
        super::REQUIRE_ENV,
        violations.join("\n  ")
    );
}
