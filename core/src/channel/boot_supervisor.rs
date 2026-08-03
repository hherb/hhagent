//! Supervised channel bring-up (#514). Filled in by Task 3; for now it only
//! carries the pure escalation policy.

pub mod downtime;

pub use downtime::DowntimeEscalator;
