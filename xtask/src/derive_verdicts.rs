//! P5-02: a thin CLI wrapper over `orchestrator::derive_all_read_only_hint_verdicts` — the
//! offline batch job itself, and its tests, are the real deliverable
//! (`crates/orchestrator/src/derive.rs`); this module exists so the job can actually be run
//! as "a batch job... not a step in the run loop," the exit criterion's own words, rather
//! than only ever being exercised from inside a test.
//!
//! Takes explicit paths rather than a fixed default location: this codebase does not yet
//! have one canonical, persistent metadata-DB/evidence-store location that accumulates
//! across separate `cargo xtask` invocations — `first-verdict` and `derive-ruleset-v2`, for
//! example, each still use their own ephemeral scratch directory, torn down when the
//! process exits. Consolidating onto one durable location is a real piece of future work
//! (naturally P5-04's "aggregate reporting" territory, which needs the same thing), not
//! silently assumed to already exist here.

use std::path::Path;

const DEFAULT_RULESET_PATH: &str = "rulesets/v1.json";
const DEFAULT_SLOTS: usize = 4;

/// Run the offline `readOnlyHint` derivation batch job.
///
/// `args` (after the `derive-verdicts` subcommand itself): `<db-path> <blob-store-root>
/// [ruleset-path] [slots]`.
///
/// # Errors
/// A malformed argument list, or whatever `orchestrator::derive_all_read_only_hint_verdicts`
/// itself can fail with.
pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let [db_path, blob_store_root, rest @ ..] = args else {
        return Err("usage: derive-verdicts <db-path> <blob-store-root> [ruleset-path] [slots]".into());
    };
    let ruleset_path = rest.first().map_or(DEFAULT_RULESET_PATH, String::as_str);
    let slots = rest
        .get(1)
        .map(|s| s.parse::<usize>())
        .transpose()
        .map_err(|e| format!("slots must be a positive integer: {e}"))?
        .unwrap_or(DEFAULT_SLOTS);

    println!(
        "deriving readOnlyHint verdicts: db={db_path} evidence={blob_store_root} \
         ruleset={ruleset_path} slots={slots}"
    );
    let report = orchestrator::derive_all_read_only_hint_verdicts(
        Path::new(db_path),
        Path::new(blob_store_root),
        Path::new(ruleset_path),
        slots,
    )?;
    println!("derived {} verdicts ({} failed)", report.derived, report.failed);
    Ok(())
}
