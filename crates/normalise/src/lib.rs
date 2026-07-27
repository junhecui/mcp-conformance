//! `(raw_evidence, ruleset) -> canonical_changeset`. Pure and deterministic.
//!
//! **Must not:** read anything outside its inputs — no clock, no filesystem, no network.
//!
//! `no_std` is load-bearing, not stylistic. It is what makes ADR-005 a property of the
//! build rather than a comment: there is no `std::fs` to call because `std` is not linked.
//! F-04's dependency-graph assertion is the outer layer; this is the inner one.
//!
//! P1-06 tested whether that survives contact with real path matching — it does, without
//! reaching for `regex`: [`glob`] is a hand-rolled matcher covering exactly the syntax
//! ADR-008's ruleset v1 patterns use (`**` and a single `*` per segment), kept inside
//! `datamodel`'s `PURE_ALLOWLIST` closure rather than adding a real dependency for eleven
//! fixed patterns. Revisit if ruleset v2 (P2-10) needs syntax this matcher doesn't cover —
//! that would be the deliberate, documented relaxation this doc comment already anticipated.

#![no_std]

extern crate alloc;

mod glob;

use alloc::vec::Vec;

use datamodel::{CanonicalChangeset, ClassifiedPath, PathTaxonomy, RawEvidence, Ruleset};

/// Apply a ruleset to raw evidence, yielding the canonical changeset that every
/// verification protocol is evaluated against.
///
/// The ruleset arrives already parsed. See [`datamodel::Ruleset`] for why. Output is sorted
/// by raw path bytes regardless of the order `evidence.entries` arrived in — this function's
/// own determinism should not depend on a caller happening to preserve ADR-009's capture
/// order.
#[must_use]
pub fn normalise(evidence: &RawEvidence, ruleset: &Ruleset) -> CanonicalChangeset {
    let mut entries: Vec<ClassifiedPath> = evidence
        .entries
        .iter()
        .map(|entry| ClassifiedPath { path: entry.path.clone(), taxonomy: classify(&entry.path, ruleset) })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    CanonicalChangeset::new(entries)
}

/// ADR-008: `ephemeral` checked before `server_internal`; a path matching neither falls
/// through to the conservative default, `user_state`.
fn classify(path: &[u8], ruleset: &Ruleset) -> PathTaxonomy {
    if ruleset.ephemeral_globs.iter().any(|pattern| glob::glob_match(pattern.as_bytes(), path)) {
        return PathTaxonomy::Ephemeral;
    }
    if ruleset
        .server_internal_globs
        .iter()
        .any(|pattern| glob::glob_match(pattern.as_bytes(), path))
    {
        return PathTaxonomy::ServerInternal;
    }
    PathTaxonomy::UserState
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;
    use datamodel::{EntryKind, EvidenceEntry};

    fn ruleset_v1() -> Ruleset {
        Ruleset::new(
            String::from("v1"),
            vec![String::from("/tmp/**"), String::from("**/*.lock")],
            vec![String::from("**/.cache/**")],
        )
    }

    fn entry(path: &str) -> EvidenceEntry {
        EvidenceEntry {
            path: path.as_bytes().to_vec(),
            kind: EntryKind::Regular,
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            inode: 0,
            dev_major: 0,
            dev_minor: 0,
        }
    }

    /// P1-06's literal exit criterion, exercised end to end: a mixed changeset normalises
    /// into all three ADR-008 buckets, correctly and deterministically.
    #[test]
    fn a_mixed_changeset_is_classified_into_all_three_buckets() {
        let evidence = RawEvidence::new(vec![
            entry("home/user/document.txt"), // user_state (default)
            entry("tmp/scratch.txt"),         // ephemeral
            entry("home/user/.cache/tool/x"), // server_internal
            entry("var/data.lock"),           // ephemeral
        ]);

        let changeset = normalise(&evidence, &ruleset_v1());

        let taxonomy_of = |path: &str| {
            changeset
                .entries
                .iter()
                .find(|e| e.path == path.as_bytes())
                .map(|e| e.taxonomy)
                .unwrap_or_else(|| panic!("{path} missing from changeset"))
        };
        assert_eq!(taxonomy_of("home/user/document.txt"), PathTaxonomy::UserState);
        assert_eq!(taxonomy_of("tmp/scratch.txt"), PathTaxonomy::Ephemeral);
        assert_eq!(taxonomy_of("home/user/.cache/tool/x"), PathTaxonomy::ServerInternal);
        assert_eq!(taxonomy_of("var/data.lock"), PathTaxonomy::Ephemeral);
    }

    #[test]
    fn output_is_sorted_by_path_regardless_of_input_order() {
        let evidence = RawEvidence::new(vec![entry("b"), entry("a"), entry("c")]);
        let changeset = normalise(&evidence, &ruleset_v1());
        let paths: Vec<&[u8]> = changeset.entries.iter().map(|e| e.path.as_slice()).collect();
        assert_eq!(paths, vec![b"a".as_slice(), b"b".as_slice(), b"c".as_slice()]);
    }

    /// architecture.md §4.3's own consumer-facing check: `user_state_is_empty` is what
    /// `readOnlyHint`'s verdict is decided against.
    #[test]
    fn user_state_is_empty_ignores_ephemeral_and_server_internal_entries() {
        let all_noise = RawEvidence::new(vec![entry("tmp/scratch.txt"), entry("x/.cache/y")]);
        let changeset = normalise(&all_noise, &ruleset_v1());
        assert!(changeset.user_state_is_empty());

        let with_real_write =
            RawEvidence::new(vec![entry("tmp/scratch.txt"), entry("home/user/output.txt")]);
        let changeset = normalise(&with_real_write, &ruleset_v1());
        assert!(!changeset.user_state_is_empty());
    }

    #[test]
    fn an_empty_ruleset_classifies_everything_as_user_state() {
        let empty_ruleset = Ruleset::new(String::from("empty"), vec![], vec![]);
        let evidence = RawEvidence::new(vec![entry("tmp/scratch.txt")]);
        let changeset = normalise(&evidence, &empty_ruleset);
        assert_eq!(changeset.entries[0].taxonomy, PathTaxonomy::UserState);
    }
}
