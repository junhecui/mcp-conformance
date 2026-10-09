//! What one run of one tool actually produced, as the verdict engine is allowed to see it.
//!
//! Three things travel together here, and the shape of this module is the reason they
//! cannot be separated:
//!
//! - the **gate attestation** ([`datamodel::GateAttestation`]), without which there is no
//!   [`GatedRun`] to assess at all (ADR-004: the gate is a hard precondition, not a check
//!   in a call chain);
//! - the **invocation result** ([`InvocationResult`]) — did the `tools/call` actually
//!   complete, or did it fail at the tool level, or produce nothing;
//! - the **derivation result** — a canonical changeset, or a typed
//!   [`DerivationFailure`].
//!
//! The false-`holds` gap commit `832d990` closed in Track B was exactly a dropped second
//! item: a changeset was in hand, the invocation had failed, and nothing in the types made
//! the caller say so. [`Observation::new`] is the only constructor, and it takes all of
//! them, so there is no way to hand the engine a changeset while *omitting* how the call
//! went. [`Completion`] then makes the stronger half structural: `Outcome::Holds` has a
//! private constructor that requires a `Completion`, and the only source of one is
//! [`Observation::completion`], which yields `None` for every result but
//! [`InvocationResult::Completed`].

use datamodel::{CanonicalChangeset, DerivationFailure, GateAttestation, InvocationResult};

/// Proof that the invocation completed.
///
/// Zero-sized, with a private field, and minted **only** by [`Observation::completion`] —
/// which is in this module, so no other module in this crate (let alone another crate) can
/// construct one. [`crate::Assessment::holds`] requires one by value, which is what makes
/// "an empty changeset from a failed invocation is `unverifiable`, never `holds`" a
/// property of the types rather than a branch a future author has to remember to write.
///
/// Note what it deliberately does *not* gate: `Outcome::Violated`. A non-empty `user_state`
/// partition is positive evidence of a mutation and stands on its own, so a contradiction
/// survives a failed invocation while a confirmation does not (ADR-012 decision 4).
pub(crate) struct Completion(());

/// One run's observable result, as the engine may read it.
///
/// `Copy`: an enum discriminant plus a borrow. The changeset it points at stays owned by
/// the derivation job, which is what keeps this crate free of any allocation it did not
/// need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation<'a> {
    call: InvocationResult,
    derived: Result<&'a CanonicalChangeset, DerivationFailure>,
}

impl<'a> Observation<'a> {
    /// The only constructor. Both halves are required: see the module docs for why that is
    /// the point rather than an inconvenience.
    #[must_use]
    pub const fn new(
        call: InvocationResult,
        derived: Result<&'a CanonicalChangeset, DerivationFailure>,
    ) -> Self {
        Self { call, derived }
    }

    /// How the `tools/call` went.
    #[must_use]
    pub const fn call(&self) -> InvocationResult {
        self.call
    }

    /// The derived changeset, or why there isn't one.
    ///
    /// Available regardless of [`Self::call`], because a change that *is* present is
    /// evidence of a write whether or not the call reported success. Reading an *absence*
    /// from it is what requires the crate-private `Completion` witness.
    pub const fn derived(&self) -> Result<&'a CanonicalChangeset, DerivationFailure> {
        self.derived
    }

    /// A [`Completion`] witness, or `None` if the invocation did not complete.
    pub(crate) fn completion(&self) -> Option<Completion> {
        match self.call {
            InvocationResult::Completed => Some(Completion(())),
            InvocationResult::ToolReportedError | InvocationResult::NoResult => None,
        }
    }
}

/// An [`Observation`] the integrity gate has attested (ADR-004).
///
/// Every public entry point of this crate takes one of these, so there is no way to assess
/// evidence that has not passed the gate: the gate is the only thing that can produce the
/// [`GateAttestation`] this borrows, and a `GatedRun` cannot be built without it. The gate
/// itself does not exist yet (P1-05); this is the boundary it will plug into, designed so
/// that constructing it needs none of the gate's logic.
#[derive(Debug)]
pub struct GatedRun<'a> {
    attestation: &'a GateAttestation,
    observation: Observation<'a>,
}

impl<'a> GatedRun<'a> {
    /// Pair an attested run with what it observed.
    #[must_use]
    pub const fn new(attestation: &'a GateAttestation, observation: Observation<'a>) -> Self {
        Self { attestation, observation }
    }

    /// The `RUN.run_id` the gate attested.
    #[must_use]
    pub fn run_id(&self) -> &'a str {
        self.attestation.run_id()
    }

    /// What the run observed.
    #[must_use]
    pub const fn observation(&self) -> &Observation<'a> {
        &self.observation
    }
}
