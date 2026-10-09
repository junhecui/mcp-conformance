//! The one cross-oracle reason code that is spelled twice, guarded where it compiles.
//!
//! ADR-012 decision 9 records that `invocation_failed` is deliberately the **same** string
//! in `verdict::reason` and in `probe::protocol`, so the two oracles' records read the same
//! way where they mean the same thing, and that it is **not single-sourced**: `verdict` and
//! `probe` cannot share a constant without a dependency edge ADR-005 forbids, and P2-11 is
//! where the taxonomy should become one artefact in `datamodel`.
//!
//! What that disclosure said next was that renaming one and not the other would silently
//! split a published category — and then left it at that. But "not single-sourceable" and
//! "not guardable" are different claims, and only the first is true: `xtask` can depend on
//! both crates without either depending on the other, so the hazard is checkable today, and
//! this is the check. It costs one assert and it fails the build on the exact mistake the
//! caveat predicts. (`probe` re-exports its two reason-code constructors from its crate root
//! for this; they were `pub` inside a private module before, which is the one thing that
//! genuinely made this unwritable.)
//!
//! Deliberately asserts the **strings**, not that one is defined in terms of the other.
//! There is no shared definition to assert, which is the whole point; what must hold is that
//! two independent definitions still agree.

/// A rename on either side of the ADR-005 boundary splits one published reason-code category
/// into two that look unrelated in a results table. P2-11 should replace this with a single
/// shared constant in `datamodel`; until then, this is what notices.
#[test]
fn both_oracles_still_spell_invocation_failed_the_same_way() {
    let from_probe = probe::invocation_failed();
    let probe_code = from_probe
        .reason
        .as_ref()
        .map(|r| r.0.as_str())
        .expect("an unverifiable probe assessment always carries a reason");

    assert_eq!(
        probe_code,
        verdict::reason::INVOCATION_FAILED,
        "`invocation_failed` is spelled independently in `verdict::reason` and \
         `probe::protocol` (ADR-005 forbids sharing a constant). They have drifted apart, \
         which silently splits one published category in two — rename both, or single-source \
         the taxonomy in `datamodel` as P2-11 intends."
    );

    // Both sides reach the same outcome with that code, too: a shared spelling attached to
    // different outcomes would be worse than a split spelling, because it would read as one
    // category in a report while meaning two things.
    assert_eq!(from_probe.outcome, datamodel::Outcome::Unverifiable);
}
