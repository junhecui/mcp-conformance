//! `(raw_evidence, ruleset) -> canonical_changeset`. Pure and deterministic.
//!
//! **Must not:** read anything outside its inputs — no clock, no filesystem, no network.
//!
//! `no_std` is load-bearing, not stylistic. It is what makes ADR-005 a property of the
//! build rather than a comment: there is no `std::fs` to call because `std` is not linked.
//! F-04's dependency-graph assertion is the outer layer; this is the inner one.
//!
//! P1-06 will test whether that survives contact with real path matching. `regex` supports
//! `no_std` + `alloc`; `globset` likely does not. If matching forces `std`, relaxing this
//! is a deliberate, documented act — and the `clippy.toml` layer described in ADR-007
//! becomes load-bearing at that moment.

#![no_std]

use datamodel::{CanonicalChangeset, RawEvidence, Ruleset};

/// Apply a ruleset to raw evidence, yielding the canonical changeset that every
/// verification protocol is evaluated against.
///
/// The ruleset arrives already parsed. See [`datamodel::Ruleset`] for why.
pub fn normalise(_evidence: &RawEvidence, _ruleset: &Ruleset) -> CanonicalChangeset {
    todo!("P1-06 — blocked on ADR-008 (path taxonomy) and ADR-009 (evidence tree format)")
}
