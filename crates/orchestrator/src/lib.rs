//! Run queue, worker pool, scheduling. The one crate permitted to depend on everything.
//!
//! **Must not:** run more than one sandbox per worker slot at a time. Concurrent sandboxes
//! share a kernel and a page cache, and the resulting timing coupling is exactly the noise
//! that P2-08's noise floor is trying to measure. Scale out, not up (architecture.md §7).
//!
//! This is also where ruleset data is loaded and parsed, so that `normalise` can take a
//! parsed [`datamodel::Ruleset`] and stay free of I/O — [`load_ruleset`], brought forward
//! from P5-01's full scope because P1-06 needed something to actually feed `normalise` a
//! real ruleset with, rather than only synthetic in-test data. The queue/worker-pool
//! machinery this crate's doc comment otherwise describes remains P5-01's placeholder.
//!
//! JSON, not YAML: `serde_json` is already a workspace dependency (`discovery`, `probe`);
//! reaching for a YAML crate for one small, already-JSON-shaped file would be a second
//! parser for no benefit ruleset v1's format needs.

use std::fs;
use std::io;
use std::path::Path;

use datamodel::Ruleset;
use serde::Deserialize;

/// P2-07: execute `Arm 1'`, `Arm 2`, and `Arm 2R` against a real sandboxed program. Gated at
/// the module boundary, not the whole crate — `load_ruleset` above is genuinely
/// cross-platform, but everything in `arms` drives `sandbox::spawn` directly.
#[cfg(target_os = "linux")]
mod arms;
#[cfg(target_os = "linux")]
pub use arms::{run_arm_1_prime, run_arm_2, run_arm_2r, ArmError, ArmProgram, ArmRun};

/// P2-08: `N = D1 Δ D1'`, computed per tool, per run, by actually running two independent
/// `Arm 1'`-shaped executions. Same gating rationale as `arms`.
#[cfg(target_os = "linux")]
mod noise;
#[cfg(target_os = "linux")]
pub use noise::measure_noise_floor;

/// P2-09: `idempotentHint`'s multi-arm protocol, driven against real sandboxed runs. Same
/// gating rationale as `arms`.
#[cfg(target_os = "linux")]
mod idempotency;
#[cfg(target_os = "linux")]
pub use idempotency::content_delta;

#[derive(Deserialize)]
struct RulesetFile {
    version: String,
    #[serde(default)]
    ephemeral: Vec<String>,
    #[serde(default)]
    server_internal: Vec<String>,
}

/// Why loading a ruleset file failed.
#[derive(Debug)]
pub enum LoadRulesetError {
    /// The file couldn't be read.
    Io(io::Error),
    /// The file's content wasn't the expected shape.
    Json(serde_json::Error),
}

impl std::fmt::Display for LoadRulesetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "failed to read ruleset file: {e}"),
            Self::Json(e) => write!(f, "ruleset file is not the expected shape: {e}"),
        }
    }
}

impl std::error::Error for LoadRulesetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
        }
    }
}

/// Read and parse a ruleset file (e.g. `rulesets/v1.json`) into a [`datamodel::Ruleset`]
/// `normalise` can consume directly. The only place in this codebase that touches a
/// ruleset file's bytes — `normalise` itself never does (ADR-005).
pub fn load_ruleset(path: &Path) -> Result<Ruleset, LoadRulesetError> {
    let bytes = fs::read(path).map_err(LoadRulesetError::Io)?;
    let file: RulesetFile = serde_json::from_slice(&bytes).map_err(LoadRulesetError::Json)?;
    Ok(Ruleset::new(file.version, file.ephemeral, file.server_internal))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves `rulesets/v1.json` — the actual file `normalise`'s ADR-008 tests exercise
    /// against synthetic rulesets — parses into exactly ADR-008's worked patterns, not just
    /// that *some* file happens to load without erroring.
    #[test]
    fn loads_the_real_ruleset_v1_file_with_adr_008s_exact_patterns() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rulesets/v1.json");
        let ruleset = load_ruleset(&path).expect("load rulesets/v1.json");

        assert_eq!(ruleset.version, "v1");
        assert_eq!(
            ruleset.ephemeral_globs,
            vec!["/tmp/**", "/var/tmp/**", "/run/**", "**/*.lock", "**/*.pid", "**/*.sock"]
        );
        assert_eq!(
            ruleset.server_internal_globs,
            vec![
                "**/.cache/**",
                "**/.config/**",
                "**/.local/state/**",
                "**/__pycache__/**",
                "**/node_modules/.cache/**",
            ]
        );
    }

    #[test]
    fn missing_file_reports_io_error() {
        let err = load_ruleset(Path::new("/nonexistent/ruleset.json")).expect_err("must fail");
        assert!(matches!(err, LoadRulesetError::Io(_)));
    }

    #[test]
    fn malformed_json_reports_json_error() {
        let dir = tempfile_dir();
        let path = dir.join("bad.json");
        fs::write(&path, b"not json").expect("write");
        let err = load_ruleset(&path).expect_err("must fail");
        assert!(matches!(err, LoadRulesetError::Json(_)));
        let _ = fs::remove_file(&path);
    }

    fn tempfile_dir() -> std::path::PathBuf {
        std::env::temp_dir()
    }
}
