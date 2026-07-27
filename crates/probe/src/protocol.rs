//! probe → invoke → probe: the decision logic for `readOnlyHint` and `idempotentHint` over
//! the weaker, resources-only oracle (architecture.md §2).
//!
//! Symmetric in spirit to the kernel-changeset protocols in design.md §6 / architecture.md
//! §4, but deliberately simpler: no noise-floor arm (ADR-003 exists because the
//! kernel-changeset protocol can afford independent repeat runs from a byte-identical base;
//! a live third-party server offers neither independence nor the politeness budget for that
//! many extra requests), and no restart-interleaved caching check (there is no way to
//! restart a server this harness doesn't control). What is left is the honest core: did the
//! state the probe can see change, or not — and where that alone can't decide something,
//! the answer is `unverifiable`, never a guess dressed up as `holds`.
//!
//! These functions are pure over already-taken snapshots — no I/O here, so the decision
//! itself is trivially unit-testable without a network. (Not subject to ADR-005's purity
//! *rule*, which binds only `normalise`/`verdict`; this module just happens to benefit from
//! the same discipline for the same reason.)

use datamodel::{Digest, Oracle, Outcome, ReasonCode};

/// One protocol-probe assessment. [`Self::ORACLE`] is always [`Oracle::ProtocolProbe`] —
/// there is no constructor in this crate that produces any other oracle value, so a caller
/// cannot accidentally tag a weak-oracle result as if it came from a kernel changeset
/// (B-03's concern, enforced here by construction rather than by convention).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeAssessment {
    /// Whether the annotation held, was violated, or could not be decided from this
    /// oracle.
    pub outcome: Outcome,
    /// Why, when [`Self::outcome`] is [`Outcome::Unverifiable`] — mandatory in exactly that
    /// case, mirroring `verdict::Assessment`'s same rule for the Class A path
    /// (architecture.md §6 invariant 3).
    pub reason: Option<ReasonCode>,
}

impl ProbeAssessment {
    /// Every [`ProbeAssessment`] this crate produces was reached this way. Named as an
    /// associated constant, not a field, precisely so it can never vary per-instance.
    pub const ORACLE: Oracle = Oracle::ProtocolProbe;
}

fn holds() -> ProbeAssessment {
    ProbeAssessment { outcome: Outcome::Holds, reason: None }
}

fn violated() -> ProbeAssessment {
    ProbeAssessment { outcome: Outcome::Violated, reason: None }
}

fn unverifiable(reason: ReasonCode) -> ProbeAssessment {
    ProbeAssessment { outcome: Outcome::Unverifiable, reason: Some(reason) }
}

/// A probe run that never got a usable surface at all — no resources, or `resources/list`
/// itself unsupported. The rest must honestly stay `unverifiable`, per B-01's exit
/// criterion, rather than forcing a verdict the surface can't support.
#[must_use]
pub fn no_probe_surface() -> ProbeAssessment {
    unverifiable(ReasonCode::NoProbeSurface)
}

/// A probe run that reached the invoke step but the call itself failed — most often because
/// [`crate::argsynth_min`]'s placeholder arguments didn't satisfy the tool's real semantic
/// requirements (design.md §8's "semantic argument validity" limitation, arriving here as a
/// concrete reason code instead of a silent gap).
#[must_use]
pub fn invocation_failed() -> ProbeAssessment {
    unverifiable(ReasonCode::InvocationFailed)
}

/// Decide `readOnlyHint` from one before/after snapshot pair.
///
/// `declared` is the tool's *effective* declared value: `true` only when explicitly
/// declared `true`, the spec default (`false`) otherwise — the same convention `census`
/// uses for "declared" elsewhere in this codebase.
///
/// | declared | snapshot changed | outcome | why |
/// |---|---|---|---|
/// | `true` | no | `holds` | consistent with a read-only claim |
/// | `true` | yes | `violated` | contradicts a read-only claim |
/// | `false` | yes | `holds` | confirms the tool does mutate observable state |
/// | `false` | no | `unverifiable` | absence of an *observed* change doesn't confirm mutation happened nowhere — this oracle only sees what the server chose to expose as a resource (architecture.md §2), so "nothing changed here" is not "nothing changed" |
#[must_use]
pub fn assess_read_only(declared: bool, before: Digest, after: Digest) -> ProbeAssessment {
    let changed = before != after;
    match (declared, changed) {
        (true, false) => holds(),
        (true, true) => violated(),
        (false, true) => holds(),
        (false, false) => unverifiable(ReasonCode::ProbeSurfaceIncomplete),
    }
}

/// Decide `idempotentHint` from the state after the first call (`s1`) and after an
/// identical second call (`s2`). The state *before* the first call plays no role in this
/// comparison — idempotence asks whether the second call added anything beyond the first,
/// which is exactly `s1` vs. `s2`, independent of where the sequence started.
///
/// Same decision shape as [`assess_read_only`], substituting "the second call changed
/// observable state" for "the call changed observable state":
///
/// | declared | s1 ≠ s2 | outcome |
/// |---|---|---|
/// | `true` | no | `holds` |
/// | `true` | yes | `violated` |
/// | `false` | yes | `holds` |
/// | `false` | no | `unverifiable` (`probe_surface_incomplete`) |
#[must_use]
pub fn assess_idempotent(declared: bool, s1: Digest, s2: Digest) -> ProbeAssessment {
    let changed_by_second_call = s1 != s2;
    match (declared, changed_by_second_call) {
        (true, false) => holds(),
        (true, true) => violated(),
        (false, true) => holds(),
        (false, false) => unverifiable(ReasonCode::ProbeSurfaceIncomplete),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Digest = Digest::from_bytes([0u8; 32]);
    const B: Digest = Digest::from_bytes([1u8; 32]);

    #[test]
    fn read_only_declared_true_unchanged_holds() {
        assert_eq!(assess_read_only(true, A, A).outcome, Outcome::Holds);
    }

    #[test]
    fn read_only_declared_true_changed_violated() {
        let a = assess_read_only(true, A, B);
        assert_eq!(a.outcome, Outcome::Violated);
        assert!(a.reason.is_none(), "a decisive outcome must not carry a reason code");
    }

    #[test]
    fn read_only_declared_false_changed_holds() {
        assert_eq!(assess_read_only(false, A, B).outcome, Outcome::Holds);
    }

    #[test]
    fn read_only_declared_false_unchanged_unverifiable_with_reason() {
        let a = assess_read_only(false, A, A);
        assert_eq!(a.outcome, Outcome::Unverifiable);
        assert_eq!(a.reason, Some(ReasonCode::ProbeSurfaceIncomplete));
    }

    #[test]
    fn idempotent_declared_true_second_call_added_nothing_holds() {
        assert_eq!(assess_idempotent(true, A, A).outcome, Outcome::Holds);
    }

    #[test]
    fn idempotent_declared_true_second_call_changed_state_violated() {
        assert_eq!(assess_idempotent(true, A, B).outcome, Outcome::Violated);
    }

    #[test]
    fn idempotent_declared_false_second_call_changed_state_holds() {
        assert_eq!(assess_idempotent(false, A, B).outcome, Outcome::Holds);
    }

    #[test]
    fn idempotent_declared_false_second_call_added_nothing_unverifiable() {
        let a = assess_idempotent(false, A, A);
        assert_eq!(a.outcome, Outcome::Unverifiable);
        assert_eq!(a.reason, Some(ReasonCode::ProbeSurfaceIncomplete));
    }

    #[test]
    fn every_assessment_from_this_module_is_tagged_protocol_probe() {
        // Not a behavioural assertion about ProbeAssessment (the oracle isn't even a field
        // on it) — this documents and locks in the structural guarantee: ORACLE is a
        // `const`, so it is the same value for every assessment this crate can ever
        // produce, by construction, not by every call site remembering to set it.
        assert_eq!(ProbeAssessment::ORACLE, Oracle::ProtocolProbe);
    }

    #[test]
    fn no_probe_surface_and_invocation_failed_are_distinct_reason_codes() {
        assert_ne!(no_probe_surface().reason, invocation_failed().reason);
        assert_eq!(no_probe_surface().outcome, Outcome::Unverifiable);
        assert_eq!(invocation_failed().outcome, Outcome::Unverifiable);
    }
}
