//! Aggregate per-annotation coverage: `explicit` / `defaulted` / `absent`, per tool, per server, corpus-wide.
//!
//! **Must not:** Touch behavioural evidence.
//!
//! Contract: [architecture.md §3.1].

pub mod coverage;
