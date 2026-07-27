//! F-04: assert the ADR-005 purity firewall over the workspace dependency graph.
//!
//! # Why an allowlist, not a denylist
//!
//! tasks.md F-04 phrases the rule as a prohibition: `normalise` and `verdict` *"may not
//! depend on `sandbox`, `observe`, `store`, or any I/O, clock, network, or model
//! dependency."* Implemented literally that is a denylist, and a denylist over a package
//! ecosystem is whack-a-mole — it passes for any I/O crate nobody thought to name.
//!
//! This checks the strictly stronger property: the transitive dependency closure of a pure
//! crate must be a *subset* of an explicit allowlist. Adding a dependency to `normalise` or
//! `verdict` therefore requires editing this file, which is exactly the friction ADR-005
//! wants. The failure mode inverts from "silently permitted" to "refuses to build until
//! someone justifies it."
//!
//! # Why `cargo tree` and not `cargo metadata`
//!
//! `cargo metadata` emits JSON, which would mean a JSON parser in the one tool whose job is
//! to keep dependencies out. `cargo tree --prefix none` emits one package per line and needs
//! nothing. `cargo-deny`'s `[bans]` section was also considered and rejected: it is
//! workspace-wide and cannot express "crate X may not depend on Y while crate Z may" except
//! through an inverted `wrappers` allowlist.

use std::collections::BTreeSet;
use std::process::Command;

/// Crates bound by the ADR-005 purity rule.
pub const PURE_CRATES: &[&str] = &["normalise", "verdict"];

/// The complete set of packages a pure crate may transitively depend on.
///
/// Additions require an ADR-005 justification. The bar: the dependency must be a pure data
/// transformation with no access to a clock, a filesystem, the network, or a model. `serde`
/// core would qualify; `serde_yaml` would not, because parsing belongs to the caller.
pub const PURE_ALLOWLIST: &[&str] = &["datamodel"];

/// A dependency edge that breaches the firewall.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Violation {
    /// The pure crate whose closure was breached.
    pub pure_crate: String,
    /// The disallowed package found in that closure.
    pub offender: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`{}` depends on `{}`, which is not in the pure allowlist (ADR-005)",
            self.pure_crate, self.offender
        )
    }
}

/// Check one pure crate's dependency closure against the allowlist.
///
/// Pure function over an already-collected closure, so the rule is testable against
/// synthetic graphs without breaking the real build. A crate is always allowed to appear in
/// its own closure.
pub fn check_closure(pure_crate: &str, closure: &[String], allowlist: &[&str]) -> Vec<Violation> {
    let permitted: BTreeSet<&str> = allowlist.iter().copied().chain([pure_crate]).collect();
    let mut violations: Vec<Violation> = closure
        .iter()
        .filter(|pkg| !permitted.contains(pkg.as_str()))
        .map(|pkg| Violation {
            pure_crate: pure_crate.to_string(),
            offender: pkg.clone(),
        })
        .collect();
    violations.sort();
    violations.dedup();
    violations
}

/// Collect the transitive dependency closure of a workspace package.
///
/// Includes build-dependencies: a build script can read a clock just as easily as the crate
/// can. Excludes dev-dependencies, which do not ship in the derived artifact.
pub fn dependency_closure(pkg: &str) -> Result<Vec<String>, String> {
    let out = Command::new(env!("CARGO"))
        .args(["tree", "--package", pkg, "--edges", "normal,build", "--prefix", "none"])
        .output()
        .map_err(|e| format!("could not run `cargo tree`: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "`cargo tree -p {pkg}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    Ok(parse_tree(&String::from_utf8_lossy(&out.stdout)))
}

/// Extract package names from `cargo tree --prefix none` output.
///
/// Lines look like `verdict v0.1.0 (/path/to/crate)`, sometimes suffixed `(*)` where cargo
/// has deduplicated a subtree. Section headers such as `[build-dependencies]` are skipped.
pub fn parse_tree(stdout: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('[') {
            continue;
        }
        if let Some(name) = line.split_whitespace().next() {
            seen.insert(name.to_string());
        }
    }
    seen.into_iter().collect()
}

/// Run the check across every pure crate against the real workspace graph.
pub fn run() -> Result<Vec<Violation>, String> {
    let mut violations = Vec::new();
    for pure in PURE_CRATES {
        let closure = dependency_closure(pure)?;
        violations.extend(check_closure(pure, &closure, PURE_ALLOWLIST));
    }
    Ok(violations)
}
