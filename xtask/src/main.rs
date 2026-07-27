//! Workspace tooling. Run via the cargo aliases in `.cargo/config.toml`.

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("purity") => purity(),
        Some(other) => {
            eprintln!("unknown task `{other}`\n\nusage: cargo xtask purity");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("usage: cargo xtask purity");
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
