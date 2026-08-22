//! Offline calibration for the guard-model tier: a labelled corpus and
//! a report.
//!
//! Nothing here runs in the daemon. `kastellan-cli guard calibrate` and
//! `guard capture` are thin shells over these modules — the same
//! lib-plus-thin-CLI split `observation::replay` uses — so the report
//! formatter stays a pure function testable without a model or a
//! network.
//!
//! # Two specs, and `D`-numbers mean different things in each
//!
//! - `docs/superpowers/specs/2026-08-21-shieldstral-guard-slice-1-design.md`
//!   — the adjudicator, the endpoint seam, and this harness's shape.
//! - `docs/superpowers/specs/2026-08-22-guard-measurement-3-corpus-design.md`
//!   — the corpus, the manifest, and τ's selection criterion.
//!
//! **The two `D` namespaces collide and the collisions are plausible**,
//! which is worse than a dangling reference: slice-1's D1 is "the guard
//! endpoint is its own config", corpus-design's D1 is "no third-party
//! text is committed"; slice-1's D7 is "catalogue scores are computed at
//! calibrate time", corpus-design's D7 is τ's criterion. A bare `D7` in
//! [`operating_point`] resolves to a coherent-sounding decision about
//! something else entirely. `manifest` and `operating_point` cite
//! **corpus-design**; `corpus` and `report` cite **slice 1** except
//! where they say otherwise.

pub mod corpus;
pub mod manifest;
pub mod operating_point;
pub mod report;
