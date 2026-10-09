//! The engine's output: an [`Assessment`].
//!
//! The per-partition counts it reports alongside the outcome are
//! [`datamodel::PartitionCounts`], which lives there rather than here because `store` has to
//! write them to `VERDICT` and ADR-005 forbids an edge between `verdict` and `store`.

use alloc::string::{String, ToString};

use datamodel::{InvocationResult, Oracle, Outcome, PartitionCounts, ReasonCode};

use crate::observation::Completion;

/// The result of applying one verification protocol.
///
/// `reason` is mandatory whenever `outcome` is [`Outcome::Unverifiable`] — architecture.md
/// §6, invariant 3. The database enforces the same rule as a constraint (F-06); this type
/// exists so the constraint is not the only thing standing between us and a shrug. Every
/// constructor here is private to this crate and sets the two together, so the pairing
/// cannot be got wrong at a call site.
///
/// # Why the fields are private
///
/// They were public in the first version of this crate, on the reasoning that a report has
/// to read them and that `probe::protocol::ProbeAssessment` has the same shape. A review
/// pass then compiled an external crate that forged a `holds` three ways: a bare struct
/// literal, a fake `impl datamodel::IntegrityGate`, and — the one nothing disclosed —
/// `a.outcome = Outcome::Holds; a.reason = None;` applied to an `Assessment` this engine had
/// just produced. The third is the plausible one, because a driver post-processing a result
/// never has to write `Outcome::Holds` beside a struct literal to reach it. Private fields
/// with accessors close all three for every crate but this one, at the cost of four methods.
///
/// What that does **not** buy, stated rather than implied. Nothing here can stop a driver
/// writing whatever row it likes straight through `store::db::insert_verdict`: that is the
/// real forgery surface and no signature in this crate can guard it. And invariant 3 is
/// enforced *at construction* and now maintained against outside mutation, but the
/// database's own `CHECK` still only catches a *cleared reason* at insert time — it cannot
/// catch a driver that flipped an `Unverifiable` row to `Holds` **and** dropped the reason,
/// because the resulting row is internally consistent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    outcome: Outcome,
    reason: Option<ReasonCode>,
    call: InvocationResult,
    reported: Option<PartitionCounts>,
    ruleset_identity: Option<String>,
}

impl Assessment {
    /// Every [`Assessment`] this crate produces was reached this way: the kernel-provided
    /// overlay changeset (architecture.md §5), the strong oracle, Class A only.
    ///
    /// An associated constant, not a field — the same construction
    /// `probe::protocol::ProbeAssessment::ORACLE` uses for the weak oracle, and for the
    /// same reason: it cannot vary per instance, so no call site can mislabel one oracle's
    /// result as the other's. ADR-002 forbids aggregating across oracles without
    /// disclosure and B-03 makes that a test; this is the half that makes the tag itself
    /// trustworthy.
    pub const ORACLE: Oracle = Oracle::KernelChangeset;

    /// Whether the annotation held, was violated, or could not be decided.
    #[must_use]
    pub const fn outcome(&self) -> Outcome {
        self.outcome
    }

    /// Why, when [`Self::outcome`] is [`Outcome::Unverifiable`]; `None` otherwise. One of
    /// [`crate::reason`]'s codes.
    #[must_use]
    pub const fn reason(&self) -> Option<&ReasonCode> {
        self.reason.as_ref()
    }

    /// How the `tools/call` behind this assessment went.
    ///
    /// Carried onto the assessment, not merely consulted while deciding, because the stored
    /// row needs it: ADR-012 decision 4 deliberately allows a `violated` resting on a
    /// *failed* invocation, and without this a `violated` from a clean successful call is
    /// byte-identical in storage to one from a call that errored. A maintainer objecting
    /// *"your harness called my tool a violation when the call errored"* could then not be
    /// answered from the record, P5-03 could not triage the disclosure, and P5-04 could not
    /// report the two populations apart.
    #[must_use]
    pub const fn call(&self) -> InvocationResult {
        self.call
    }

    /// Counts in all three ADR-008 partitions, for publication alongside the outcome
    /// (architecture.md §4.3). `None` only when derivation produced no changeset at all.
    #[must_use]
    pub const fn reported(&self) -> Option<PartitionCounts> {
        self.reported
    }

    /// `Ruleset::identity()` — `"<label>+sha256:<hex>"` — of the ruleset the changeset was
    /// derived under, carried straight off the [`datamodel::CanonicalChangeset`] so the
    /// record cannot name a different ruleset than the one that produced the outcome. `None`
    /// only when derivation produced no changeset.
    #[must_use]
    pub fn ruleset_identity(&self) -> Option<&str> {
        self.ruleset_identity.as_deref()
    }

    /// Observation is consistent with the declaration.
    ///
    /// Takes a [`Completion`] by value, which is the whole mechanism behind *"an empty
    /// changeset from a failed invocation is `unverifiable`, never `holds`"*: `Completion`
    /// has a private field and is minted only by `Observation::completion`, so this
    /// function is uncallable for a run whose invocation did not complete.
    pub(crate) fn holds(
        _completion: Completion,
        call: InvocationResult,
        reported: Option<PartitionCounts>,
        ruleset_identity: Option<String>,
    ) -> Self {
        Self { outcome: Outcome::Holds, reason: None, call, reported, ruleset_identity }
    }

    /// Observation contradicts the declaration.
    ///
    /// Deliberately needs no [`Completion`]: a contradiction rests on evidence that is
    /// *present*, which a failed invocation cannot explain away (ADR-012 decision 4) — which
    /// is exactly why [`Self::call`] has to reach the record.
    pub(crate) fn violated(
        call: InvocationResult,
        reported: Option<PartitionCounts>,
        ruleset_identity: Option<String>,
    ) -> Self {
        Self { outcome: Outcome::Violated, reason: None, call, reported, ruleset_identity }
    }

    /// The protocol could not decide, for the named reason.
    pub(crate) fn unverifiable(
        reason: &str,
        call: InvocationResult,
        reported: Option<PartitionCounts>,
        ruleset_identity: Option<String>,
    ) -> Self {
        Self {
            outcome: Outcome::Unverifiable,
            reason: Some(ReasonCode(reason.to_string())),
            call,
            reported,
            ruleset_identity,
        }
    }
}
