//! P1-07, end to end on the std side: stored evidence bytes → `normalise` → `verdict` →
//! a `VERDICT` row → read back.
//!
//! This lives in `store`'s integration tests rather than in `verdict`'s own because it is
//! the only place in the workspace that can hold all four pieces at once. `verdict` and
//! `normalise` are both bound by ADR-005 and may not depend on each other, and neither may
//! touch a filesystem — so the crate that owns the blob store, the metadata DB and the
//! ruleset loader is where the chain they form can actually be exercised against the real
//! published `rulesets/v1.yaml` on disk. (Integration tests are dev-dependencies, which
//! `cargo purity` excludes by policy; nothing here changes either pure crate's dependency
//! closure.)
//!
//! It is **not** P1-09. That task's exit criterion is regenerating the *whole verdict
//! table* from stored evidence as an offline batch job; this is one tool, one arm, and it
//! proves the pieces compose and that re-deriving from the stored bytes reproduces the
//! assessment byte for byte.

use std::path::PathBuf;

use datamodel::{DerivationFailure, GateAttestation, IntegrityGate, Outcome, RawEvidence};
use evtree::{Entry, Payload};
use store::db::{
    self, RulesetRecord, ServerRecord, ToolSnapshotRecord, VerdictProvenance, VerdictRecord,
};
use verdict::{Declared, GatedRun, InvocationResult, Observation, reason, read_only_hint};

/// Stands in for `crates/integrity` (P1-05), which does not exist. Declared here in the
/// open: a `GateAttestation` has no public constructor, and the only way to get one is to
/// claim to be the integrity gate, which is exactly the boundary ADR-004 asks for.
struct FakeGate;
impl IntegrityGate for FakeGate {}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn entry(path: &str, payload: Payload) -> Entry {
    Entry {
        path: path.as_bytes().to_vec(),
        mode: match payload {
            Payload::Directory => 0o040_755,
            _ => 0o100_644,
        },
        uid: 0,
        gid: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        inode: 0,
        dev_major: 0,
        dev_minor: 0,
        xattrs: Vec::new(),
        payload,
    }
}

fn dir(path: &str) -> Entry {
    entry(path, Payload::Directory)
}

fn file(path: &str, content: &str) -> Entry {
    entry(path, Payload::Regular(content.as_bytes().to_vec()))
}

/// A base layer with one user-state file and one cache directory, and an upper layer in
/// which the tool rewrote the user-state file and dropped a lock file. Under ADR-008 only
/// the first of those is decisive.
fn evidence() -> RawEvidence {
    let base = evtree::encode(&[
        dir("home"),
        dir("home/u"),
        file("home/u/doc.txt", "original"),
        dir("home/u/.cache"),
    ]);
    let upper = evtree::encode(&[
        dir("home"),
        dir("home/u"),
        file("home/u/doc.txt", "rewritten by the tool"),
        dir("home/u/.cache"),
        file("home/u/.cache/blob", "warm"),
        file("home/u/run.lock", ""),
    ]);
    RawEvidence { base_layer: base, upper_layer: upper }
}

/// Seed the `SERVER` / `TOOL_SNAPSHOT` / `RUN` chain a `VERDICT` row's foreign keys need.
///
/// `RUN` matters as much as the other two since migration `0002`: `VERDICT.run_id` is the
/// only path from a verdict row to the `EVIDENCE` blobs it was derived from
/// (architecture.md §6's `EVIDENCE ||--o{ VERDICT : supports`).
fn seed(conn: &rusqlite::Connection, run_id: &str, annotations_raw: &str, readonly: bool) {
    db::insert_server(
        conn,
        &ServerRecord {
            server_id: "srv-1",
            source_uri: "stdio://reference-filesystem-server",
            containability_class: datamodel::ContainabilityClass::A,
            spec_revision: "2025-11-25",
        },
    )
    .expect("insert server");
    db::insert_tool_snapshot(
        conn,
        &ToolSnapshotRecord {
            snapshot_id: "snap-1",
            server_id: "srv-1",
            tool_name: "write_file",
            metadata_pin: "0".repeat(64).as_str(),
            annotations_raw,
            readonly_explicit: readonly,
            destructive_explicit: false,
            idempotent_explicit: false,
            openworld_explicit: false,
            observed_at: "2026-10-07T00:00:00Z",
        },
    )
    .expect("insert tool snapshot");
    conn.execute(
        "INSERT INTO run (run_id, snapshot_id, arm, arguments, harness_version, started_at)
         VALUES (?1, 'snap-1', 'arm1', '{}', '0.1.0', '2026-10-07T00:00:00Z')",
        [run_id],
    )
    .expect("insert run");
}

/// The whole chain, against the real published ruleset: a tool declaring
/// `readOnlyHint: true` that rewrites a user-state file is `violated`, the verdict names
/// the ruleset identity and the derivation build, and the row round-trips.
#[test]
fn a_user_state_write_becomes_a_stored_violated_verdict() {
    let ruleset = store::ruleset::load_file(&repo_root().join("rulesets").join("v1.yaml"))
        .expect("load the published ruleset v1");
    let identity = ruleset.identity();
    assert!(identity.starts_with("v1+sha256:"), "{identity}");

    let raw = evidence();
    let changeset = normalise::normalise(&raw, &ruleset).expect("normalise");

    // ADR-008 in action: the rewritten document decides, the cache write and the lock file
    // are reported but cannot.
    assert_eq!(changeset.user_state.len(), 1, "{:?}", changeset.user_state);
    assert_eq!(changeset.user_state[0].path, b"/home/u/doc.txt");
    assert_eq!(changeset.server_internal.len(), 1);
    assert_eq!(changeset.ephemeral.len(), 1);

    let attestation: GateAttestation = FakeGate.attest_gate_passed("run-p1-07-1");
    let run = GatedRun::new(
        &attestation,
        Observation::new(InvocationResult::Completed, Ok(&changeset)),
    );
    let assessment = read_only_hint(Declared::Explicit(true), &run);

    assert_eq!(assessment.outcome(), Outcome::Violated);
    assert!(assessment.reason().is_none());
    let counts = assessment.reported().expect("partition counts");
    assert_eq!(counts.user_state, 1);
    assert_eq!(counts.server_internal, 1);
    assert_eq!(counts.ephemeral, 1);
    assert_eq!(assessment.ruleset_identity(), Some(identity.as_str()));
    assert_eq!(assessment.call(), InvocationResult::Completed);

    // ---- persist ----
    let dir = tempfile::tempdir().expect("tempdir");
    let blobs = store::BlobStore::open(dir.path().join("evidence")).expect("blob store");
    let base_digest = blobs.put(&raw.base_layer).expect("put base");
    let upper_digest = blobs.put(&raw.upper_layer).expect("put upper");

    let conn = db::open_and_migrate(":memory:").expect("open_and_migrate");
    seed(&conn, run.run_id(), r#"{"readOnlyHint":true}"#, true);
    for (kind, digest) in [("overlay_base", base_digest), ("overlay_upper", upper_digest)] {
        conn.execute(
            "INSERT INTO evidence (digest, run_id, kind, blob_ref) VALUES (?1, ?2, ?3, ?1)",
            rusqlite::params![digest.to_string(), run.run_id(), kind],
        )
        .expect("insert evidence row");
    }
    db::insert_ruleset(
        &conn,
        &RulesetRecord {
            ruleset_identity: &identity,
            // The rules themselves, via `store::ruleset::to_json` — not
            // `{"source":"rulesets/v1.yaml"}`, which was the first draft of this line and
            // breaks ADR-005's claim that a reviewer handed the evidence and the ruleset
            // reproduces every verdict: the identity's digest would pin bytes that are
            // nowhere in the bundle, and a filename is not rules.
            rules: &store::ruleset::to_json(&ruleset),
            published_at: "2026-10-07T00:00:00Z",
        },
    )
    .expect("insert ruleset");
    db::insert_verdict(
        &conn,
        &VerdictRecord {
            verdict_id: "v-1",
            snapshot_id: "snap-1",
            annotation: datamodel::Annotation::ReadOnlyHint,
            declared: "true",
            outcome: assessment.outcome(),
            reason_code: assessment.reason().map(|r| r.0.as_str()),
            provenance: VerdictProvenance::KernelChangeset {
                ruleset_identity: &identity,
                derivation_version: db::derivation_version(),
                // The counts the engine reported, stored rather than dropped. Without them
                // this row would be identical in every column to a verdict on a tool that
                // touched nothing, which is what made architecture.md §4.3's promise true of
                // the type and false of the artefact.
                counts: assessment.reported().expect("a derived changeset has counts"),
                call: assessment.call(),
            },
            run_id: Some(run.run_id()),
            protocol_version: "2025-11-25",
            derived_at: "2026-10-07T00:00:01Z",
        },
    )
    .expect("insert verdict");

    let rows = db::list_verdicts(&conn).expect("list_verdicts");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, Outcome::Violated);
    assert_eq!(rows[0].oracle, datamodel::Oracle::KernelChangeset);
    assert_eq!(rows[0].declared, "true");
    assert_eq!(rows[0].ruleset_identity.as_deref(), Some(identity.as_str()));
    assert_eq!(rows[0].derivation_version.as_deref(), Some(db::derivation_version()));
    assert_eq!(rows[0].counts, Some(counts), "the partitions must survive to the row");
    assert_eq!(rows[0].invocation_result, Some(InvocationResult::Completed));
    assert_eq!(rows[0].run_id.as_deref(), Some(run.run_id()));

    // The stored row can name the evidence it rests on, which is what makes re-derivation a
    // check on it rather than an unrelated computation.
    let mut stmt = conn
        .prepare(
            "SELECT e.kind, e.digest FROM verdict v
             JOIN evidence e ON e.run_id = v.run_id
             WHERE v.verdict_id = 'v-1' ORDER BY e.kind",
        )
        .expect("prepare join");
    let named: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect");
    assert_eq!(
        named,
        vec![
            ("overlay_base".to_string(), base_digest.to_string()),
            ("overlay_upper".to_string(), upper_digest.to_string()),
        ]
    );

    // ---- re-derive from the stored bytes alone, executing no tool ----
    let replayed = RawEvidence {
        base_layer: blobs.get(&base_digest).expect("get base"),
        upper_layer: blobs.get(&upper_digest).expect("get upper"),
    };
    let replayed_changeset = normalise::normalise(&replayed, &ruleset).expect("re-normalise");
    assert_eq!(replayed_changeset, changeset, "derivation is a function of the stored bytes");
    let replayed_run = GatedRun::new(
        &attestation,
        Observation::new(InvocationResult::Completed, Ok(&replayed_changeset)),
    );
    assert_eq!(
        read_only_hint(Declared::Explicit(true), &replayed_run),
        assessment,
        "the same evidence and ruleset must reproduce the same assessment exactly"
    );

    // And the counts a reader would argue with are the counts re-derivation produces, so a
    // row whose counts disagree with its evidence is detectable rather than merely unlikely.
    assert_eq!(
        rows[0].counts,
        read_only_hint(Declared::Explicit(true), &replayed_run).reported()
    );
}

/// The same tool, same evidence, but the `tools/call` failed at the tool level. Nothing in
/// the user-state partition is contradicted, so the honest answer is `unverifiable` — the
/// false-`holds` gap commit 832d990 closed in Track B, checked here through the whole
/// std-side chain rather than only in `verdict`'s unit tests.
#[test]
fn a_failed_invocation_over_a_quiet_changeset_is_unverifiable() {
    let ruleset = store::ruleset::load_file(&repo_root().join("rulesets").join("v1.yaml"))
        .expect("load ruleset");
    // A tool that wrote only a lock file: nothing decisive, exactly the shape where
    // "nothing happened" is tempting and wrong.
    let base = evtree::encode(&[dir("home"), dir("home/u")]);
    let upper = evtree::encode(&[dir("home"), dir("home/u"), file("home/u/run.lock", "")]);
    let raw = RawEvidence { base_layer: base, upper_layer: upper };
    let changeset = normalise::normalise(&raw, &ruleset).expect("normalise");
    assert!(changeset.user_state.is_empty());
    assert_eq!(changeset.ephemeral.len(), 1);

    let attestation = FakeGate.attest_gate_passed("run-p1-07-2");
    for call in [InvocationResult::ToolReportedError, InvocationResult::NoResult] {
        let run = GatedRun::new(&attestation, Observation::new(call, Ok(&changeset)));
        let a = read_only_hint(Declared::Explicit(true), &run);
        assert_eq!(a.outcome(), Outcome::Unverifiable, "{call:?}");
        assert_eq!(a.reason().map(|r| r.0.as_str()), Some(reason::INVOCATION_FAILED));
        // The ephemeral write is still reported — the reader can see what did happen.
        assert_eq!(a.reported().expect("counts").ephemeral, 1);
        // And *which* failure it was, so the row is not indistinguishable from the next.
        assert_eq!(a.call(), call);
    }

    // And the completed-invocation counterpart, for contrast: now the absence decides.
    let run = GatedRun::new(
        &attestation,
        Observation::new(InvocationResult::Completed, Ok(&changeset)),
    );
    assert_eq!(read_only_hint(Declared::Explicit(true), &run).outcome(), Outcome::Holds);
}

/// Hostile stored evidence, through the real decoder: every truncation of a valid capture
/// must come back as `unverifiable`/`malformed_evidence` — never `holds`, and never a
/// panic. The tool under test wrote the tree these bytes were captured from (design.md §3),
/// so this is the path an attacker controls end to end.
#[test]
fn hostile_evidence_bytes_can_only_ever_reach_unverifiable() {
    let ruleset = store::ruleset::load_file(&repo_root().join("rulesets").join("v1.yaml"))
        .expect("load ruleset");
    let raw = evidence();
    let attestation = FakeGate.attest_gate_passed("run-p1-07-3");

    let mut rejected = 0;
    for cut in 0..raw.upper_layer.len() {
        let truncated = RawEvidence {
            base_layer: raw.base_layer.clone(),
            upper_layer: raw.upper_layer[..cut].to_vec(),
        };
        let failure = match normalise::normalise(&truncated, &ruleset) {
            Ok(_) => continue,
            Err(e) => DerivationFailure::from(&e),
        };
        assert_eq!(failure, DerivationFailure::MalformedEvidence);
        rejected += 1;
        let run = GatedRun::new(
            &attestation,
            Observation::new(InvocationResult::Completed, Err(failure)),
        );
        let a = read_only_hint(Declared::Explicit(true), &run);
        assert_eq!(a.outcome(), Outcome::Unverifiable, "truncation at {cut}");
        assert_eq!(a.reason().map(|r| r.0.as_str()), Some(reason::MALFORMED_EVIDENCE));
    }
    assert!(rejected > 10, "only {rejected} truncations were rejected — the loop proves little");

    // An unloadable ruleset is one of the *other* derivation failures, and both must stay
    // distinguishable from the above: they are harness faults, not findings about a server.
    let mut broken = ruleset.clone();
    broken.ephemeral.push("not-anchored/**".to_string());
    let failure = DerivationFailure::from(
        &normalise::normalise(&raw, &broken).expect_err("uncompilable pattern"),
    );
    assert_eq!(failure, DerivationFailure::InvalidRuleset);
    let run =
        GatedRun::new(&attestation, Observation::new(InvocationResult::Completed, Err(failure)));
    let a = read_only_hint(Declared::Explicit(true), &run);
    assert_eq!(a.reason().map(|r| r.0.as_str()), Some(reason::INVALID_RULESET));

    // A corrupt **base** layer is a harness fault too: `world::base_layer` builds it and the
    // overlay mounts it read-only, so the tool cannot have produced these bytes. Reported as
    // `malformed_evidence` it would be published as a finding against a server.
    let bad_base = RawEvidence {
        base_layer: b"not an evtree1 capture".to_vec(),
        upper_layer: raw.upper_layer.clone(),
    };
    let failure = DerivationFailure::from(
        &normalise::normalise(&bad_base, &ruleset).expect_err("undecodable base layer"),
    );
    assert_eq!(failure, DerivationFailure::MalformedBaseLayer);
    let run =
        GatedRun::new(&attestation, Observation::new(InvocationResult::Completed, Err(failure)));
    let a = read_only_hint(Declared::Explicit(true), &run);
    assert_eq!(a.outcome(), Outcome::Unverifiable);
    assert_eq!(a.reason().map(|r| r.0.as_str()), Some(reason::MALFORMED_BASE_LAYER));
}

/// ADR-012 decision 5's own row shape, stored: a derivation that failed inside the pure
/// closure produces a verdict, and that verdict has to be writable **without** a ruleset
/// identity and **without** claiming the wrong oracle.
///
/// Before `VerdictProvenance::KernelChangesetDerivationFailed` existed, a driver holding
/// this assessment could only write `protocol_probe` (a false oracle — the exact thing
/// ADR-002 and B-03 exist to prevent), restate the loader's ruleset identity (which ADR-012
/// decision 7 forbids, since the identity comes off a changeset and there is none here), or
/// drop the row and defeat decision 5. This is the end-to-end proof that none of those is
/// necessary any more, through the same chain the happy path uses.
#[test]
fn a_derivation_failure_is_stored_with_a_truthful_oracle_and_no_ruleset() {
    let ruleset = store::ruleset::load_file(&repo_root().join("rulesets").join("v1.yaml"))
        .expect("load ruleset");
    // Hostile bytes in the upper layer: this is the path the tool under test controls.
    let raw = RawEvidence {
        base_layer: evtree::encode(&[dir("home")]),
        upper_layer: b"\x00not evtree1".to_vec(),
    };
    let failure = DerivationFailure::from(
        &normalise::normalise(&raw, &ruleset).expect_err("malformed upper layer"),
    );
    assert_eq!(failure, DerivationFailure::MalformedEvidence);

    let attestation = FakeGate.attest_gate_passed("run-p1-07-4");
    let run = GatedRun::new(
        &attestation,
        Observation::new(InvocationResult::ToolReportedError, Err(failure)),
    );
    let assessment = read_only_hint(Declared::Explicit(true), &run);
    assert_eq!(assessment.outcome(), Outcome::Unverifiable);
    assert!(assessment.reported().is_none(), "no changeset, so nothing to count");
    assert!(assessment.ruleset_identity().is_none(), "and no identity to carry off one");

    let conn = db::open_and_migrate(":memory:").expect("open_and_migrate");
    seed(&conn, run.run_id(), "null", false);
    // Deliberately no `insert_ruleset`: this row must not need one, so the FK cannot be
    // what accepts or rejects it.
    db::insert_verdict(
        &conn,
        &VerdictRecord {
            verdict_id: "v-derivation-failed",
            snapshot_id: "snap-1",
            annotation: datamodel::Annotation::ReadOnlyHint,
            declared: "true",
            outcome: assessment.outcome(),
            reason_code: assessment.reason().map(|r| r.0.as_str()),
            provenance: VerdictProvenance::KernelChangesetDerivationFailed {
                derivation_version: db::derivation_version(),
                call: assessment.call(),
            },
            run_id: Some(run.run_id()),
            protocol_version: "2025-11-25",
            derived_at: "2026-10-07T00:00:01Z",
        },
    )
    .expect("a derivation-failure verdict must be storable");

    let rows = db::list_verdicts(&conn).expect("list_verdicts");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].oracle,
        datamodel::Oracle::KernelChangeset,
        "a derivation failure on a Class A run is not a protocol probe"
    );
    assert_eq!(rows[0].ruleset_identity, None);
    assert_eq!(rows[0].counts, None);
    assert_eq!(rows[0].derivation_version.as_deref(), Some(db::derivation_version()));
    assert_eq!(rows[0].invocation_result, Some(InvocationResult::ToolReportedError));
    assert_eq!(rows[0].run_id.as_deref(), Some(run.run_id()));
}
