//! Workspace tooling. Run via the cargo aliases in `.cargo/config.toml`.

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("purity") => purity(),
        Some("census") => census(),
        Some("census-stage1") => census_stage1(),
        Some("census-stage2-class-a") => census_stage2_class_a(),
        Some("census-pin-stability") => census_pin_stability(),
        Some("probe-stage1") => probe_stage1(),
        Some("dump-tools") => dump_tools(),
        Some("first-verdict") => first_verdict(),
        Some("derive-ruleset-v2") => derive_ruleset_v2(),
        Some(other) => {
            eprintln!("unknown task `{other}`\n\n{USAGE}");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "usage: cargo xtask <purity|census|census-stage1 [sample_size]|census-stage2-class-a [sample_size]|census-pin-stability <input.json>|probe-stage1 [sample_size]|dump-tools <url>|first-verdict|derive-ruleset-v2>";

#[cfg(target_os = "linux")]
fn derive_ruleset_v2() -> ExitCode {
    match xtask::ruleset_v2::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("derive-ruleset-v2 failed: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn derive_ruleset_v2() -> ExitCode {
    eprintln!("derive-ruleset-v2 requires Linux (sandbox/observe are Linux-only)");
    ExitCode::FAILURE
}

#[cfg(target_os = "linux")]
fn first_verdict() -> ExitCode {
    match xtask::first_verdict::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("first-verdict failed: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn first_verdict() -> ExitCode {
    eprintln!("first-verdict requires Linux (sandbox/observe are Linux-only)");
    ExitCode::FAILURE
}

fn dump_tools() -> ExitCode {
    let Some(url) = std::env::args().nth(2) else {
        eprintln!("usage: cargo xtask dump-tools <url>");
        return ExitCode::FAILURE;
    };
    match xtask::dump_tools::run(&url) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dump-tools failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn census_pin_stability() -> ExitCode {
    let Some(input_path) = std::env::args().nth(2) else {
        eprintln!("usage: cargo xtask census-pin-stability <path-to-stage1-output.json>");
        return ExitCode::FAILURE;
    };
    match xtask::pin_stability::run(&input_path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("census-pin-stability failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn census() -> ExitCode {
    match xtask::census::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("census failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn census_stage1() -> ExitCode {
    let sample_size: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    match xtask::census_stage1::run(sample_size) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("census-stage1 failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn probe_stage1() -> ExitCode {
    let sample_size: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(100);
    match xtask::probe_stage1::run(sample_size) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("probe-stage1 failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn census_stage2_class_a() -> ExitCode {
    let sample_size: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    match xtask::class_a_stage2::run(sample_size) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("census-stage2-class-a failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn purity() -> ExitCode {
    match xtask::purity::run() {
        Err(e) => {
            eprintln!("purity check could not run: {e}");
            ExitCode::FAILURE
        }
        Ok(v) if v.is_empty() => {
            println!(
                "purity: OK — {:?} depend only on {:?}",
                xtask::purity::PURE_CRATES,
                xtask::purity::PURE_ALLOWLIST
            );
            ExitCode::SUCCESS
        }
        Ok(violations) => {
            eprintln!("purity: FAILED — the ADR-005 firewall is breached\n");
            for v in &violations {
                eprintln!("  {v}");
            }
            eprintln!(
                "\nReproducibility depends on `normalise` and `verdict` being pure functions of\n\
                 (evidence, ruleset). Either drop the dependency, or add it to PURE_ALLOWLIST in\n\
                 xtask/src/purity.rs with an ADR-005 justification."
            );
            ExitCode::FAILURE
        }
    }
}
