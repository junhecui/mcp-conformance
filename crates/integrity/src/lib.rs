//! Decide whether containment held well enough for the run to count (architecture.md §5.1).
//!
//! **Must not:** Be bypassable by configuration (ADR-004).
//!
//! Contract: [architecture.md §3.1].
//!
//! # Phase 1 scope: "teardown + timeout only"
//!
//! architecture.md §10 scopes this crate's Phase 1 landing to exactly two of the full
//! four-branch gate in §5.1's diagram — clean teardown and the hard timeout. The other two
//! branches (a resource-cap hit, an escape-class denied syscall) need signals that don't
//! exist yet: cgroups (P2-02) and seccomp (P4-01). P2-03 ("full integrity gate") is where
//! all four land together; this module does not pretend to implement them early.
//!
//! No dependency on `sandbox` or `observe`'s concrete types: [`RunSignals`] is a small,
//! platform-independent value type this crate defines for itself, so `decide` stays a pure,
//! trivially testable function with nothing platform-specific about the *decision* — only
//! the code that *produces* a `RunSignals` (in `sandbox`/`observe`) needs a kernel to run
//! against.

#![forbid(unsafe_code)]

use datamodel::ReasonCode;

/// What P1-05 v1 knows about how one run ended.
///
/// `containment_uncertain` is the honest placeholder for orphan-PID detection: Phase 1 has
/// no PID namespace (P2-01) to enumerate a killed process's surviving descendants at all —
/// `sandbox::supervisor`'s own tests demonstrate exactly this gap directly (a shell's
/// grandchild surviving a `SIGKILL` to the shell) — so no producer in this codebase can set
/// this `true` yet. It exists now, rather than being bolted on later, so this crate's own
/// gating behaviour around it is defined and tested *before* P2-01 lands the detector that
/// will actually set it — the same "define the check before the thing that trips it exists"
/// pattern B-02 already used for the `kernel_changeset` oracle tag.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunSignals {
    /// Whether the sandbox's hard timeout fired and killed the process, rather than the
    /// process exiting (successfully or not) on its own.
    pub timed_out: bool,
    /// Whether the caller has positive reason to distrust clean teardown. Every producer in
    /// this codebase today passes `false` — see this struct's own doc comment for why.
    pub containment_uncertain: bool,
}

/// What the gate decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// Containment held well enough for this run's evidence to proceed to the verdict
    /// engine.
    Accept,
    /// architecture.md §5.1: no evidence proceeds to the verdict engine without this
    /// decision — ADR-004's `unverifiable`.
    ///
    /// Carries `datamodel::ReasonCode` — still the open string newtype `datamodel`'s own
    /// doc comment describes ("fixing the variants before the integrity gate... have run
    /// would be guessing"), not a second, closed taxonomy invented here. P2-11 is where the
    /// corpus-wide closed set gets fixed, once this gate and the idempotency protocol have
    /// actually produced codes to fix it from.
    Unverifiable(ReasonCode),
}

/// The exact text this gate writes for each of its two reasons — kept in one place so nothing
/// downstream (a future `VERDICT.reason_code` writer) can drift from what this module actually
/// produces by re-deriving the string at a second call site.
pub const REASON_CONTAINMENT_UNCERTAIN: &str = "containment_uncertain";

/// See [`REASON_CONTAINMENT_UNCERTAIN`].
pub const REASON_TIMEOUT: &str = "timeout";

/// Decide whether `signals` clears P1-05's gate.
///
/// There is no parameter here to skip a check, no feature flag, and no code path that
/// returns [`GateOutcome::Accept`] without both fields having been examined — ADR-004's
/// "not configurable off" is the absence of an off switch, not a switch defaulted to on.
/// Order matches architecture.md §5.1's own diagram: teardown (`G1`) is decided before the
/// timeout branch (`G3`), so a run that is *both* uncertain-teardown and timed-out is
/// reported as the former, exactly as the diagram's branch order implies.
#[must_use]
pub fn decide(signals: RunSignals) -> GateOutcome {
    if signals.containment_uncertain {
        return GateOutcome::Unverifiable(ReasonCode(REASON_CONTAINMENT_UNCERTAIN.into()));
    }
    if signals.timed_out {
        return GateOutcome::Unverifiable(ReasonCode(REASON_TIMEOUT.into()));
    }
    GateOutcome::Accept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(text: &str) -> ReasonCode {
        ReasonCode(text.into())
    }

    #[test]
    fn clean_run_is_accepted() {
        let outcome = decide(RunSignals { timed_out: false, containment_uncertain: false });
        assert_eq!(outcome, GateOutcome::Accept);
    }

    #[test]
    fn timed_out_run_is_unverifiable_with_the_timeout_reason() {
        let outcome = decide(RunSignals { timed_out: true, containment_uncertain: false });
        assert_eq!(outcome, GateOutcome::Unverifiable(reason(REASON_TIMEOUT)));
    }

    #[test]
    fn containment_uncertain_run_is_unverifiable_with_that_reason() {
        let outcome = decide(RunSignals { timed_out: false, containment_uncertain: true });
        assert_eq!(outcome, GateOutcome::Unverifiable(reason(REASON_CONTAINMENT_UNCERTAIN)));
    }

    /// architecture.md §5.1's own branch order: `G1` (teardown) is decided before `G3`
    /// (timeout). Proven, not assumed — a run that is both uncertain and timed out must
    /// report the teardown reason, never let the timeout branch quietly win or blend the
    /// two into something ambiguous.
    #[test]
    fn containment_uncertain_takes_priority_over_timed_out() {
        let outcome = decide(RunSignals { timed_out: true, containment_uncertain: true });
        assert_eq!(outcome, GateOutcome::Unverifiable(reason(REASON_CONTAINMENT_UNCERTAIN)));
    }

    /// ADR-004, made literal: nothing about a run's *other* properties can be exploited to
    /// avoid `ContainmentUncertain` once the flag itself is true, and nothing can force
    /// `Accept` while it is — there is no bypass path hiding in unexamined fields, because
    /// there are only the two fields, and both gate.
    #[test]
    fn there_is_no_combination_of_inputs_that_bypasses_a_true_containment_uncertain_flag() {
        for timed_out in [false, true] {
            let outcome = decide(RunSignals { timed_out, containment_uncertain: true });
            assert_eq!(
                outcome,
                GateOutcome::Unverifiable(reason(REASON_CONTAINMENT_UNCERTAIN)),
                "timed_out={timed_out} must not change the outcome when containment is uncertain"
            );
        }
    }
}
