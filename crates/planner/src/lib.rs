//! Compile a requested annotation set into the minimal deduplicated set of run specs (architecture.md §4.1).
//!
//! **Must not:** Reorder or share arms that are required to be independent.
//!
//! Contract: [architecture.md §3.1].
//!
//! # Scope: the four arms §4.1's own exit criterion names
//!
//! architecture.md §4.1's diagram draws six arm shapes (`Arm 0` through `Arm N`), but only
//! four of them are driven by *this* function's input — a per-tool request over
//! `{readOnlyHint, idempotentHint, openWorldHint}`:
//!
//! - **`Arm 0` (base only, no invocation)** measures provisioning noise — the diff between
//!   two constructions of the base layer *itself*, with no tool call in between at all. It
//!   is not gated by any requested annotation; it belongs to the world provisioner's own
//!   reproducibility proof (P2-05: "byte-reproducible across constructions"), not to a
//!   per-request arm compiler.
//! - **`Arm N` (single call, instrumented network)** is data-dependent, not
//!   request-dependent: architecture.md §4.4's own diagram only reaches it from the
//!   *ambiguous* branch of `openWorldHint`'s decision tree — after `Arm 1` has already run
//!   and its outcome observed. A static compiler over the request alone cannot know ahead of
//!   time whether that branch will be reached; scheduling `Arm N` unconditionally would not
//!   be the *minimal* set this function is required to produce. It is a follow-up planning
//!   decision for whichever component executes arms and observes `Arm 1`'s outcome, not this
//!   one.
//!
//! Both exclusions are structural, not oversights, and both are named here so a reader
//! auditing this crate against §4.1's six-node diagram finds two nodes deliberately absent
//! and a stated reason, not a silent gap.
//!
//! `destructiveHint` is deliberately not a [`plan`] input at all: architecture.md §4.5
//! decides it from a mechanical proxy over already-normalised evidence plus a
//! held-out-evaluated model classifier, never from a dedicated arm run — there is no arm
//! this crate could ever schedule for it.

#![forbid(unsafe_code)]

use datamodel::Annotation;

/// Which of the three arm-driven annotations were requested for one tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AnnotationRequest {
    /// `readOnlyHint` requested.
    pub read_only_hint: bool,
    /// `idempotentHint` requested.
    pub idempotent_hint: bool,
    /// `openWorldHint` requested (strict-mode arm only — see this module's own doc comment
    /// for why `Arm N`, the instrumented follow-up, is out of scope here).
    pub open_world_hint: bool,
}

/// One of the four arm shapes architecture.md §4.1 assigns to a per-request compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arm {
    /// `Arm 1`: single call, strict network. Load-bearing for up to three annotations at
    /// once — see [`PlannedArm::feeds`].
    SingleCallStrictNetwork,
    /// `Arm 1'`: single call, independent repeat — the other half of the `idempotentHint`
    /// noise floor `N = D1 Δ D1'` (architecture.md §4.2).
    SingleCallIndependentRepeat,
    /// `Arm 2`: double call, same process — `D2`.
    DoubleCallSameProcess,
    /// `Arm 2R`: call, restart, call again — `D2R`, resolving the caching confound.
    CallRestartCall,
}

/// One arm this tool's run needs, and which requested annotation(s) its evidence feeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedArm {
    /// Which tool this arm belongs to. Carried on every arm, not just the plan as a whole,
    /// so a caller cannot accidentally detach an arm from its tool and feed it to another
    /// one's evidence — see this crate's own doc comment on why `Arm`s are never reused
    /// across tools.
    pub tool_id: String,
    /// Which of the four arm shapes this is.
    pub arm: Arm,
    /// Which requested annotation(s) this arm's evidence feeds. `Arm::SingleCallStrictNetwork`
    /// is the only arm that can carry more than one entry — up to all three of
    /// `[ReadOnlyHint, IdempotentHint, OpenWorldHint]` — because it is the *same physical
    /// run*, deduplicated and reused three ways (architecture.md §4.1's own framing: "a
    /// single-invocation run from a clean base serves as the `readOnlyHint` evidence *and*
    /// as the `D1` arm of the idempotency protocol *and* as the strict-mode `openWorldHint`
    /// observation"). Every other arm always carries exactly one entry
    /// (`Annotation::IdempotentHint`), since only the idempotency protocol ever needs `D1'`,
    /// `D2`, or `D2R` at all — never merged into `Arm 1` or into each other, regardless of
    /// which combination of annotations was requested.
    pub feeds: Vec<Annotation>,
}

/// The full set of arms one tool's run needs, in the order architecture.md §4.1's own
/// diagram lists them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunPlan {
    /// Every arm this plan requires, deduplicated. Empty when `requested` asked for nothing.
    pub arms: Vec<PlannedArm>,
}

/// Compile `requested` into the minimal deduplicated arm set for `tool_id`.
///
/// Pure and total: no field of `requested` can make this function fail, panic, or reach for
/// I/O — it only decides *which* arms a caller must go on to construct (each from a fresh,
/// byte-identical base layer — architecture.md §4.1's own requirement on the *executor*,
/// enforced here only in that this type carries no field through which two `PlannedArm`s
/// could ever alias the same sandbox instance) and *why* each is needed.
#[must_use]
pub fn plan(tool_id: &str, requested: AnnotationRequest) -> RunPlan {
    let mut arm1_feeds = Vec::new();
    if requested.read_only_hint {
        arm1_feeds.push(Annotation::ReadOnlyHint);
    }
    if requested.idempotent_hint {
        arm1_feeds.push(Annotation::IdempotentHint);
    }
    if requested.open_world_hint {
        arm1_feeds.push(Annotation::OpenWorldHint);
    }

    let mut arms = Vec::new();
    if !arm1_feeds.is_empty() {
        arms.push(PlannedArm {
            tool_id: tool_id.to_string(),
            arm: Arm::SingleCallStrictNetwork,
            feeds: arm1_feeds,
        });
    }
    if requested.idempotent_hint {
        for arm in [Arm::SingleCallIndependentRepeat, Arm::DoubleCallSameProcess, Arm::CallRestartCall] {
            arms.push(PlannedArm {
                tool_id: tool_id.to_string(),
                arm,
                feeds: vec![Annotation::IdempotentHint],
            });
        }
    }
    RunPlan { arms }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arm_kinds(plan: &RunPlan) -> Vec<Arm> {
        plan.arms.iter().map(|a| a.arm).collect()
    }

    #[test]
    fn nothing_requested_produces_an_empty_plan() {
        let result = plan("tool-a", AnnotationRequest::default());
        assert!(result.arms.is_empty());
    }

    #[test]
    fn read_only_alone_is_a_single_arm_1() {
        let result =
            plan("tool-a", AnnotationRequest { read_only_hint: true, ..Default::default() });
        assert_eq!(arm_kinds(&result), vec![Arm::SingleCallStrictNetwork]);
        assert_eq!(result.arms[0].feeds, vec![Annotation::ReadOnlyHint]);
    }

    #[test]
    fn open_world_alone_is_a_single_arm_1() {
        let result =
            plan("tool-a", AnnotationRequest { open_world_hint: true, ..Default::default() });
        assert_eq!(arm_kinds(&result), vec![Arm::SingleCallStrictNetwork]);
        assert_eq!(result.arms[0].feeds, vec![Annotation::OpenWorldHint]);
    }

    /// `idempotentHint` alone still needs all four arms: `Arm 1` for `D1`, plus the three
    /// arms that are always independent of it and of each other.
    #[test]
    fn idempotent_alone_needs_all_four_arms_in_diagram_order() {
        let result =
            plan("tool-a", AnnotationRequest { idempotent_hint: true, ..Default::default() });
        assert_eq!(
            arm_kinds(&result),
            vec![
                Arm::SingleCallStrictNetwork,
                Arm::SingleCallIndependentRepeat,
                Arm::DoubleCallSameProcess,
                Arm::CallRestartCall,
            ]
        );
        assert_eq!(result.arms[0].feeds, vec![Annotation::IdempotentHint]);
    }

    /// architecture.md §4.1's own headline claim, proven directly: requesting all three at
    /// once still produces exactly one `Arm 1`, carrying all three annotations it feeds —
    /// not three separate single-call arms.
    #[test]
    fn all_three_requested_deduplicates_arm_1_into_one_entry_feeding_all_three() {
        let result = plan(
            "tool-a",
            AnnotationRequest { read_only_hint: true, idempotent_hint: true, open_world_hint: true },
        );
        assert_eq!(
            arm_kinds(&result),
            vec![
                Arm::SingleCallStrictNetwork,
                Arm::SingleCallIndependentRepeat,
                Arm::DoubleCallSameProcess,
                Arm::CallRestartCall,
            ],
            "Arm 1 must appear exactly once even though three annotations feed off it"
        );
        assert_eq!(
            result.arms[0].feeds,
            vec![Annotation::ReadOnlyHint, Annotation::IdempotentHint, Annotation::OpenWorldHint]
        );
    }

    /// The "must not reorder or share arms that are required to be independent" exit
    /// criterion, made literal: `Arm 1'`, `Arm 2`, and `Arm 2R` are always three distinct
    /// entries, in every combination that requests `idempotentHint`, never merged into `Arm
    /// 1` or into each other regardless of what else was also requested.
    #[test]
    fn independent_arms_are_never_merged_across_every_requested_combination() {
        for read_only_hint in [false, true] {
            for open_world_hint in [false, true] {
                let result = plan(
                    "tool-a",
                    AnnotationRequest { read_only_hint, idempotent_hint: true, open_world_hint },
                );
                let independent_arms: Vec<Arm> = result
                    .arms
                    .iter()
                    .filter(|a| a.arm != Arm::SingleCallStrictNetwork)
                    .map(|a| a.arm)
                    .collect();
                assert_eq!(
                    independent_arms,
                    vec![Arm::SingleCallIndependentRepeat, Arm::DoubleCallSameProcess, Arm::CallRestartCall],
                    "read_only_hint={read_only_hint} open_world_hint={open_world_hint}: \
                     Arm 1', Arm 2, and Arm 2R must each appear exactly once, distinct from \
                     Arm 1 and from each other"
                );
                for arm in &result.arms {
                    if arm.arm != Arm::SingleCallStrictNetwork {
                        assert_eq!(
                            arm.feeds,
                            vec![Annotation::IdempotentHint],
                            "an independent arm must never carry a feed it wasn't built for"
                        );
                    }
                }
            }
        }
    }

    /// "Arms are never reused across tools": the same request compiled for two different
    /// tools produces plans whose arms carry different `tool_id`s — there is no shared,
    /// tool-agnostic plan value that a careless caller could feed to the wrong tool's
    /// executor. Compiling the same `tool_id` twice is deterministic, proving there is no
    /// hidden call-order-dependent state that could let two tools' arms alias each other.
    #[test]
    fn arms_are_tagged_with_their_own_tool_and_never_reused_across_tools() {
        let request = AnnotationRequest { idempotent_hint: true, ..Default::default() };
        let plan_a = plan("tool-a", request);
        let plan_b = plan("tool-b", request);

        assert!(plan_a.arms.iter().all(|a| a.tool_id == "tool-a"));
        assert!(plan_b.arms.iter().all(|a| a.tool_id == "tool-b"));
        assert_ne!(plan_a, plan_b, "plans for different tools must not compare equal");

        let plan_a_again = plan("tool-a", request);
        assert_eq!(plan_a, plan_a_again, "compiling the same tool twice must be deterministic");
    }
}
