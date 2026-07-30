//! P5-04: assemble the published aggregate report — rates by annotation, by containability
//! class, and by oracle (this task's own literal exit criterion), plus the two
//! harness-maturity signals `store::aggregate`'s own doc comment keeps distinct: the
//! `Unverifiable` rate (ADR-004) and the fraction of discovered tool snapshots that never
//! got as far as producing any verdict at all (this task's own checklist item).
//!
//! This module only *assembles* the report from what `store::db`/`store::aggregate` already
//! compute; it holds no aggregation logic of its own, and it re-verifies its own output with
//! `store::aggregate::verify_report_matches_records` before returning it — the same
//! independent guard B-03 built, applied to this task's own real output rather than only to
//! synthetic test data. Genuinely cross-platform (SQLite plus arithmetic, nothing
//! Linux-specific), same rationale as `queue`/`disclosure`.

use std::path::Path;

use serde_json::{json, Value};
use store::aggregate::{self, RateRow, ReportRow, SnapshotCoverage, VerdictSummary};

/// Why building the aggregate report failed.
#[derive(Debug)]
pub enum AggregateReportError {
    /// The metadata DB reported an error.
    Db(rusqlite::Error),
    /// `store::aggregate::verify_report_matches_records` rejected this module's own output
    /// — a bug in this module, never something a caller should see in practice, but
    /// propagated rather than unwrapped so it fails loudly instead of publishing a report
    /// that could not pass its own cross-oracle guard.
    SelfVerificationFailed(aggregate::AggregationError),
}

impl std::fmt::Display for AggregateReportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "metadata DB error: {e}"),
            Self::SelfVerificationFailed(e) => {
                write!(f, "aggregate report failed its own cross-oracle guard: {e}")
            }
        }
    }
}

impl std::error::Error for AggregateReportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(e) => Some(e),
            Self::SelfVerificationFailed(e) => Some(e),
        }
    }
}

impl From<rusqlite::Error> for AggregateReportError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

/// Build the aggregate report over every `VERDICT` row currently in the DB at `db_path`, as
/// a JSON value ready to be written under `results/conformance/`.
///
/// # Errors
/// Whatever `store::db` fails with, or [`AggregateReportError::SelfVerificationFailed`] if
/// this module's own output somehow fails B-03's independent guard (should be unreachable
/// in practice — see this function's own tests).
pub fn build_report(db_path: &Path) -> Result<Value, AggregateReportError> {
    let conn = store::db::open_and_migrate(db_path.to_str().expect("utf8 db path"))?;

    let records: Vec<VerdictSummary> =
        store::db::list_verdicts(&conn)?.into_iter().map(VerdictSummary::from).collect();
    let aggregate_rows = aggregate::aggregate(&records);
    let rate_rows = aggregate::rates(&aggregate_rows);

    let report: Vec<ReportRow> = aggregate::to_report_rows(&aggregate_rows);
    aggregate::verify_report_matches_records(&report, &records)
        .map_err(AggregateReportError::SelfVerificationFailed)?;

    let total_snapshots = store::db::count_tool_snapshots(&conn)?;
    let snapshots_with_a_verdict = store::db::count_tool_snapshots_with_a_verdict(&conn)?;
    let coverage = aggregate::snapshot_coverage(
        usize::try_from(total_snapshots).unwrap_or(0),
        usize::try_from(snapshots_with_a_verdict).unwrap_or(0),
    );

    Ok(json!({
        "total_verdicts": records.len(),
        // ADR-004's own literal metric — see this module's own doc comment for why it is
        // kept distinct from `snapshot_coverage` below rather than folded into one number.
        "unverifiable_rate": aggregate::unverifiable_rate(&records),
        "snapshot_coverage": snapshot_coverage_json(coverage),
        "rates": rate_rows.iter().map(rate_row_json).collect::<Vec<_>>(),
    }))
}

fn snapshot_coverage_json(coverage: SnapshotCoverage) -> Value {
    json!({
        "total_snapshots": coverage.total_snapshots,
        "snapshots_with_a_verdict": coverage.snapshots_with_a_verdict,
        "no_verdict_fraction": coverage.no_verdict_fraction,
    })
}

fn rate_row_json(row: &RateRow) -> Value {
    json!({
        "annotation": row.annotation.as_db_str(),
        "containability_class": row.containability_class.as_db_str(),
        "oracle": row.oracle.as_db_str(),
        "outcome": row.outcome.as_db_str(),
        "count": row.count,
        "bucket_total": row.bucket_total,
        "rate": row.rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_verdict(
        conn: &rusqlite::Connection,
        server_id: &str,
        class: datamodel::ContainabilityClass,
        snapshot_id: &str,
        verdict_id: &str,
        outcome: datamodel::Outcome,
        reason_code: Option<&str>,
    ) {
        store::db::insert_server(
            conn,
            &store::db::ServerRecord {
                server_id,
                source_uri: "stdio://tool",
                containability_class: class,
                spec_revision: "2026-06-18",
            },
        )
        .expect("insert_server");
        store::db::insert_tool_snapshot(
            conn,
            &store::db::ToolSnapshotRecord {
                snapshot_id,
                server_id,
                tool_name: "read_file",
                metadata_pin: "pin-1",
                annotations_raw: "{}",
                readonly_explicit: true,
                destructive_explicit: false,
                idempotent_explicit: false,
                openworld_explicit: false,
                observed_at: "unix:0",
            },
        )
        .expect("insert_tool_snapshot");
        store::db::insert_verdict(
            conn,
            &store::db::VerdictRecord {
                verdict_id,
                snapshot_id,
                annotation: datamodel::Annotation::ReadOnlyHint,
                declared: "true",
                outcome,
                reason_code,
                oracle: datamodel::Oracle::KernelChangeset,
                ruleset_version: None,
                protocol_version: "2026-06-18",
                derived_at: "unix:0",
            },
        )
        .expect("insert_verdict");
    }

    /// P5-04's exit criterion, over a real DB: a report built from two Class A verdicts (one
    /// `Holds`, one `Unverifiable`) and one Class B verdict (`Violated`) correctly separates
    /// rates by class, reports the right `unverifiable_rate`, and passes its own
    /// cross-oracle/class guard.
    #[test]
    fn build_report_separates_rates_by_class_and_reports_unverifiable_rate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("meta.sqlite3");
        let conn = store::db::open_and_migrate(db_path.to_str().unwrap()).expect("open_and_migrate");

        seed_verdict(
            &conn,
            "srv-a1",
            datamodel::ContainabilityClass::A,
            "snap-a1",
            "v-a1",
            datamodel::Outcome::Holds,
            None,
        );
        seed_verdict(
            &conn,
            "srv-a2",
            datamodel::ContainabilityClass::A,
            "snap-a2",
            "v-a2",
            datamodel::Outcome::Unverifiable,
            Some("timeout"),
        );
        seed_verdict(
            &conn,
            "srv-b1",
            datamodel::ContainabilityClass::B,
            "snap-b1",
            "v-b1",
            datamodel::Outcome::Violated,
            None,
        );
        drop(conn);

        let report = build_report(&db_path).expect("build_report");
        assert_eq!(report["total_verdicts"], 3);
        assert!((report["unverifiable_rate"].as_f64().unwrap() - (1.0 / 3.0)).abs() < 1e-9);

        let rates = report["rates"].as_array().expect("rates array");
        let class_a_holds = rates
            .iter()
            .find(|r| r["containability_class"] == "A" && r["outcome"] == "holds")
            .expect("class A holds row");
        assert_eq!(class_a_holds["count"], 1);
        assert_eq!(class_a_holds["bucket_total"], 2, "both class-A verdicts share one bucket");

        let class_b_violated = rates
            .iter()
            .find(|r| r["containability_class"] == "B" && r["outcome"] == "violated")
            .expect("class B violated row");
        assert_eq!(class_b_violated["bucket_total"], 1, "the class-B verdict must not share class A's bucket");
    }

    /// P5-04's snapshot-coverage checklist item: a snapshot discovered but never assessed
    /// must show up in `no_verdict_fraction`, distinct from an assessed-but-`Unverifiable`
    /// one.
    #[test]
    fn build_report_reports_snapshot_coverage_distinctly_from_unverifiable_rate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("meta.sqlite3");
        let conn = store::db::open_and_migrate(db_path.to_str().unwrap()).expect("open_and_migrate");

        seed_verdict(
            &conn,
            "srv-a1",
            datamodel::ContainabilityClass::A,
            "snap-a1",
            "v-a1",
            datamodel::Outcome::Holds,
            None,
        );
        // A second snapshot that was discovered and pinned but never assessed at all —
        // no RUN, no EVIDENCE, no VERDICT.
        store::db::insert_tool_snapshot(
            &conn,
            &store::db::ToolSnapshotRecord {
                snapshot_id: "snap-unassessed",
                server_id: "srv-a1",
                tool_name: "write_file",
                metadata_pin: "pin-2",
                annotations_raw: "{}",
                readonly_explicit: false,
                destructive_explicit: false,
                idempotent_explicit: false,
                openworld_explicit: false,
                observed_at: "unix:0",
            },
        )
        .expect("insert unassessed snapshot");
        drop(conn);

        let report = build_report(&db_path).expect("build_report");
        assert_eq!(report["unverifiable_rate"], 0.0, "the one verdict that exists is Holds, not Unverifiable");
        assert_eq!(report["snapshot_coverage"]["total_snapshots"], 2);
        assert_eq!(report["snapshot_coverage"]["snapshots_with_a_verdict"], 1);
        assert!(
            (report["snapshot_coverage"]["no_verdict_fraction"].as_f64().unwrap() - 0.5).abs() < 1e-9
        );
    }

    #[test]
    fn build_report_over_an_empty_db_reports_zero_rates_with_no_division_by_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("meta.sqlite3");
        store::db::open_and_migrate(db_path.to_str().unwrap()).expect("open_and_migrate");

        let report = build_report(&db_path).expect("build_report");
        assert_eq!(report["total_verdicts"], 0);
        assert_eq!(report["unverifiable_rate"], 0.0);
        assert_eq!(report["rates"].as_array().unwrap().len(), 0);
        assert_eq!(report["snapshot_coverage"]["no_verdict_fraction"], 0.0);
    }
}
