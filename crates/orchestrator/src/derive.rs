//! P5-02: an offline batch job that regenerates `VERDICT` rows from stored `EVIDENCE` plus
//! a ruleset — "not a step in the run loop" (the exit criterion's own words). This reads
//! only what the harvesting side of this project already wrote to the metadata DB and the
//! evidence store; it executes no sandboxed tool, and it is safe to re-run at any time —
//! architecture.md §6 invariant 2: "Re-running the normaliser with a new ruleset over
//! historical evidence regenerates the whole verdict table without re-executing a single
//! tool," the property P1-09's replay test already proved for one stored run by hand. This
//! is that same pipeline (`store::BlobStore` → `observe::evtree::decode` →
//! `orchestrator::load_ruleset` → `normalise::normalise` → `verdict::read_only_hint`)
//! driven for real over however many runs the metadata DB actually holds.
//!
//! **Scope actually derived here, disclosed rather than assumed:** `readOnlyHint` only,
//! from each run's `upper_layer` evidence — exactly P1-08/P1-09's own demonstrated single-
//! arm pipeline, now driven from storage instead of from a live run's own return value.
//! `idempotentHint` and `openWorldHint` would need correlating *several* runs' worth of
//! evidence per tool (arms `1'`, `2`, `2R` for the former; the strict and instrumented arms
//! together for the latter) rather than one evidence row at a time — a real extension this
//! task does not build, disclosed here rather than silently left for a reader to discover
//! by noticing what this module doesn't do.
//!
//! Reuses P5-01's [`crate::RunQueue`]/[`crate::WorkerPool`] for real, not merely because
//! the phase ordering says so: each evidence row to (re-)derive becomes one queued job, and
//! [`crate::WorkerPool::drain_all`] processes them across real concurrent worker threads —
//! proving the same queue/pool machinery that drives sandboxed jobs elsewhere in this crate
//! generalises to plain CPU-bound batch work too, where there is no per-slot sandbox to
//! serialise and nothing stops every slot from running flat out.
//!
//! Wired concretely to [`store::BlobStore`] (the "object store" the exit criterion names),
//! not the [`store::object_store::ObjectStore`] trait generically — [`store::HttpObjectStore`]
//! satisfies the same trait and could be substituted, but nothing in this task's own scope
//! needs that generality yet, and adding it now would be exactly the speculative
//! abstraction this codebase otherwise avoids.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use datamodel::{Annotation, Digest, Oracle};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::queue::{QueueError, RunQueue};
use crate::worker_pool::{JobOutcome, WorkerPool};

/// Why offline derivation failed outright — not why one job failed. A single job's failure
/// (its snapshot vanished, its evidence blob can't be read back, ...) is recorded via
/// [`JobOutcome::Failed`] and reported in [`DeriveReport::failed`]; it does not stop the
/// batch.
#[derive(Debug)]
pub enum DeriveError {
    /// The metadata DB reported an error.
    Db(rusqlite::Error),
    /// The run queue reported an error.
    Queue(QueueError),
    /// Loading the ruleset file failed.
    Ruleset(crate::LoadRulesetError),
}

impl std::fmt::Display for DeriveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "metadata DB error: {e}"),
            Self::Queue(e) => write!(f, "run queue error: {e}"),
            Self::Ruleset(e) => write!(f, "ruleset load error: {e}"),
        }
    }
}

impl std::error::Error for DeriveError {}

impl From<rusqlite::Error> for DeriveError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}
impl From<QueueError> for DeriveError {
    fn from(e: QueueError) -> Self {
        Self::Queue(e)
    }
}
impl From<crate::LoadRulesetError> for DeriveError {
    fn from(e: crate::LoadRulesetError) -> Self {
        Self::Ruleset(e)
    }
}

/// How many `readOnlyHint` verdicts this run of the batch job (re-)wrote versus how many
/// evidence rows it found it could not derive a verdict from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeriveReport {
    /// Verdicts successfully (re-)written.
    pub derived: usize,
    /// Evidence rows a job handler could not turn into a verdict — a snapshot that no
    /// longer exists, a blob the store can no longer read back, or a stored digest that
    /// doesn't parse. Recorded, not silently dropped from the count.
    pub failed: usize,
}

#[derive(Serialize, Deserialize)]
struct DeriveJob {
    run_id: String,
    snapshot_id: String,
    digest: String,
}

fn now_iso() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    format!("unix:{secs}")
}

/// Regenerate every `readOnlyHint`/`kernel_changeset` verdict from stored evidence.
///
/// Deletes existing verdicts of exactly that `(annotation, oracle)` pair first (the "safe
/// to truncate" half of architecture.md §6 invariant 2, scoped precisely so a different
/// oracle's verdicts — e.g. Track B's `protocol_probe` results for Class B servers — are
/// never touched), then re-derives one verdict per run that has `upper_layer` evidence,
/// using `ruleset` to normalise. Uses [`WorkerPool::drain_all`] with `slots` worker
/// threads, each opening its own DB connection and [`store::BlobStore`] handle per job —
/// this is CPU/IO-bound batch work with no sandbox to serialise, unlike the sandbox-driving
/// jobs `worker_pool`'s own doc comment discusses.
///
/// # Errors
/// Opening the queue or metadata DB, or the initial verdict-wipe, failing outright. A
/// single job's own failure does not surface here — see [`DeriveReport::failed`].
pub fn derive_all_read_only_hint_verdicts(
    db_path: &Path,
    blob_store_root: &Path,
    ruleset_path: &Path,
    slots: usize,
) -> Result<DeriveReport, DeriveError> {
    let ruleset = Arc::new(crate::load_ruleset(ruleset_path)?);

    {
        let conn = store::db::open_and_migrate(db_path.to_str().expect("utf8 db path"))?;
        // `VERDICT.ruleset_version` is a foreign key into `RULESET` — a verdict naming a
        // version nothing has registered would fail to insert. Idempotent (`INSERT OR
        // IGNORE`): safe to call on every run of this batch job against the same ruleset.
        store::db::insert_ruleset(
            &conn,
            &store::db::RulesetRecord {
                ruleset_version: &ruleset.version,
                rules: &serde_json::json!({
                    "ephemeral": ruleset.ephemeral_globs,
                    "server_internal": ruleset.server_internal_globs,
                })
                .to_string(),
                published_at: &now_iso(),
            },
        )?;
        store::db::delete_verdicts_by_annotation_and_oracle(
            &conn,
            Annotation::ReadOnlyHint,
            Oracle::KernelChangeset,
        )?;
    }

    let queue_db_path = db_path.to_path_buf();
    let evidence = {
        let conn = store::db::open_and_migrate(db_path.to_str().expect("utf8 db path"))?;
        store::db::list_evidence_by_kind(&conn, "upper_layer")?
    };

    let queue = RunQueue::open(&queue_db_path)?;
    for row in &evidence {
        let payload = serde_json::to_string(&DeriveJob {
            run_id: row.run_id.clone(),
            snapshot_id: row.snapshot_id.clone(),
            digest: row.digest.clone(),
        })
        .expect("DeriveJob serialises");
        queue.enqueue(&payload)?;
    }
    drop(queue);

    let failed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let db_path_for_handler = db_path.to_path_buf();
    let blob_store_root = blob_store_root.to_path_buf();
    let failed_for_handler = Arc::clone(&failed);

    let processed = WorkerPool::drain_all(
        &queue_db_path,
        slots,
        Duration::from_secs(60),
        move |payload: &str| {
            if derive_one(&db_path_for_handler, &blob_store_root, &ruleset, payload).is_ok() {
                JobOutcome::Completed
            } else {
                failed_for_handler.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                JobOutcome::Failed
            }
        },
    )?;

    let failed = failed.load(std::sync::atomic::Ordering::SeqCst);
    Ok(DeriveReport { derived: processed - failed, failed })
}

/// Derive and write one verdict from one queued job's payload. `Err(())` rather than a
/// detailed error: a job handler given to [`WorkerPool::drain_all`] can only report
/// [`JobOutcome::Completed`] or [`JobOutcome::Failed`] — the specific reason is not this
/// batch job's concern once it's decided not to retry (a future revision wanting per-job
/// diagnostics would log here, which this task does not need).
fn derive_one(
    db_path: &Path,
    blob_store_root: &Path,
    ruleset: &datamodel::Ruleset,
    payload: &str,
) -> Result<(), ()> {
    let job: DeriveJob = serde_json::from_str(payload).map_err(|_| ())?;

    let conn = store::db::open_and_migrate(db_path.to_str().ok_or(())?).map_err(|_| ())?;
    let snapshot =
        store::db::get_snapshot_for_verdict(&conn, &job.snapshot_id).map_err(|_| ())?.ok_or(())?;

    let annotations: Value = serde_json::from_str(&snapshot.annotations_raw).map_err(|_| ())?;
    // design.md's conservative default table: readOnlyHint defaults to `false` when a tool
    // declares nothing at all.
    let declared_read_only =
        annotations.get("readOnlyHint").and_then(Value::as_bool).unwrap_or(false);

    let digest = Digest::from_hex(&job.digest).ok_or(())?;
    let blob_store = store::BlobStore::open(blob_store_root).map_err(|_| ())?;
    let capture_bytes = blob_store.get(&digest).map_err(|_| ())?;
    let raw_evidence = observe::evtree::decode(&capture_bytes).map_err(|_| ())?;
    let changeset = normalise::normalise(&raw_evidence, ruleset);

    let assessment = verdict::read_only_hint(declared_read_only, &changeset);

    let verdict_id = format!("{}:readOnlyHint:{}", job.snapshot_id, job.run_id);
    store::db::insert_verdict(
        &conn,
        &store::db::VerdictRecord {
            verdict_id: &verdict_id,
            snapshot_id: &job.snapshot_id,
            annotation: Annotation::ReadOnlyHint,
            declared: if declared_read_only { "true" } else { "false" },
            outcome: assessment.outcome(),
            reason_code: assessment.reason().map(|r| r.as_db_str()),
            oracle: assessment.oracle(),
            ruleset_version: Some(&ruleset.version),
            protocol_version: &snapshot.spec_revision,
            derived_at: &now_iso(),
        },
    )
    .map_err(|_| ())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;
    use std::path::PathBuf;

    /// Builds a real directory with plain `std::fs` and captures it with the real
    /// `observe::evtree::capture` walker — the same "a run happened, at some point in the
    /// past" boundary `orchestrator/tests/replay.rs` (P1-09) already established: no
    /// `sandbox::spawn`, no subprocess, nothing this batch job's own pipeline could reach
    /// back across even if it wanted to.
    fn capture_a_past_runs_upper_layer(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let upper = tempfile::tempdir().expect("tempdir");
        for (path, contents) in entries {
            let full = upper.path().join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).expect("mkdir parent");
            }
            std::fs::write(&full, contents).expect("write entry");
        }
        observe::evtree::capture(upper.path()).expect("capture")
    }

    /// Writes a real `server`/`tool_snapshot`/`run`/`evidence` chain by hand, with a real
    /// `evtree1` capture stored in a real `BlobStore` — standing in for "P1-04's harvest
    /// already ran and wrote this," so this test proves the batch job's own
    /// storage-to-verdict pipeline, not a live run.
    fn seed_one_run(
        db_path: &Path,
        blob_store_root: &Path,
        run_id: &str,
        snapshot_id: &str,
        declared_read_only: bool,
        upper_layer_entries: &[(&str, &[u8])],
    ) {
        let conn = store::db::open_and_migrate(db_path.to_str().unwrap()).expect("open_and_migrate");
        store::db::insert_server(
            &conn,
            &store::db::ServerRecord {
                server_id: "srv-1",
                source_uri: "stdio://tool",
                containability_class: datamodel::ContainabilityClass::A,
                spec_revision: "2026-06-18",
            },
        )
        .expect("insert_server");
        store::db::insert_tool_snapshot(
            &conn,
            &store::db::ToolSnapshotRecord {
                snapshot_id,
                server_id: "srv-1",
                tool_name: "read_file",
                metadata_pin: "pin-1",
                annotations_raw: &json!({ "readOnlyHint": declared_read_only }).to_string(),
                readonly_explicit: true,
                destructive_explicit: false,
                idempotent_explicit: false,
                openworld_explicit: false,
                observed_at: "unix:0",
            },
        )
        .expect("insert_tool_snapshot");
        store::db::insert_run(
            &conn,
            &store::db::RunRecord {
                run_id,
                snapshot_id,
                arm: "1",
                fixture_id: None,
                arguments: "{}",
                harness_version: "test",
                started_at: "unix:0",
            },
        )
        .expect("insert_run");

        let capture_bytes = capture_a_past_runs_upper_layer(upper_layer_entries);
        let blob_store = store::BlobStore::open(blob_store_root).expect("open blob store");
        let digest = blob_store.put(&capture_bytes).expect("put evidence");

        store::db::insert_evidence(
            &conn,
            &store::db::EvidenceRecord {
                run_id,
                kind: "upper_layer",
                digest: &digest.to_string(),
                blob_ref: &digest.to_string(),
            },
        )
        .expect("insert_evidence");
    }

    fn ruleset_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rulesets/v1.json")
    }

    /// P5-02's own exit criterion, over a real (though hand-seeded rather than live-run)
    /// evidence chain, executing no tool: a clean run (empty changeset) declaring
    /// `readOnlyHint: true` derives `Holds`, and the resulting `VERDICT` row is read back
    /// from the DB exactly as written, not merely returned in-memory.
    #[test]
    fn a_clean_run_derives_a_holds_verdict_and_it_reads_back_from_the_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("meta.sqlite3");
        let blob_store_root = dir.path().join("evidence");

        seed_one_run(&db_path, &blob_store_root, "run-1", "snap-1", true, &[]);

        let report =
            derive_all_read_only_hint_verdicts(&db_path, &blob_store_root, &ruleset_path(), 2)
                .expect("derive_all_read_only_hint_verdicts");
        assert_eq!(report, DeriveReport { derived: 1, failed: 0 });

        let conn = store::db::open_and_migrate(db_path.to_str().unwrap()).expect("reopen");
        let verdicts = store::db::list_verdicts(&conn).expect("list_verdicts");
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0].annotation, Annotation::ReadOnlyHint);
        assert_eq!(verdicts[0].oracle, Oracle::KernelChangeset);
        assert_eq!(verdicts[0].outcome, datamodel::Outcome::Holds);
    }

    /// A tool that declares `readOnlyHint: true` but whose evidence contains a real
    /// `user_state` write must derive `Violated` — proving this batch job's normalise ->
    /// verdict step actually inspects the stored changeset, not just its presence.
    #[test]
    fn a_run_with_a_real_write_derives_a_violated_verdict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("meta.sqlite3");
        let blob_store_root = dir.path().join("evidence");

        seed_one_run(
            &db_path,
            &blob_store_root,
            "run-1",
            "snap-1",
            true,
            &[("output.txt", b"a real user-facing write")],
        );

        derive_all_read_only_hint_verdicts(&db_path, &blob_store_root, &ruleset_path(), 1)
            .expect("derive_all_read_only_hint_verdicts");

        let conn = store::db::open_and_migrate(db_path.to_str().unwrap()).expect("reopen");
        let verdicts = store::db::list_verdicts(&conn).expect("list_verdicts");
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0].outcome, datamodel::Outcome::Violated);
    }

    /// Two different runs producing byte-identical (here: both empty) evidence must both
    /// derive their own verdict — the direct payoff of P5-02's own `0003_evidence_synthetic_
    /// key.sql` fix: before it, the second run's `EVIDENCE` row could never have been
    /// written at all, so there would be nothing here for this batch job to derive from.
    #[test]
    fn two_runs_sharing_an_evidence_digest_each_derive_their_own_verdict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("meta.sqlite3");
        let blob_store_root = dir.path().join("evidence");

        {
            let conn =
                store::db::open_and_migrate(db_path.to_str().unwrap()).expect("open_and_migrate");
            store::db::insert_server(
                &conn,
                &store::db::ServerRecord {
                    server_id: "srv-1",
                    source_uri: "stdio://tool",
                    containability_class: datamodel::ContainabilityClass::A,
                    spec_revision: "2026-06-18",
                },
            )
            .expect("insert_server");
        }
        // Both runs are clean (empty changeset) against *different* snapshots, so both
        // evidence rows point at the exact same deduplicated blob, and each needs its own
        // verdict written against its own snapshot.
        for (run_id, snapshot_id) in [("run-1", "snap-1"), ("run-2", "snap-2")] {
            let conn =
                store::db::open_and_migrate(db_path.to_str().unwrap()).expect("open_and_migrate");
            store::db::insert_tool_snapshot(
                &conn,
                &store::db::ToolSnapshotRecord {
                    snapshot_id,
                    server_id: "srv-1",
                    tool_name: "read_file",
                    metadata_pin: "pin-1",
                    annotations_raw: &json!({ "readOnlyHint": true }).to_string(),
                    readonly_explicit: true,
                    destructive_explicit: false,
                    idempotent_explicit: false,
                    openworld_explicit: false,
                    observed_at: "unix:0",
                },
            )
            .expect("insert_tool_snapshot");
            store::db::insert_run(
                &conn,
                &store::db::RunRecord {
                    run_id,
                    snapshot_id,
                    arm: "1",
                    fixture_id: None,
                    arguments: "{}",
                    harness_version: "test",
                    started_at: "unix:0",
                },
            )
            .expect("insert_run");

            let capture_bytes = capture_a_past_runs_upper_layer(&[]);
            let blob_store = store::BlobStore::open(&blob_store_root).expect("open blob store");
            let digest = blob_store.put(&capture_bytes).expect("put evidence");
            store::db::insert_evidence(
                &conn,
                &store::db::EvidenceRecord {
                    run_id,
                    kind: "upper_layer",
                    digest: &digest.to_string(),
                    blob_ref: &digest.to_string(),
                },
            )
            .expect("insert_evidence");
        }

        let report =
            derive_all_read_only_hint_verdicts(&db_path, &blob_store_root, &ruleset_path(), 2)
                .expect("derive_all_read_only_hint_verdicts");
        assert_eq!(report, DeriveReport { derived: 2, failed: 0 });

        let conn = store::db::open_and_migrate(db_path.to_str().unwrap()).expect("reopen");
        let verdicts = store::db::list_verdicts(&conn).expect("list_verdicts");
        assert_eq!(verdicts.len(), 2);
    }

    /// Re-running the batch job must not accumulate duplicate verdicts — the "regenerates
    /// the whole verdict table" half of architecture.md §6 invariant 2, proven directly: a
    /// second run over the same evidence replaces the first run's verdict rather than
    /// adding to them.
    #[test]
    fn rerunning_the_batch_job_replaces_rather_than_duplicates_verdicts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("meta.sqlite3");
        let blob_store_root = dir.path().join("evidence");
        seed_one_run(&db_path, &blob_store_root, "run-1", "snap-1", true, &[]);

        derive_all_read_only_hint_verdicts(&db_path, &blob_store_root, &ruleset_path(), 1)
            .expect("first derive");
        derive_all_read_only_hint_verdicts(&db_path, &blob_store_root, &ruleset_path(), 1)
            .expect("second derive over the same evidence");

        let conn = store::db::open_and_migrate(db_path.to_str().unwrap()).expect("reopen");
        let verdicts = store::db::list_verdicts(&conn).expect("list_verdicts");
        assert_eq!(verdicts.len(), 1, "re-deriving must replace, not accumulate");
    }

    /// A snapshot with no explicit `readOnlyHint` key must derive against the spec's
    /// conservative default (`false`, design.md's own table) — proven by seeding a tool
    /// with a real write and no declared annotation at all: `false` promises nothing, so
    /// the run must still `Hold`, never `Violated`.
    #[test]
    fn a_snapshot_with_no_declared_read_only_hint_defaults_to_false() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("meta.sqlite3");
        let blob_store_root = dir.path().join("evidence");

        let conn = store::db::open_and_migrate(db_path.to_str().unwrap()).expect("open_and_migrate");
        store::db::insert_server(
            &conn,
            &store::db::ServerRecord {
                server_id: "srv-1",
                source_uri: "stdio://tool",
                containability_class: datamodel::ContainabilityClass::A,
                spec_revision: "2026-06-18",
            },
        )
        .expect("insert_server");
        store::db::insert_tool_snapshot(
            &conn,
            &store::db::ToolSnapshotRecord {
                snapshot_id: "snap-1",
                server_id: "srv-1",
                tool_name: "write_file",
                metadata_pin: "pin-1",
                annotations_raw: "{}",
                readonly_explicit: false,
                destructive_explicit: false,
                idempotent_explicit: false,
                openworld_explicit: false,
                observed_at: "unix:0",
            },
        )
        .expect("insert_tool_snapshot");
        store::db::insert_run(
            &conn,
            &store::db::RunRecord {
                run_id: "run-1",
                snapshot_id: "snap-1",
                arm: "1",
                fixture_id: None,
                arguments: "{}",
                harness_version: "test",
                started_at: "unix:0",
            },
        )
        .expect("insert_run");
        let capture_bytes = capture_a_past_runs_upper_layer(&[("output.txt", b"a write")]);
        let blob_store = store::BlobStore::open(&blob_store_root).expect("open blob store");
        let digest = blob_store.put(&capture_bytes).expect("put evidence");
        store::db::insert_evidence(
            &conn,
            &store::db::EvidenceRecord {
                run_id: "run-1",
                kind: "upper_layer",
                digest: &digest.to_string(),
                blob_ref: &digest.to_string(),
            },
        )
        .expect("insert_evidence");
        drop(conn);

        derive_all_read_only_hint_verdicts(&db_path, &blob_store_root, &ruleset_path(), 1)
            .expect("derive_all_read_only_hint_verdicts");

        let conn = store::db::open_and_migrate(db_path.to_str().unwrap()).expect("reopen");
        let verdicts = store::db::list_verdicts(&conn).expect("list_verdicts");
        assert_eq!(verdicts.len(), 1);
        assert_eq!(
            verdicts[0].outcome,
            datamodel::Outcome::Holds,
            "a defaulted-false declaration must never be contradicted, real write or not"
        );
    }
}
