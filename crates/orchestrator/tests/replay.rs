//! P1-09: prove the full verdict can be regenerated from stored evidence plus a ruleset
//! version, executing no tool — architecture.md §6 invariant 2: *"it is worth an
//! integration test that literally does it."* This is the property that makes ruleset
//! iteration (P2-10) safe: correcting a bad classification must never require re-running a
//! tool.
//!
//! Linux-only (same as `sandbox`/`observe` themselves): `observe::evtree` needs real
//! `lstat`/xattr syscalls with no portable equivalent.
#![cfg(target_os = "linux")]

use std::path::Path;

/// Where a tool invocation's evidence is captured once — built directly with plain
/// `std::fs`, deliberately *not* via `sandbox::spawn`. This step stands in for "a run
/// happened, at some point in the past," which is exactly the boundary this test's replay
/// step must not reach back across.
fn simulate_a_past_runs_upper_layer(root: &Path) {
    std::fs::create_dir(root.join("tmp")).expect("mkdir tmp");
    std::fs::write(root.join("tmp/scratch.lock"), b"pid-file-contents").expect("write ephemeral");
    std::fs::write(root.join("output.txt"), b"a real user-facing write").expect("write user_state");
}

#[test]
fn verdict_replays_from_stored_evidence_without_executing_anything() {
    // --- One-time step: a run happened, and its evidence was harvested and stored. ---
    let upper = tempfile::tempdir().expect("tempdir");
    simulate_a_past_runs_upper_layer(upper.path());

    let capture_bytes = observe::evtree::capture(upper.path()).expect("capture");
    let store_dir = tempfile::tempdir().expect("tempdir");
    let blob_store = store::BlobStore::open(store_dir.path()).expect("open store");
    let digest = blob_store.put(&capture_bytes).expect("put");

    // The source tree is gone. Everything from here on must work from the store alone —
    // no filesystem walk, no subprocess, no MCP server, nothing but `digest` and a ruleset.
    drop(upper);

    let ruleset_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rulesets/v1.json");
    let ruleset = orchestrator::load_ruleset(&ruleset_path).expect("load ruleset");

    let replay = |blob_store: &store::BlobStore| -> verdict::Assessment {
        let stored_bytes = blob_store.get(&digest).expect("get");
        let raw_evidence = observe::evtree::decode(&stored_bytes).expect("decode");
        let changeset = normalise::normalise(&raw_evidence, &ruleset);
        verdict::read_only_hint(true, &changeset)
    };

    let assessment = replay(&blob_store);

    // The real, user-facing write must still surface — this is not a trivial "empty
    // changeset" replay; it proves the taxonomy split (ephemeral `tmp/scratch.lock` vs.
    // `output.txt`) survived the store round trip and still drives a real `Violated`.
    assert_eq!(assessment.outcome(), datamodel::Outcome::Violated);
    assert_eq!(assessment.oracle(), datamodel::Oracle::KernelChangeset);

    // Regenerate a second time, independently, from the same stored digest — the literal
    // exit criterion: this is a pure function of (evidence, ruleset_version), not something
    // that merely happened to work once.
    let assessment_again = replay(&blob_store);
    assert_eq!(assessment, assessment_again);
}

/// The taxonomy split itself must also be a stable function of stored evidence: a `holds`
/// verdict when the only writes are ephemeral, replayed the same way.
#[test]
fn a_purely_ephemeral_changeset_replays_to_holds() {
    let upper = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(upper.path().join("tmp")).expect("mkdir");
    std::fs::write(upper.path().join("tmp/scratch.lock"), b"pid-file-contents").expect("write");

    let capture_bytes = observe::evtree::capture(upper.path()).expect("capture");
    let store_dir = tempfile::tempdir().expect("tempdir");
    let blob_store = store::BlobStore::open(store_dir.path()).expect("open store");
    let digest = blob_store.put(&capture_bytes).expect("put");
    drop(upper);

    let ruleset_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rulesets/v1.json");
    let ruleset = orchestrator::load_ruleset(&ruleset_path).expect("load ruleset");

    let stored_bytes = blob_store.get(&digest).expect("get");
    let raw_evidence = observe::evtree::decode(&stored_bytes).expect("decode");
    let changeset = normalise::normalise(&raw_evidence, &ruleset);
    let assessment = verdict::read_only_hint(true, &changeset);

    assert_eq!(assessment.outcome(), datamodel::Outcome::Holds);
}
