//! Shared vocabulary: pure data types with no behaviour.
//!
//! Every other crate depends on this one and it depends on nothing. That is what lets
//! `normalise` and `verdict` name their inputs and outputs without reaching for an I/O
//! crate — see [ADR-007] and ADR-005.
//!
//! `no_std` is deliberate: a clock or a filesystem is not merely discouraged here, it is
//! not linked.
//!
//! Named `datamodel` rather than `model` because "model" already means *language model*
//! throughout the design docs, and this crate sits inside the pure allowlist where that
//! ambiguity would be actively dangerous.
//!
//! [ADR-007]: ../../../docs/adr/007-implementation-language.md

#![no_std]

extern crate alloc;

use alloc::string::String;

/// The outcome of a single conformance assessment.
///
/// `Unverifiable` is a first-class verdict, not an error path (design.md §9). Collapsing it
/// into `Holds` would be the worst available failure mode for a trust signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// Observation is consistent with the declared annotation.
    Holds,
    /// Observation contradicts the declared annotation.
    Violated,
    /// The protocol could not decide. Always accompanied by a [`ReasonCode`].
    Unverifiable,
}

/// Which observation surface produced a verdict.
///
/// Recorded on every verdict so that results are never aggregated across oracles without
/// disclosure (ADR-002). B-03 makes that a test rather than a habit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Oracle {
    /// Kernel-provided overlay changeset. The strong oracle; Class A only.
    KernelChangeset,
    /// Probe → invoke → probe over the protocol surface. Weaker: it observes only the
    /// state the server chooses to expose.
    ProtocolProbe,
}

/// Whether a server can be contained, decided at intake (ADR-002).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContainabilityClass {
    /// Launchable locally. Full containment, full observation suite.
    A,
    /// Remote HTTP endpoint only. Protocol proxy at most; no changeset, no syscalls.
    B,
    /// Genuinely undecidable. A real class, never a fallback — the classifier must not guess.
    Unclassifiable,
}

/// Why an [`Outcome::Unverifiable`] was reached.
///
/// Deliberately an open newtype for now. The closed taxonomy is P2-11 — *"`unverifiable`
/// without a reason is not a finding, it is a shrug"* — and fixing the variants before the
/// integrity gate and the idempotency protocol have run would be guessing at the very
/// codes the paper reports.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReasonCode(pub String);

/// Evidence exactly as harvested, before any normalisation.
///
/// Shape is deferred to ADR-009 (F-07). Two constraints already bind it: the primary
/// artifact is a *directory tree*, not a byte string, and **capture is lossless** —
/// mtimes and inode data are noise but must still be stored, because discarding them at
/// capture time is normalisation, and ADR-005 requires normalisation to be a pure function
/// of stored evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RawEvidence {}

/// A normalisation ruleset, already parsed.
///
/// Passed in as a value rather than a path. That is the precondition that keeps
/// `normalise` free of I/O: YAML lives in `rulesets/`, and parsing it is the caller's job.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Ruleset {}

/// A changeset after normalisation — the input to every verification protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CanonicalChangeset {}

/// A SHA-256 content digest identifying a blob in the evidence store (F-05).
///
/// Pure value type: hashing needs an algorithm implementation, which is [`store`]'s job,
/// not this crate's — `datamodel` stays free of behaviour so `normalise` and `verdict`
/// stay `no_std` and dependency-free (ADR-005). This type only carries the 32 bytes and
/// knows how to print them; `EVIDENCE.digest` (architecture.md §6) is one of these.
///
/// [`store`]: ../../store/index.html
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Wrap an already-computed digest. Callers are responsible for the hashing itself.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl core::fmt::Display for Digest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
