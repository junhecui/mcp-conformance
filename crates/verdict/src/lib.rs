//! `(canonical_evidence, protocol_version) -> verdict`. Pure.
//!
//! **Must not:** call a model, a network, or a clock.
//!
//! `no_std`, per ADR-007. Taking a clock as a dependency here is not a mistake CI catches
//! after the fact; it is code that does not link. Anything that later wants `std` in the
//! verdict engine is a signal that it belongs *outside* the verdict engine — which is the
//! intended effect, and will feel like an obstruction at least once.

#![no_std]

use datamodel::{CanonicalChangeset, Outcome, ReasonCode};

/// The result of applying one verification protocol.
///
/// `reason` is mandatory whenever `outcome` is [`Outcome::Unverifiable`] — architecture.md
/// §6, invariant 3. The database enforces the same rule as a constraint (F-06); this type
/// exists so the constraint is not the only thing standing between us and a shrug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    /// Whether the annotation held, was violated, or could not be decided.
    pub outcome: Outcome,
    /// Why, when the outcome is `Unverifiable`.
    pub reason: Option<ReasonCode>,
}

/// Decide `readOnlyHint` from a single-invocation changeset.
///
/// A non-empty canonical changeset over `user_state` contradicts a `true` declaration.
/// The `user_state` / `server_internal` / `ephemeral` split is ADR-008, which is why this
/// cannot be written yet.
pub fn read_only_hint(_declared: bool, _d1: &CanonicalChangeset) -> Assessment {
    todo!("P1-07 — blocked on ADR-008 (path taxonomy)")
}
