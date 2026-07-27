//! F-04's negative test: prove the purity check actually fires.
//!
//! *"A rule nobody checks is a comment"* — and a checker nobody tests is the same thing one
//! level up. These run against synthetic graphs rather than the real workspace, so the
//! proof that the rule bites does not require leaving a broken dependency edge in the tree.
//!
//! The real graph is checked by `cargo purity` as its own CI step. It is deliberately not a
//! test: `cargo tree` inside `cargo test` is a recursive cargo invocation contending for the
//! same package-cache lock.

use xtask::purity::{PURE_ALLOWLIST, Violation, check_closure, parse_tree};

#[test]
fn clean_closure_passes() {
    let closure = vec!["verdict".to_string(), "datamodel".to_string()];
    assert!(check_closure("verdict", &closure, PURE_ALLOWLIST).is_empty());
}

#[test]
fn crate_may_always_depend_on_itself() {
    let closure = vec!["normalise".to_string()];
    assert!(check_closure("normalise", &closure, PURE_ALLOWLIST).is_empty());
}

/// The exact edge F-04 names: `verdict` -> `store`.
#[test]
fn verdict_depending_on_store_is_a_violation() {
    let closure = vec![
        "verdict".to_string(),
        "datamodel".to_string(),
        "store".to_string(),
    ];
    let found = check_closure("verdict", &closure, PURE_ALLOWLIST);
    assert_eq!(
        found,
        vec![Violation {
            pure_crate: "verdict".to_string(),
            offender: "store".to_string(),
        }]
    );
    assert!(found[0].to_string().contains("ADR-005"));
}

/// The allowlist's whole purpose: catching I/O crates nobody thought to name.
#[test]
fn unforeseen_io_crates_are_caught_without_being_enumerated() {
    for offender in ["tokio", "reqwest", "chrono", "rand", "some-future-http-client"] {
        let closure = vec![
            "normalise".to_string(),
            "datamodel".to_string(),
            offender.to_string(),
        ];
        let found = check_closure("normalise", &closure, PURE_ALLOWLIST);
        assert_eq!(found.len(), 1, "{offender} should breach the firewall");
        assert_eq!(found[0].offender, offender);
    }
}

#[test]
fn transitive_offenders_are_caught() {
    // `cargo tree` flattens the closure, so a dependency three hops away is
    // indistinguishable from a direct one. That is the property we want.
    let closure = vec![
        "verdict".to_string(),
        "datamodel".to_string(),
        "serde".to_string(),
        "tokio".to_string(),
    ];
    let found = check_closure("verdict", &closure, PURE_ALLOWLIST);
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].offender, "serde");
    assert_eq!(found[1].offender, "tokio");
}

#[test]
fn parses_cargo_tree_output() {
    let stdout = "\
verdict v0.1.0 (/Users/x/mcpconf/crates/verdict)
datamodel v0.1.0 (/Users/x/mcpconf/crates/datamodel)

[build-dependencies]
cc v1.0.83
datamodel v0.1.0 (*)
";
    assert_eq!(parse_tree(stdout), vec!["cc", "datamodel", "verdict"]);
}
