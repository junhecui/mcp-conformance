//! P5-05: a thin CLI wrapper over `orchestrator::build_methodology_report`, writing its
//! output under `results/conformance/` — the exit criterion's own literal destination.
//! Same posture `derive_verdicts.rs`/`aggregate_report.rs` already take: the report logic
//! and its tests are the real deliverable (`crates/orchestrator/src/methodology.rs`); this
//! module exists so it can actually be produced as a real file, not only ever exercised
//! from a test.

use std::path::Path;

const RESULT_PATH: &str = "results/conformance/methodology.json";
const DEFAULT_DESIGN_MD_PATH: &str = "docs/design.md";
const DEFAULT_RULESET_PATH: &str = "rulesets/v1.json";

/// Build the methodology report from `docs/design.md` and `rulesets/v1.json` (or the paths
/// given in `args`, in that order) and write it to [`RESULT_PATH`].
///
/// # Errors
/// Whatever `orchestrator::build_methodology_report` itself can fail with, or an I/O error
/// writing the result file.
pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let design_md_path = args.first().map_or(DEFAULT_DESIGN_MD_PATH, String::as_str);
    let ruleset_path = args.get(1).map_or(DEFAULT_RULESET_PATH, String::as_str);

    let report =
        orchestrator::build_methodology_report(Path::new(design_md_path), Path::new(ruleset_path))?;
    println!(
        "methodology report: ruleset {} ({} limitations published)",
        report["ruleset"]["version"],
        report["limitations"].as_array().map_or(0, Vec::len),
    );

    if let Some(parent) = Path::new(RESULT_PATH).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(RESULT_PATH, serde_json::to_string_pretty(&report)?)?;
    println!("wrote {RESULT_PATH}");
    Ok(())
}
