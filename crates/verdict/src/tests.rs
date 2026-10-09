//! Unit tests for the verdict engine. See [ADR-012] for the decisions under test.
//!
//! [ADR-012]: ../../../docs/adr/012-verdict-engine-and-readonlyhint.md

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use datamodel::{
    CanonicalChangeset, Change, ChangeKind, DerivationFailure, FileType, GateAttestation,
    IntegrityGate, Node, Oracle, Outcome, PathClass, ReasonCode,
};

use super::{
    Assessment, Declared, GatedRun, InvocationResult, Observation, PartitionCounts, reason,
    read_only_hint,
};

// ---------------------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------------------

/// Stands in for `crates/integrity`, which P1-05 has not built. Spelled out rather than
/// hidden in a helper function: minting an attestation is supposed to require declaring
/// yourself the gate at a visible call site, and a test is no exception.
struct FakeGate;
impl IntegrityGate for FakeGate {}

fn attested() -> GateAttestation {
    FakeGate.attest_gate_passed("run-1")
}

fn node(data: &[u8]) -> Node {
    Node {
        file_type: FileType::Regular,
        mode: 0o100_644,
        uid: 1000,
        gid: 1000,
        dev_major: 0,
        dev_minor: 0,
        xattrs: Vec::new(),
        data: data.to_vec(),
    }
}

fn created(path: &[u8]) -> Change {
    Change { path: path.to_vec(), kind: ChangeKind::Created(node(b"payload")) }
}

fn deleted(path: &[u8]) -> Change {
    Change { path: path.to_vec(), kind: ChangeKind::Deleted { was: Some(FileType::Regular) } }
}

/// A changeset with the given changes in each ADR-008 partition.
fn changeset(user: Vec<Change>, internal: Vec<Change>, ephemeral: Vec<Change>) -> CanonicalChangeset {
    CanonicalChangeset {
        ruleset_identity: "v1+sha256:48e55850021a462d5710d72e06b5bebe256b1a8107d5a70b0202a1f0b63c1128"
            .to_string(),
        user_state: user,
        server_internal: internal,
        ephemeral,
    }
}

/// The empty changeset — a tool that wrote nothing anywhere.
fn nothing() -> CanonicalChangeset {
    changeset(vec![], vec![], vec![])
}

/// One write to user state.
fn wrote_user_state() -> CanonicalChangeset {
    changeset(vec![created(b"/home/u/doc.txt")], vec![], vec![])
}

/// Assess `declared` against `changeset` for a given invocation result.
fn assess(declared: Declared, call: InvocationResult, cs: &CanonicalChangeset) -> Assessment {
    let attestation = attested();
    read_only_hint(declared, &GatedRun::new(&attestation, Observation::new(call, Ok(cs))))
}

/// Assess a derivation failure.
fn assess_failed_derivation(declared: Declared, failure: DerivationFailure) -> Assessment {
    let attestation = attested();
    read_only_hint(
        declared,
        &GatedRun::new(&attestation, Observation::new(InvocationResult::Completed, Err(failure))),
    )
}

const EVERY_INVOCATION_RESULT: [InvocationResult; 3] = [
    InvocationResult::Completed,
    InvocationResult::ToolReportedError,
    InvocationResult::NoResult,
];

const EVERY_DECLARED: [Declared; 3] =
    [Declared::Explicit(true), Declared::Explicit(false), Declared::Defaulted];

/// Every variant, written out rather than matched with a wildcard: a fourth cause of
/// derivation failure added later has to be added here too, and the compiler says so.
const EVERY_DERIVATION_FAILURE: [DerivationFailure; 3] = [
    DerivationFailure::MalformedEvidence,
    DerivationFailure::MalformedBaseLayer,
    DerivationFailure::InvalidRuleset,
];

// ---------------------------------------------------------------------------------------
// The exit criterion, and the rest of the decision table
// ---------------------------------------------------------------------------------------

/// P1-07's literal exit criterion: *"`canonical(D1)` non-empty over `user_state`
/// contradicts a `true` declaration."*
#[test]
fn a_user_state_change_contradicts_a_declared_true_read_only_hint() {
    let a = assess(Declared::Explicit(true), InvocationResult::Completed, &wrote_user_state());
    assert_eq!(a.outcome(), Outcome::Violated);
    assert!(a.reason().is_none(), "a decisive outcome must not carry a reason code");
    assert_eq!(a.reported().expect("counts reported").user_state, 1);
}

/// The other three rows of the completed-invocation table, asserted together so the table
/// is readable as a table. Row for row this is the same shape as
/// `probe::protocol::assess_read_only`, the Track B oracle's equivalent; only the fourth
/// row's reason code differs, and ADR-012 decision 3 says why.
#[test]
fn the_completed_invocation_decision_table() {
    let unchanged = nothing();
    let changed = wrote_user_state();
    let c = InvocationResult::Completed;

    assert_eq!(
        assess(Declared::Explicit(true), c, &unchanged).outcome(),
        Outcome::Holds,
        "declared true, nothing in user_state"
    );
    assert_eq!(
        assess(Declared::Explicit(true), c, &changed).outcome(),
        Outcome::Violated,
        "declared true, user_state changed"
    );
    assert_eq!(
        assess(Declared::Explicit(false), c, &changed).outcome(),
        Outcome::Holds,
        "declared false, user_state changed — the declaration is confirmed"
    );
    let undecided = assess(Declared::Explicit(false), c, &unchanged);
    assert_eq!(
        undecided.outcome(),
        Outcome::Unverifiable,
        "declared false, nothing observed — nothing confirmed and nothing contradicted"
    );
    assert_eq!(
        undecided.reason(),
        Some(&ReasonCode(reason::NO_USER_STATE_CHANGE.to_string())),
        "and it must say why"
    );
}

/// `readOnlyHint` defaults to `false` (design.md §1), so a tool that declared nothing is
/// assessed exactly as one that declared `false` — while `is_explicit` keeps the two
/// distinguishable for the coverage story P0-05 reports.
#[test]
fn a_defaulted_read_only_hint_is_assessed_as_false_but_stays_distinguishable() {
    // Pins design.md §1's default: `readOnlyHint` defaults to `false`, unlike
    // `destructiveHint` and `openWorldHint`, which default to `true`. Asserted through
    // `effective` rather than against the constant directly so the check is a real call,
    // not something the compiler folds away.
    assert!(!Declared::Defaulted.effective(super::READ_ONLY_HINT_DEFAULT));
    assert!(Declared::Defaulted.effective(true), "the default is the caller's to supply");
    assert!(!Declared::Defaulted.is_explicit());
    assert!(Declared::Explicit(false).is_explicit());

    for call in EVERY_INVOCATION_RESULT {
        for cs in [nothing(), wrote_user_state()] {
            assert_eq!(
                assess(Declared::Defaulted, call, &cs),
                assess(Declared::Explicit(false), call, &cs),
                "a defaulted readOnlyHint must behave exactly like an explicit false"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------
// The false-`holds` gap (commit 832d990's bug, in Track A)
// ---------------------------------------------------------------------------------------

/// The structural guarantee, checked exhaustively rather than argued: across every
/// combination of declaration, invocation result and changeset this crate can represent,
/// `Outcome::Holds` appears only where the invocation completed.
///
/// `Assessment::holds` requires a `Completion` witness that only
/// `Observation::completion()` mints, so this cannot be made to fail by editing
/// `read_only_hint` alone — but the test is what catches someone widening
/// `Observation::completion` later.
#[test]
fn holds_is_unreachable_for_every_invocation_result_but_completed() {
    let mut seen_holds = 0;
    for declared in EVERY_DECLARED {
        for call in EVERY_INVOCATION_RESULT {
            for cs in [nothing(), wrote_user_state(), changeset(vec![deleted(b"/srv/data")], vec![], vec![])] {
                let a = assess(declared, call, &cs);
                if a.outcome() == Outcome::Holds {
                    seen_holds += 1;
                    assert!(
                        call.is_complete(),
                        "holds reached with invocation result {call:?} — the false-holds gap"
                    );
                }
            }
        }
    }
    assert!(seen_holds > 0, "the loop must actually reach holds, or it proves nothing");
}

/// The specific shape of the Track B bug, transplanted: a tool whose call failed at the
/// *tool* level (`isError: true`) and which wrote nothing must be `unverifiable`, never
/// `holds` — the unchanged state proves nothing when the tool never ran its effectful path.
#[test]
fn a_tool_level_error_with_an_empty_changeset_is_unverifiable_not_holds() {
    let a = assess(Declared::Explicit(true), InvocationResult::ToolReportedError, &nothing());
    assert_eq!(a.outcome(), Outcome::Unverifiable);
    assert_eq!(a.reason(), Some(&ReasonCode(reason::INVOCATION_FAILED.to_string())));
}

/// A crash or a killed process reaches the same place as a tool-level error. One reason
/// code for both today, deliberately: P2-11 splits the taxonomy against observed runs, and
/// the variants exist so that split needs no API change.
#[test]
fn no_result_and_a_tool_level_error_share_one_reason_code_today() {
    let crashed = assess(Declared::Explicit(true), InvocationResult::NoResult, &nothing());
    let errored = assess(Declared::Explicit(true), InvocationResult::ToolReportedError, &nothing());
    assert_eq!(crashed.outcome(), Outcome::Unverifiable);
    assert_eq!(crashed.reason(), errored.reason());
    assert_eq!(crashed.reason(), Some(&ReasonCode(reason::INVOCATION_FAILED.to_string())));
}

/// The deliberate asymmetry (ADR-012 decision 4): a failed invocation cannot manufacture
/// confidence, but it must not suppress a contradiction the kernel already recorded. A tool
/// declaring `readOnlyHint: true` that wrote to user state and *then* errored has still
/// written to user state.
#[test]
fn a_failed_invocation_still_reports_a_contradiction_as_violated() {
    for call in [InvocationResult::ToolReportedError, InvocationResult::NoResult] {
        let a = assess(Declared::Explicit(true), call, &wrote_user_state());
        assert_eq!(a.outcome(), Outcome::Violated, "contradiction suppressed for {call:?}");
        assert!(a.reason().is_none());
    }
}

/// The mirror of the above: with a `false`/defaulted declaration there is nothing to
/// contradict, so a failed invocation yields `unverifiable` even though a change was
/// observed — a *confirmation* is exactly what a failed run cannot license.
#[test]
fn a_failed_invocation_never_confirms_a_declared_false() {
    for declared in [Declared::Explicit(false), Declared::Defaulted] {
        let a = assess(declared, InvocationResult::NoResult, &wrote_user_state());
        assert_eq!(a.outcome(), Outcome::Unverifiable);
        assert_eq!(a.reason(), Some(&ReasonCode(reason::INVOCATION_FAILED.to_string())));
    }
}

// ---------------------------------------------------------------------------------------
// Derivation failures (ADR-012 decision 5)
// ---------------------------------------------------------------------------------------

/// A pure, in-closure derivation failure is a verdict with a reason code, not a missing
/// row — and the three causes keep distinct codes, so a harness-side fault is never
/// published as a finding about a server.
#[test]
fn a_derivation_failure_is_unverifiable_with_its_own_reason_code() {
    for declared in EVERY_DECLARED {
        let malformed = assess_failed_derivation(declared, DerivationFailure::MalformedEvidence);
        assert_eq!(malformed.outcome(), Outcome::Unverifiable);
        assert_eq!(malformed.reason(), Some(&ReasonCode(reason::MALFORMED_EVIDENCE.to_string())));
        assert!(malformed.reported().is_none(), "there is no changeset to report counts from");
        assert!(malformed.ruleset_identity().is_none());

        let bad_rules = assess_failed_derivation(declared, DerivationFailure::InvalidRuleset);
        assert_eq!(bad_rules.outcome(), Outcome::Unverifiable);
        assert_eq!(bad_rules.reason(), Some(&ReasonCode(reason::INVALID_RULESET.to_string())));
        assert_ne!(malformed.reason(), bad_rules.reason());
    }
}

/// The base layer is harness-built (`world::base_layer`, P1-02) and mounted read-only
/// beneath the tool, so a base layer that will not decode is an **operator-side** fault, not
/// a finding about a server — the same category as an uncompilable ruleset, and the reason
/// `InvalidRuleset` was split out in the first place.
///
/// Mapping it to `malformed_evidence` published a harness bug as a finding about a server
/// and, read the other way, handed any server deniability for a real malformed capture. The
/// three codes must stay mutually distinct, which is what this asserts.
#[test]
fn a_malformed_base_layer_is_not_published_as_a_finding_about_the_tool() {
    for declared in EVERY_DECLARED {
        let base = assess_failed_derivation(declared, DerivationFailure::MalformedBaseLayer);
        assert_eq!(base.outcome(), Outcome::Unverifiable);
        assert_eq!(
            base.reason(),
            Some(&ReasonCode(reason::MALFORMED_BASE_LAYER.to_string())),
            "an operator-side failure needs its own code"
        );

        let upper = assess_failed_derivation(declared, DerivationFailure::MalformedEvidence);
        let rules = assess_failed_derivation(declared, DerivationFailure::InvalidRuleset);
        assert_ne!(
            base.reason(),
            upper.reason(),
            "a harness-built base layer and a hostile tool's own writes are different findings"
        );
        assert_ne!(base.reason(), rules.reason());
    }
}

/// A derivation failure dominates the invocation result: with no changeset there is
/// nothing to read either an absence or a contradiction from. Over every variant, so a
/// fourth one added later without a `read_only_hint` arm fails to compile rather than
/// quietly reaching a default.
#[test]
fn a_derivation_failure_is_never_holds_or_violated_whatever_the_invocation_did() {
    let attestation = attested();
    for failure in EVERY_DERIVATION_FAILURE {
        for call in EVERY_INVOCATION_RESULT {
            for declared in EVERY_DECLARED {
                let run = GatedRun::new(&attestation, Observation::new(call, Err(failure)));
                let a = read_only_hint(declared, &run);
                assert_eq!(a.outcome(), Outcome::Unverifiable);
                // The derivation failed; the *run* still happened, and what its call did is
                // a fact about the run rather than about the derivation, so it is carried.
                assert_eq!(a.call(), call, "{failure:?}");
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// `user_state` decides; the other two partitions are reported (architecture.md §4.3)
// ---------------------------------------------------------------------------------------

/// *"Emit the verdict against `user_state` while reporting the other two."* Arbitrary
/// content in `server_internal` and `ephemeral` must leave the outcome untouched — if this
/// test fails, ADR-008's allowlists have become load-bearing for the verdict itself, which
/// is the thing §4.3 exists to prevent.
#[test]
fn only_user_state_can_change_the_outcome() {
    let noise = || {
        (
            vec![created(b"/home/u/.cache/blob"), deleted(b"/home/u/.config/x")],
            vec![created(b"/tmp/t"), deleted(b"/run/app.pid"), created(b"/srv/app.lock")],
        )
    };
    for declared in EVERY_DECLARED {
        for call in EVERY_INVOCATION_RESULT {
            for user in [vec![], vec![created(b"/home/u/doc.txt")]] {
                let (internal, ephemeral) = noise();
                let quiet = changeset(user.clone(), vec![], vec![]);
                let noisy = changeset(user, internal, ephemeral);
                assert_eq!(
                    assess(declared, call, &quiet).outcome(),
                    assess(declared, call, &noisy).outcome(),
                    "server_internal/ephemeral content changed the verdict"
                );
            }
        }
    }
}

/// Reported alongside every outcome, including the undecided ones — that is where a reader
/// most needs to see what *was* written before arguing with the verdict.
#[test]
fn partition_counts_are_reported_even_when_the_outcome_is_undecided() {
    let cs = changeset(
        vec![],
        vec![created(b"/home/u/.cache/a"), created(b"/home/u/.cache/b")],
        vec![created(b"/tmp/t")],
    );
    let a = assess(Declared::Defaulted, InvocationResult::Completed, &cs);
    assert_eq!(a.outcome(), Outcome::Unverifiable);
    let counts = a.reported().expect("counts reported for an undecided outcome");
    assert_eq!(counts, PartitionCounts { user_state: 0, server_internal: 2, ephemeral: 1 });
    assert_eq!(counts.total(), 3);

    // And `PartitionCounts::of` agrees with reading the partitions off the changeset.
    assert_eq!(counts.server_internal, cs.class(PathClass::ServerInternal).len());
    assert_eq!(counts.ephemeral, cs.class(PathClass::Ephemeral).len());
    assert_eq!(counts.user_state, cs.class(PathClass::UserState).len());
}

/// The verdict names the exact ruleset bytes it was derived under, taken off the changeset
/// rather than restated by a caller — the binding ADR-011 decision 9 made tamper-evident
/// is worthless if the record can name a different one.
#[test]
fn the_ruleset_identity_is_carried_off_the_changeset() {
    let cs = wrote_user_state();
    let a = assess(Declared::Explicit(true), InvocationResult::Completed, &cs);
    assert_eq!(a.ruleset_identity(), Some(cs.ruleset_identity.as_str()));
    assert!(
        cs.ruleset_identity.contains("+sha256:"),
        "the identity, not the bare label — see ADR-012 decision 7"
    );
}

// ---------------------------------------------------------------------------------------
// Invariants that hold across the whole table
// ---------------------------------------------------------------------------------------

/// architecture.md §6 invariant 3 — *"`unverifiable` without a reason is not a finding, it
/// is a shrug"* — and its converse: a decisive outcome must not carry one, or a reader
/// cannot tell which rows were actually decided. Checked over the whole reachable table,
/// and against the closed set of codes this crate documents.
#[test]
fn every_unverifiable_carries_a_known_reason_and_no_decisive_outcome_does() {
    const CODES: [&str; 5] = [
        reason::INVOCATION_FAILED,
        reason::MALFORMED_EVIDENCE,
        reason::MALFORMED_BASE_LAYER,
        reason::INVALID_RULESET,
        reason::NO_USER_STATE_CHANGE,
    ];
    let attestation = attested();
    let mut unverifiable = 0;
    let mut decisive = 0;
    for declared in EVERY_DECLARED {
        for call in EVERY_INVOCATION_RESULT {
            let mut cases: Vec<Observation<'_>> = Vec::new();
            let unchanged = nothing();
            let changed = wrote_user_state();
            cases.push(Observation::new(call, Ok(&unchanged)));
            cases.push(Observation::new(call, Ok(&changed)));
            for failure in EVERY_DERIVATION_FAILURE {
                cases.push(Observation::new(call, Err(failure)));
            }
            for observation in cases {
                let a = read_only_hint(declared, &GatedRun::new(&attestation, observation));
                match a.outcome() {
                    Outcome::Unverifiable => {
                        unverifiable += 1;
                        let ReasonCode(code) = a.reason().expect("unverifiable without a reason");
                        assert!(CODES.contains(&code.as_str()), "undocumented reason code {code}");
                    }
                    Outcome::Holds | Outcome::Violated => {
                        decisive += 1;
                        assert!(a.reason().is_none(), "decisive outcome carried a reason code");
                    }
                }
            }
        }
    }
    assert!(unverifiable > 0 && decisive > 0, "both branches must actually be reached");
}

/// Every assessment records how the `tools/call` went, for every outcome and not only the
/// ones where it decided anything.
///
/// The gap this closes: ADR-012 decision 4 deliberately allows `violated` to rest on a
/// *failed* invocation, and keeps it. Without the result on the assessment there is nowhere
/// for `VERDICT.invocation_result` to come from, so such a row is byte-identical in storage
/// to a `violated` from a clean successful call — P5-03 cannot triage the disclosure, P5-04
/// cannot report the two populations apart, and a maintainer objecting *"your harness called
/// my tool a violation when the call errored"* cannot be answered from the record.
#[test]
fn every_assessment_records_how_the_invocation_went() {
    let mut seen_violated_from_a_failed_call = 0;
    for declared in EVERY_DECLARED {
        for call in EVERY_INVOCATION_RESULT {
            for cs in [nothing(), wrote_user_state()] {
                let a = assess(declared, call, &cs);
                assert_eq!(a.call(), call, "{declared:?} {call:?}");
                if a.outcome() == Outcome::Violated && !call.is_complete() {
                    seen_violated_from_a_failed_call += 1;
                }
            }
        }
    }
    assert!(
        seen_violated_from_a_failed_call > 0,
        "the disputable row ADR-012 decision 4 allows must actually be reachable, or this \
         test is not pinning the thing that needs pinning"
    );
}

/// Mirrors `probe::protocol`'s own `every_assessment_from_this_module_is_tagged_protocol_probe`.
/// Not a behavioural assertion about `Assessment` (the oracle is not a field on it) — it
/// locks in the structural guarantee: `ORACLE` is a `const`, so it is the same value for
/// every assessment this crate can ever produce, by construction rather than by every call
/// site remembering to set it. ADR-002's no-pooling rule depends on the tag being
/// trustworthy, and B-03 tests the other half.
#[test]
fn every_assessment_from_this_crate_is_tagged_kernel_changeset() {
    assert_eq!(Assessment::ORACLE, Oracle::KernelChangeset);
    assert_ne!(Assessment::ORACLE, Oracle::ProtocolProbe);
}

/// The gate attestation travels with the run, so a verdict can be traced to the gate
/// decision that licensed it.
#[test]
fn a_gated_run_carries_the_attested_run_id() {
    let attestation = FakeGate.attest_gate_passed("run-42");
    let cs = nothing();
    let run = GatedRun::new(
        &attestation,
        Observation::new(InvocationResult::Completed, Ok(&cs)),
    );
    assert_eq!(run.run_id(), "run-42");
    assert_eq!(run.observation().call(), InvocationResult::Completed);
    assert!(run.observation().derived().is_ok());
}

// ---------------------------------------------------------------------------------------
// Hostile input
// ---------------------------------------------------------------------------------------

/// The changeset is derived from a hostile tool's own writes (design.md §3), so every byte
/// of it is attacker-chosen. The engine must be total over anything `normalise` can
/// produce: non-UTF-8 paths, embedded NULs and newlines, megabyte file contents, a path
/// that is nothing but separators, and a partition with 100,000 entries. No panic, and the
/// decision still turns only on whether `user_state` is empty.
#[test]
fn hostile_changeset_content_cannot_panic_or_change_the_decision() {
    let hostile = vec![
        Change { path: vec![0xFF, 0xFE, 0x00, b'\n', 0x80], kind: ChangeKind::Created(node(&[0u8; 1 << 20])) },
        Change { path: b"/".repeat(4096), kind: ChangeKind::Deleted { was: None } },
        Change {
            path: b"/home/u/\xC3\x28".to_vec(),
            kind: ChangeKind::Modified {
                node: node(&[]),
                content_changed: false,
                metadata_changed: false,
            },
        },
    ];
    let mut wide: Vec<Change> = Vec::with_capacity(100_000);
    for i in 0..100_000u32 {
        wide.push(created(&i.to_be_bytes()));
    }

    for user in [hostile.clone(), wide] {
        let count = user.len();
        let cs = CanonicalChangeset {
            // An empty identity is not something the loader can produce, but the engine
            // must not assume that: it is carried, not parsed.
            ruleset_identity: String::new(),
            user_state: user,
            server_internal: hostile.clone(),
            ephemeral: hostile.clone(),
        };
        let a = assess(Declared::Explicit(true), InvocationResult::Completed, &cs);
        assert_eq!(a.outcome(), Outcome::Violated);
        assert_eq!(a.reported().expect("counts").user_state, count);
        assert_eq!(a.ruleset_identity(), Some(""));
    }
}

/// An empty `user_state` partition decides the same way however much hostile content sits
/// in the other two — the laundering direction ADR-011's open questions already flag, kept
/// visible here rather than discovered later: the engine is doing exactly what ADR-008's
/// allowlists tell it to, and the counts it reports are how a reader sees that.
#[test]
fn hostile_content_outside_user_state_still_reads_as_holds_and_is_reported() {
    let cs = changeset(
        vec![],
        vec![created(b"/home/u/.cache/\xFF\xFE")],
        vec![deleted(b"/srv/app.lock"), created(b"/tmp/\x00")],
    );
    let a = assess(Declared::Explicit(true), InvocationResult::Completed, &cs);
    assert_eq!(a.outcome(), Outcome::Holds);
    let counts = a.reported().expect("counts");
    assert_eq!(counts.user_state, 0);
    assert_eq!(counts.server_internal + counts.ephemeral, 3, "all of it is still reported");
}
