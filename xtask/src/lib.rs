//! Workspace tooling, exposed as a library so the checks are unit-testable.

pub mod aggregate_report;
pub mod census;
pub mod census_stage1;
pub mod class_a_stage2;
#[cfg(target_os = "linux")]
pub mod derive_verdicts;
pub mod dump_tools;
#[cfg(target_os = "linux")]
pub mod first_verdict;
#[cfg(target_os = "linux")]
pub mod fixture_generality;
pub mod methodology;
pub mod pin_stability;
pub mod probe_stage1;
pub mod purity;
#[cfg(target_os = "linux")]
pub mod ruleset_v2;
