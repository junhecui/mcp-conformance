//! Decide whether containment held well enough for the run to count (architecture.md §5.1).
//!
//! **Must not:** Be bypassable by configuration (ADR-004).
//!
//! Contract: [architecture.md §3.1].
//!
//! # P2-03: all four §5.1 branches, in the diagram's own order
//!
//! Phase 1 landed exactly two of the full four-branch gate (`G1` clean teardown, `G3` hard
//! timeout) because the other two needed signals nothing in the codebase could produce yet:
//! a resource-cap hit (`G2`) needs cgroups (P2-02, now done) and an escape-class denied
//! syscall (`G4`) needs seccomp (P4-01, still ahead). This module now implements all four
//! branches structurally, in the diagram's exact order — `G1` → `G2` → `G3` → `G4` — so a run
//! that trips more than one branch always reports the earliest one the diagram would have
//! reached first, never a blend and never whichever field happened to be checked last.
//!
//! `G4` does not gate at all: an escape-class denial is accepted evidence, just flagged
//! (`GateOutcome::Accept { adversarial_flag: true }`) — architecture.md §5.1 is explicit that
//! attempted escapes are among the most interesting findings this harness can produce, so
//! they must reach the verdict engine and publication, not be discarded as "untrustworthy."
//!
//! No dependency on `sandbox` or `observe`'s concrete types: [`RunSignals`] is a small,
//! platform-independent value type this crate defines for itself, so `decide` stays a pure,
//! trivially testable function with nothing platform-specific about the *decision* — only
//! the code that *produces* a `RunSignals` (in `sandbox`/`observe`) needs a kernel to run
//! against.

#![forbid(unsafe_code)]

use datamodel::ReasonCode;

/// What this gate knows about how one run ended.
///
/// `escape_class_syscall_denied` is the same kind of honest placeholder
/// `containment_uncertain` already was in Phase 1: seccomp (P4-01) is the only thing that can
/// ever observe a denied escape-class syscall, and it doesn't exist yet, so no producer in
/// this codebase can set this `true` today. It is defined and gated on *now*, before its
/// producer exists, for the same reason `containment_uncertain` was — so this crate's own
/// behaviour around it is pinned down and tested before P4-01 lands the detector that will
/// actually set it.
///
/// `resource_cap_hit` has a real producer available today: `sandbox::cgroup::Cgroup::usage`
/// (P2-02) exposes the kernel counters (`memory.failcnt`/`memory.events`, `pids.events`) a
/// caller derives this boolean from. `xtask::first_verdict`'s existing demo run doesn't wire
/// a `Cgroup` in yet (that lands with P2-04's run planner, the first real caller that
/// constructs every run inside one), so it currently always passes `false` here — a disclosed
/// gap, not a hidden one, exactly like `containment_uncertain`'s own history in this crate.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunSignals {
    /// Whether the sandbox's hard timeout fired and killed the process, rather than the
    /// process exiting (successfully or not) on its own.
    pub timed_out: bool,
    /// Whether the caller has positive reason to distrust clean teardown.
    pub containment_uncertain: bool,
    /// Whether a cgroup resource cap (`memory.max`, `pids.max`) was actually hit during the
    /// run — see this struct's own doc comment for where the real signal comes from.
    pub resource_cap_hit: bool,
    /// Whether the sandbox denied an escape-class syscall attempt during the run — see this
    /// struct's own doc comment for why nothing can set this `true` yet.
    pub escape_class_syscall_denied: bool,
}

/// What the gate decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// Containment held well enough for this run's evidence to proceed to the verdict
    /// engine.
    Accept {
        /// `G4`: set when an escape-class syscall was denied during an otherwise-accepted
        /// run. Must follow this record all the way into publication (architecture.md
        /// §5.1) — carried here, on the outcome itself, rather than discarded once the gate
        /// has made its accept/reject decision, is what makes that possible.
        adversarial_flag: bool,
    },
    /// architecture.md §5.1: no evidence proceeds to the verdict engine without this
    /// decision — ADR-004's `unverifiable`.
    ///
    /// Carries `datamodel::ReasonCode` — P2-11's closed taxonomy, fixed now that this gate
    /// and the idempotency protocol have both actually run and produced real codes to fix
    /// it from, rather than a second, parallel taxonomy invented in this crate.
    Unverifiable(ReasonCode),
}

/// Decide whether `signals` clears the gate.
///
/// There is no parameter here to skip a check, no feature flag, and no code path that
/// returns [`GateOutcome::Accept`] without every field having been examined — ADR-004's
/// "not configurable off" is the absence of an off switch, not a switch defaulted to on.
/// Order matches architecture.md §5.1's own diagram exactly: `G1` (teardown), then `G2`
/// (resource cap), then `G3` (timeout), then `G4` (escape-class denial, which flags rather
/// than rejects) — so a run tripping more than one branch always reports the earliest one
/// the diagram would reach, never a blend and never whichever field this function happened
/// to check last.
#[must_use]
pub const fn decide(signals: RunSignals) -> GateOutcome {
    if signals.containment_uncertain {
        return GateOutcome::Unverifiable(ReasonCode::ContainmentUncertain);
    }
    if signals.resource_cap_hit {
        return GateOutcome::Unverifiable(ReasonCode::ExecutionTruncated);
    }
    if signals.timed_out {
        return GateOutcome::Unverifiable(ReasonCode::Timeout);
    }
    GateOutcome::Accept { adversarial_flag: signals.escape_class_syscall_denied }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean() -> RunSignals {
        RunSignals::default()
    }

    #[test]
    fn clean_run_is_accepted_unflagged() {
        let outcome = decide(clean());
        assert_eq!(outcome, GateOutcome::Accept { adversarial_flag: false });
    }

    #[test]
    fn timed_out_run_is_unverifiable_with_the_timeout_reason() {
        let outcome = decide(RunSignals { timed_out: true, ..clean() });
        assert_eq!(outcome, GateOutcome::Unverifiable(ReasonCode::Timeout));
    }

    #[test]
    fn containment_uncertain_run_is_unverifiable_with_that_reason() {
        let outcome = decide(RunSignals { containment_uncertain: true, ..clean() });
        assert_eq!(outcome, GateOutcome::Unverifiable(ReasonCode::ContainmentUncertain));
    }

    /// `G2`: a resource-cap hit is unverifiable with `execution_truncated`, distinct from
    /// both the teardown and timeout reasons — a capped run can perfectly well tear down
    /// cleanly and finish before any timeout, and still have produced evidence that proves
    /// nothing because the tool itself never got to finish its work.
    #[test]
    fn resource_cap_hit_run_is_unverifiable_with_the_execution_truncated_reason() {
        let outcome = decide(RunSignals { resource_cap_hit: true, ..clean() });
        assert_eq!(outcome, GateOutcome::Unverifiable(ReasonCode::ExecutionTruncated));
    }

    /// `G4`: an escape-class denial on an otherwise-clean run does not reject the evidence —
    /// it is accepted and flagged, per architecture.md §5.1 ("attempted escapes are among
    /// the most interesting findings the harness can produce").
    #[test]
    fn escape_class_denial_on_a_clean_run_is_accepted_and_flagged() {
        let outcome = decide(RunSignals { escape_class_syscall_denied: true, ..clean() });
        assert_eq!(outcome, GateOutcome::Accept { adversarial_flag: true });
    }

    /// architecture.md §5.1's own branch order: `G1` (teardown) is decided before `G3`
    /// (timeout). Proven, not assumed — a run that is both uncertain and timed out must
    /// report the teardown reason, never let the timeout branch quietly win or blend the
    /// two into something ambiguous.
    #[test]
    fn containment_uncertain_takes_priority_over_timed_out() {
        let outcome = decide(RunSignals { timed_out: true, containment_uncertain: true, ..clean() });
        assert_eq!(outcome, GateOutcome::Unverifiable(ReasonCode::ContainmentUncertain));
    }

    /// `G1` before `G2`: containment uncertainty outranks a resource-cap hit too — an
    /// uncertain teardown is the more fundamental problem regardless of what else happened
    /// during the run.
    #[test]
    fn containment_uncertain_takes_priority_over_resource_cap_hit() {
        let outcome =
            decide(RunSignals { containment_uncertain: true, resource_cap_hit: true, ..clean() });
        assert_eq!(outcome, GateOutcome::Unverifiable(ReasonCode::ContainmentUncertain));
    }

    /// `G2` before `G3`, matching the diagram's own top-to-bottom order: a run that both hit
    /// its resource cap and (consequently, or coincidentally) timed out reports the cap hit,
    /// not the timeout — a cap hit is diagnostic of *why* the run needed killing at all, so
    /// it is the more specific and more useful reason to surface.
    #[test]
    fn resource_cap_hit_takes_priority_over_timed_out() {
        let outcome = decide(RunSignals { resource_cap_hit: true, timed_out: true, ..clean() });
        assert_eq!(outcome, GateOutcome::Unverifiable(ReasonCode::ExecutionTruncated));
    }

    /// `G4` never overrides an unverifiable outcome from `G1`–`G3`: an escape attempt during
    /// a run that was *also* truncated by a resource cap is still unverifiable evidence —
    /// there is no accept-and-flag path that bypasses the earlier, rejecting branches.
    #[test]
    fn escape_class_denial_does_not_rescue_an_otherwise_unverifiable_run() {
        let outcome =
            decide(RunSignals { resource_cap_hit: true, escape_class_syscall_denied: true, ..clean() });
        assert_eq!(outcome, GateOutcome::Unverifiable(ReasonCode::ExecutionTruncated));

        let outcome = decide(RunSignals {
            containment_uncertain: true,
            escape_class_syscall_denied: true,
            ..clean()
        });
        assert_eq!(outcome, GateOutcome::Unverifiable(ReasonCode::ContainmentUncertain));

        let outcome =
            decide(RunSignals { timed_out: true, escape_class_syscall_denied: true, ..clean() });
        assert_eq!(outcome, GateOutcome::Unverifiable(ReasonCode::Timeout));
    }

    /// ADR-004, made literal: nothing about a run's *other* properties can be exploited to
    /// avoid `ContainmentUncertain` once the flag itself is true, and nothing can force
    /// `Accept` while it is — there is no bypass path hiding in the other three fields, now
    /// that there are four instead of two.
    #[test]
    fn there_is_no_combination_of_inputs_that_bypasses_a_true_containment_uncertain_flag() {
        for timed_out in [false, true] {
            for resource_cap_hit in [false, true] {
                for escape_class_syscall_denied in [false, true] {
                    let outcome = decide(RunSignals {
                        containment_uncertain: true,
                        timed_out,
                        resource_cap_hit,
                        escape_class_syscall_denied,
                    });
                    assert_eq!(
                        outcome,
                        GateOutcome::Unverifiable(ReasonCode::ContainmentUncertain),
                        "timed_out={timed_out} resource_cap_hit={resource_cap_hit} \
                         escape_class_syscall_denied={escape_class_syscall_denied} must not \
                         change the outcome when containment is uncertain"
                    );
                }
            }
        }
    }
}
