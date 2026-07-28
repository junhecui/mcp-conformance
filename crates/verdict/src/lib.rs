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

use alloc::vec::Vec;

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

/// Decide `idempotentHint` from architecture.md §4.2's multi-arm decision tree.
///
/// Framed the way architecture.md §4.2 itself frames it, in the metamorphic-testing
/// vocabulary (Segura et al., IEEE TSE 2017): `D1 ≡ D2` is an *equivalence metamorphic
/// relation* over the state-transformation output, and `noise_floor` (`N = D1 Δ D1'`,
/// P2-08) is the *tolerance* under which that relation is evaluated — not an exact-equality
/// check, because two genuinely identical single calls are already known (from measuring
/// `N` itself) not to produce byte-identical changesets.
///
/// Every parameter here is already a computed *difference set* — which paths two evidence
/// captures disagreed on, however the caller defines "disagreed" (by presence, or by
/// content). Deciding *how* to compare two captures needs real I/O (reading file content
/// requires a filesystem, `noise_floor`'s own consistent computation with `d2_delta_d1`/
/// `d2r_delta_d1` needs to run the same comparison architecture-wide) — this crate's own
/// "must not call a clock [or] a network" contract already forbids a filesystem read
/// happening in here, so producing these sets is the caller's job (in practice,
/// `orchestrator`), and this function's job is only the decision over already-computed sets.
///
/// Order matches the diagram exactly: `D2 Δ D1` (`C1`) is checked before `D2R Δ D1` (`C2`) —
/// a tool whose second call already shows an effect outside the noise floor is `violated`
/// regardless of what a restart would additionally show. The restart-only branch exists
/// specifically to separate "genuinely non-idempotent" from "cached only within one
/// process," and only matters once the simpler explanation is already ruled out.
#[must_use]
pub fn idempotent_hint(
    d2_delta_d1: &[Vec<u8>],
    d2r_delta_d1: &[Vec<u8>],
    noise_floor: &[Vec<u8>],
) -> Assessment {
    if !is_subset(d2_delta_d1, noise_floor) {
        return Assessment::violated(Oracle::KernelChangeset);
    }
    if !is_subset(d2r_delta_d1, noise_floor) {
        return Assessment::unverifiable(Oracle::KernelChangeset, ReasonCode::CachingSuppressedInProcess);
    }
    Assessment::holds(Oracle::KernelChangeset)
}

fn is_subset(delta: &[Vec<u8>], noise_floor: &[Vec<u8>]) -> bool {
    delta.iter().all(|path| noise_floor.iter().any(|n| n == path))
}

/// Decide `openWorldHint` from architecture.md §4.4's decision tree.
///
/// # `egress_attempted`'s actual provenance, and why S1 is not evaluated from the strict arm
/// alone
///
/// architecture.md §4.4 draws "Egress attempted?" as a question the *strict* arm (P3-01, no
/// route out) answers on its own, before ever running the instrumented arm. That would need
/// a way to observe a `connect()` attempt independent of whether it succeeded — which is
/// exactly what a seccomp/syscall audit log would give (architecture.md's own Phase 4,
/// P4-02, not yet built). Without it, this codebase's only real, non-heuristic observation
/// of "did the tool try to leave the sandbox" is P3-04's own destination classification —
/// which requires the instrumented arm (P3-02's veth and proxy) to produce anything to
/// classify at all. `egress_attempted` is therefore, honestly, "at least one connection
/// classified `External`" (`orchestrator::destination::egress_attempted`, over P3-04's
/// output) — evaluated from an instrumented-arm run, not inferred from the strict arm's bare
/// pass/fail. A future P4-02 could let a cheaper strict-arm-only fast path answer S1 directly
/// without the second run this simplification always pays for; that is a performance
/// optimisation over this same decision, not a different one.
///
/// # Why a `true` declaration is never contradicted
///
/// `declared = true` ("this tool may reach outside the sandbox") is a claim about
/// *capability*, not a promise that any one particular invocation will actually use it — the
/// same reasoning [`read_only_hint`]'s own doc comment gives for why a `false` declaration
/// there is never contradicted. No combination of `egress_attempted`/`tool_succeeded`
/// falsifies a `true` declaration, so this function returns `Holds` unconditionally for it.
///
/// # The three branches a `false` (closed-world) declaration actually decides between
///
/// - `egress_attempted = true` → `Violated`: the tool left the sandbox despite declaring it
///   never would (architecture.md §4.4's "openWorld = true, contradicts false declaration").
/// - `egress_attempted = false, tool_succeeded = true` → `Holds`: no attempt to leave, and
///   the tool finished its work anyway — consistent with a genuinely closed world.
/// - `egress_attempted = false, tool_succeeded = false` → `Unverifiable` with
///   [`ReasonCode::EgressAmbiguousRerunInstrumented`]: the tool failed without ever trying to
///   leave — undecidable whether the missing network *caused* that failure, or something
///   unrelated did, without the instrumented rerun the reason code names.
#[must_use]
pub fn open_world_hint(declared: bool, egress_attempted: bool, tool_succeeded: bool) -> Assessment {
    if !declared {
        if egress_attempted {
            return Assessment::violated(Oracle::KernelChangeset);
        }
        if !tool_succeeded {
            return Assessment::unverifiable(
                Oracle::KernelChangeset,
                ReasonCode::EgressAmbiguousRerunInstrumented,
            );
        }
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

        let reason = ReasonCode::Timeout;
        let u = Assessment::unverifiable(Oracle::KernelChangeset, reason);
        assert_eq!(u.outcome(), Outcome::Unverifiable);
        assert_eq!(u.reason(), Some(&reason));
    }

    fn path(s: &str) -> alloc::vec::Vec<u8> {
        s.as_bytes().to_vec()
    }

    #[test]
    fn no_differences_anywhere_holds() {
        let assessment = idempotent_hint(&[], &[], &[]);
        assert_eq!(assessment, Assessment::holds(Oracle::KernelChangeset));
    }

    /// `C1`: a `D2 Δ D1` difference outside the noise floor is `violated`, regardless of
    /// what `D2R Δ D1` shows — a genuinely non-idempotent tool doesn't need a restart to
    /// prove it.
    #[test]
    fn a_d2_delta_outside_the_noise_floor_is_violated() {
        let assessment = idempotent_hint(&[path("extra.txt")], &[], &[]);
        assert_eq!(assessment, Assessment::violated(Oracle::KernelChangeset));
    }

    /// `C2`: `D2 Δ D1` is fully within `N`, but `D2R Δ D1` shows something outside it — the
    /// exact caching-confound shape architecture.md §4.2 describes (an effect suppressed
    /// in-process, reappearing after a restart).
    #[test]
    fn a_d2r_delta_outside_the_noise_floor_alone_is_unverifiable_with_the_caching_reason() {
        let assessment = idempotent_hint(&[], &[path("effect.txt")], &[]);
        assert_eq!(
            assessment,
            Assessment::unverifiable(Oracle::KernelChangeset, ReasonCode::CachingSuppressedInProcess)
        );
    }

    #[test]
    fn both_deltas_fully_within_the_noise_floor_holds() {
        let n = [path("noisy.tmp")];
        let assessment = idempotent_hint(&[path("noisy.tmp")], &[path("noisy.tmp")], &n);
        assert_eq!(assessment, Assessment::holds(Oracle::KernelChangeset));
    }

    /// `C1` before `C2`, exactly matching the diagram's own top-to-bottom order: a tool
    /// whose `D2 Δ D1` *and* `D2R Δ D1` both show a difference outside `N` is still reported
    /// `violated`, not the restart-only `unverifiable` reason — the diagram never reaches
    /// `C2` once `C1` has already failed.
    #[test]
    fn d2_delta_failing_takes_priority_over_d2r_delta_also_failing() {
        let assessment =
            idempotent_hint(&[path("extra.txt")], &[path("another.txt")], &[]);
        assert_eq!(assessment, Assessment::violated(Oracle::KernelChangeset));
    }

    /// A larger noise floor than what either delta actually needs does not change the
    /// outcome — `⊆`, not `==`.
    #[test]
    fn a_noise_floor_larger_than_either_delta_still_holds() {
        let n = [path("a.tmp"), path("b.tmp"), path("c.tmp")];
        let assessment = idempotent_hint(&[path("a.tmp")], &[path("b.tmp")], &n);
        assert_eq!(assessment, Assessment::holds(Oracle::KernelChangeset));
    }

    /// Empty deltas are always a subset of any noise floor, including an empty one — the
    /// trivial case that every other test's "holds" branch quietly depends on.
    #[test]
    fn empty_deltas_are_a_subset_of_any_noise_floor() {
        let n = [path("whatever.tmp")];
        assert_eq!(idempotent_hint(&[], &[], &n), Assessment::holds(Oracle::KernelChangeset));
        assert_eq!(idempotent_hint(&[], &[], &[]), Assessment::holds(Oracle::KernelChangeset));
    }

    /// architecture.md §4.4's literal exit criterion, first branch: egress attempted despite
    /// a closed-world declaration contradicts it, regardless of whether the tool itself
    /// happened to succeed or fail — the contradiction is already established the moment
    /// egress is observed.
    #[test]
    fn egress_attempted_against_a_false_declaration_is_violated_regardless_of_success() {
        assert_eq!(
            open_world_hint(false, true, true),
            Assessment::violated(Oracle::KernelChangeset)
        );
        assert_eq!(
            open_world_hint(false, true, false),
            Assessment::violated(Oracle::KernelChangeset)
        );
    }

    /// Second branch: no egress attempted, and the tool finished its work anyway —
    /// consistent with a genuinely closed world.
    #[test]
    fn no_egress_and_the_tool_succeeded_holds_for_a_false_declaration() {
        assert_eq!(open_world_hint(false, false, true), Assessment::holds(Oracle::KernelChangeset));
    }

    /// Third branch: no egress attempted, but the tool also failed — genuinely ambiguous
    /// whether the missing network caused the failure, so this is `unverifiable` with the
    /// specific reason code naming the rerun that would resolve it, never a silent `holds` or
    /// `violated`.
    #[test]
    fn no_egress_and_the_tool_failed_is_unverifiable_with_the_rerun_reason() {
        assert_eq!(
            open_world_hint(false, false, false),
            Assessment::unverifiable(
                Oracle::KernelChangeset,
                ReasonCode::EgressAmbiguousRerunInstrumented
            )
        );
    }

    /// A `true` (open-world) declaration is a capability claim, not a per-invocation promise
    /// — the same asymmetry `read_only_hint`'s own `false`-declaration case already
    /// establishes. No combination of observations contradicts it.
    #[test]
    fn a_true_declaration_holds_regardless_of_egress_or_success() {
        for egress_attempted in [false, true] {
            for tool_succeeded in [false, true] {
                assert_eq!(
                    open_world_hint(true, egress_attempted, tool_succeeded),
                    Assessment::holds(Oracle::KernelChangeset),
                    "declared=true must always hold (egress_attempted={egress_attempted}, tool_succeeded={tool_succeeded})"
                );
            }
        }
    }
}
