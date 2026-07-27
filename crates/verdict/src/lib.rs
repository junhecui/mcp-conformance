//! `(canonical_evidence, protocol_version) -> verdict`. Pure.
//!
//! **Must not:** call a model, a network, or a clock.
//!
//! `no_std`, per ADR-007. Taking a clock as a dependency here is not a mistake CI catches
//! after the fact; it is code that does not link. Anything that later wants `std` in the
//! verdict engine is a signal that it belongs *outside* the verdict engine — which is the
//! intended effect, and will feel like an obstruction at least once.

#![no_std]

extern crate alloc;

use datamodel::{CanonicalChangeset, Oracle, Outcome, ReasonCode};

/// The result of applying one verification protocol.
///
/// `reason` is mandatory whenever `outcome` is [`Outcome::Unverifiable`] — architecture.md
/// §6, invariant 3. Enforced structurally, not by discipline: fields are private, and the
/// only three ways to build one are [`Self::holds`], [`Self::violated`], and
/// [`Self::unverifiable`] — there is no constructor that accepts `Outcome::Unverifiable`
/// without also requiring a [`ReasonCode`], so "a shrug" is not a value this type can hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    outcome: Outcome,
    reason: Option<ReasonCode>,
    oracle: Oracle,
}

impl Assessment {
    /// The annotation holds — the observation is consistent with the declared value.
    #[must_use]
    pub const fn holds(oracle: Oracle) -> Self {
        Self { outcome: Outcome::Holds, reason: None, oracle }
    }

    /// The observation contradicts the declared value.
    #[must_use]
    pub const fn violated(oracle: Oracle) -> Self {
        Self { outcome: Outcome::Violated, reason: None, oracle }
    }

    /// The protocol could not decide. Always carries a reason — there is no other way to
    /// reach `Outcome::Unverifiable` through this type.
    #[must_use]
    pub fn unverifiable(oracle: Oracle, reason: ReasonCode) -> Self {
        Self { outcome: Outcome::Unverifiable, reason: Some(reason), oracle }
    }

    /// Whether the annotation held, was violated, or could not be decided.
    #[must_use]
    pub const fn outcome(&self) -> Outcome {
        self.outcome
    }

    /// Why, when [`Self::outcome`] is [`Outcome::Unverifiable`]. Always `None` otherwise —
    /// see this type's own doc comment for why that pairing can't come apart.
    #[must_use]
    pub const fn reason(&self) -> Option<&ReasonCode> {
        self.reason.as_ref()
    }

    /// Which observation surface produced this assessment.
    #[must_use]
    pub const fn oracle(&self) -> Oracle {
        self.oracle
    }
}

/// Decide `readOnlyHint` from a single-invocation changeset (architecture.md §4.3's single
/// arm: `canonical(D1)` non-empty, over `user_state` only, contradicts a `true`
/// declaration).
///
/// Always [`Oracle::KernelChangeset`] — this is the Class A engine, over an overlayfs
/// changeset; Track B's `protocol_probe` oracle (B-01) is a separate, already-shipped
/// engine with its own decision function, not this one.
///
/// Never returns `Unverifiable` itself: by the time evidence reaches this function it has
/// already cleared P1-05's integrity gate, and this single-arm protocol has no decision
/// branch of its own that produces anything but a definite `Holds`/`Violated` — unlike
/// `idempotentHint`'s multi-arm protocol (P2-09), which does decide `unverifiable` internally
/// (the caching-suppressed-in-process branch). A caller composing a full run's verdict is
/// responsible for using the gate's own `Unverifiable` outcome (with the gate's reason)
/// instead of calling this function at all when the gate didn't pass — that composition is
/// P1-08's job, not this pure function's.
///
/// A `false` declaration is never contradicted here: declaring non-read-only promises
/// nothing about what a given invocation *will* do, so no changeset — empty or not —
/// contradicts it.
#[must_use]
pub fn read_only_hint(declared: bool, d1: &CanonicalChangeset) -> Assessment {
    if declared && !d1.user_state_is_empty() {
        return Assessment::violated(Oracle::KernelChangeset);
    }
    Assessment::holds(Oracle::KernelChangeset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use datamodel::{ClassifiedPath, PathTaxonomy};

    fn changeset(entries: alloc::vec::Vec<(&str, PathTaxonomy)>) -> CanonicalChangeset {
        CanonicalChangeset::new(
            entries
                .into_iter()
                .map(|(path, taxonomy)| ClassifiedPath { path: path.as_bytes().to_vec(), taxonomy })
                .collect(),
        )
    }

    #[test]
    fn true_declaration_with_empty_user_state_holds() {
        let d1 = changeset(vec![("tmp/scratch", PathTaxonomy::Ephemeral)]);
        let assessment = read_only_hint(true, &d1);
        assert_eq!(assessment, Assessment::holds(Oracle::KernelChangeset));
    }

    /// architecture.md §4.3's literal exit criterion.
    #[test]
    fn true_declaration_with_nonempty_user_state_is_violated() {
        let d1 = changeset(vec![
            ("tmp/scratch", PathTaxonomy::Ephemeral),
            ("home/user/output.txt", PathTaxonomy::UserState),
        ]);
        let assessment = read_only_hint(true, &d1);
        assert_eq!(assessment, Assessment::violated(Oracle::KernelChangeset));
    }

    #[test]
    fn false_declaration_holds_regardless_of_the_changeset() {
        let empty = changeset(vec![]);
        assert_eq!(read_only_hint(false, &empty), Assessment::holds(Oracle::KernelChangeset));

        let nonempty = changeset(vec![("home/user/output.txt", PathTaxonomy::UserState)]);
        assert_eq!(read_only_hint(false, &nonempty), Assessment::holds(Oracle::KernelChangeset));
    }

    #[test]
    fn server_internal_alone_does_not_violate_a_true_declaration() {
        let d1 = changeset(vec![("home/user/.cache/tool/x", PathTaxonomy::ServerInternal)]);
        assert_eq!(read_only_hint(true, &d1), Assessment::holds(Oracle::KernelChangeset));
    }

    #[test]
    fn assessment_accessors_expose_what_was_constructed() {
        let a = Assessment::holds(Oracle::KernelChangeset);
        assert_eq!(a.outcome(), Outcome::Holds);
        assert_eq!(a.reason(), None);
        assert_eq!(a.oracle(), Oracle::KernelChangeset);

        let reason = ReasonCode(alloc::string::String::from("timeout"));
        let u = Assessment::unverifiable(Oracle::KernelChangeset, reason.clone());
        assert_eq!(u.outcome(), Outcome::Unverifiable);
        assert_eq!(u.reason(), Some(&reason));
    }
}
