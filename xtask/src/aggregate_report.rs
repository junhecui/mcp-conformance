//! P5-04: a thin CLI wrapper over `orchestrator::build_report`, writing its output under
//! `results/conformance/` — the exit criterion's own literal destination. Same posture
//! `derive_verdicts.rs` already takes: the report logic and its tests are the real
//! deliverable (`crates/orchestrator/src/aggregate_report.rs`); this module exists so the
//! report can actually be produced from a real `<db-path>` rather than only ever being
//! exercised from a test.

use std::path::Path;

const RESULT_PATH: &str = "results/conformance/aggregate_report.json";

/// Build the aggregate report for the DB at `args[0]` and write it to [`RESULT_PATH`].
///
/// # Errors
/// A missing argument, whatever `orchestrator::build_report` itself can fail with, or an
/// I/O error writing the result file.
pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let [db_path] = args else {
        return Err("usage: aggregate-report <db-path>".into());
    };

    let report = orchestrator::build_report(Path::new(db_path))?;
    println!(
        "aggregate report: {} verdicts, unverifiable_rate={}, no_verdict_fraction={}",
        report["total_verdicts"],
        report["unverifiable_rate"],
        report["snapshot_coverage"]["no_verdict_fraction"],
    );

    if let Some(parent) = Path::new(RESULT_PATH).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(RESULT_PATH, serde_json::to_string_pretty(&report)?)?;
    println!("wrote {RESULT_PATH}");
    Ok(())
}
