//! Offline calibration for the guard-model tier: a labelled corpus and
//! a report.
//!
//! Nothing here runs in the daemon. `kastellan-cli guard calibrate` is a
//! thin shell over these two modules — the same lib-plus-thin-CLI split
//! `observation::replay` uses — so the report formatter stays a pure
//! function testable without a model or a network.
//!
//! See `docs/superpowers/specs/2026-08-21-shieldstral-guard-slice-1-design.md`.

pub mod corpus;
pub mod operating_point;
pub mod report;
