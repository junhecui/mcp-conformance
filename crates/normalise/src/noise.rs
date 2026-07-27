//! P2-08: the noise floor, `N = D1 Δ D1'` (architecture.md §4.2). Measured empirically from
//! a genuine pair of independent single-call runs of the same tool, never assumed to be
//! empty (ADR-003): "comparing one call against two without first establishing how much two
//! *identical* single-call runs differ is measuring noise plus signal and reporting it as
//! signal."
//!
//! # Scope: path-level symmetric difference
//!
//! `N`'s stated purpose (architecture.md §4.2) is to seed candidate normalisation rules —
//! "every element of `N` is a candidate normalisation rule, and a rule that never appears in
//! any observed `N` should not exist" — and ADR-008's ruleset is itself path-glob-shaped, not
//! content-shaped. [`noise_floor`] therefore computes symmetric difference over *paths*: a
//! path present in exactly one of the two captures. It does not additionally flag a path
//! present in both captures but with different content or metadata — a real, coarser
//! property than a full content-level diff, and the right one for a function whose output
//! feeds path-glob normalisation rules. Content-level comparison across independent runs
//! (`idempotentHint`'s own `D2` vs `D1` check) is a different question with a different
//! consumer (P2-09), not this one.

use alloc::vec::Vec;
use datamodel::RawEvidence;

/// One path where two evidence captures of "the same" run disagreed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoiseFloorEntry {
    /// Present in the first capture only.
    OnlyInFirst(Vec<u8>),
    /// Present in the second capture only.
    OnlyInSecond(Vec<u8>),
}

/// `N = first Δ second`: every path present in exactly one of the two evidence captures,
/// returned sorted by raw path bytes.
///
/// Does not assume its inputs already arrive sorted — `RawEvidence` producers in this
/// codebase always do (the same order ADR-009's wire format itself uses), but this crate's
/// own [`crate::normalise`] already re-sorts its output defensively "regardless of the order
/// entries arrived in," and this function holds itself to the same standard rather than
/// silently depending on a caller's ordering discipline.
#[must_use]
pub fn noise_floor(first: &RawEvidence, second: &RawEvidence) -> Vec<NoiseFloorEntry> {
    let mut a: Vec<&[u8]> = first.entries.iter().map(|e| e.path.as_slice()).collect();
    let mut b: Vec<&[u8]> = second.entries.iter().map(|e| e.path.as_slice()).collect();
    a.sort_unstable();
    b.sort_unstable();

    let mut out = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < a.len() && j < b.len() {
        match a[i].cmp(b[j]) {
            core::cmp::Ordering::Less => {
                out.push(NoiseFloorEntry::OnlyInFirst(a[i].to_vec()));
                i += 1;
            }
            core::cmp::Ordering::Greater => {
                out.push(NoiseFloorEntry::OnlyInSecond(b[j].to_vec()));
                j += 1;
            }
            core::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    while i < a.len() {
        out.push(NoiseFloorEntry::OnlyInFirst(a[i].to_vec()));
        i += 1;
    }
    while j < b.len() {
        out.push(NoiseFloorEntry::OnlyInSecond(b[j].to_vec()));
        j += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use datamodel::{EntryKind, EvidenceEntry};

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

    #[test]
    fn two_identical_captures_have_an_empty_noise_floor() {
        let a = RawEvidence::new(vec![entry("a"), entry("b")]);
        let b = RawEvidence::new(vec![entry("a"), entry("b")]);
        assert_eq!(noise_floor(&a, &b), vec![]);
    }

    #[test]
    fn a_path_only_in_the_first_capture_is_reported_as_such() {
        let a = RawEvidence::new(vec![entry("a"), entry("only-in-a")]);
        let b = RawEvidence::new(vec![entry("a")]);
        assert_eq!(
            noise_floor(&a, &b),
            vec![NoiseFloorEntry::OnlyInFirst(b"only-in-a".to_vec())]
        );
    }

    #[test]
    fn a_path_only_in_the_second_capture_is_reported_as_such() {
        let a = RawEvidence::new(vec![entry("a")]);
        let b = RawEvidence::new(vec![entry("a"), entry("only-in-b")]);
        assert_eq!(
            noise_floor(&a, &b),
            vec![NoiseFloorEntry::OnlyInSecond(b"only-in-b".to_vec())]
        );
    }

    #[test]
    fn differences_on_both_sides_are_both_reported_sorted_by_path() {
        let a = RawEvidence::new(vec![entry("shared"), entry("z-only-a")]);
        let b = RawEvidence::new(vec![entry("shared"), entry("m-only-b")]);
        assert_eq!(
            noise_floor(&a, &b),
            vec![
                NoiseFloorEntry::OnlyInSecond(b"m-only-b".to_vec()),
                NoiseFloorEntry::OnlyInFirst(b"z-only-a".to_vec()),
            ]
        );
    }

    /// This function must not silently depend on its inputs already being sorted — proven
    /// the same way `normalise`'s own `output_is_sorted_by_path_regardless_of_input_order`
    /// test proves it for `normalise` itself.
    #[test]
    fn unsorted_inputs_still_produce_the_correct_noise_floor() {
        let a = RawEvidence::new(vec![entry("z"), entry("a"), entry("m")]);
        let b = RawEvidence::new(vec![entry("m"), entry("z")]);
        assert_eq!(noise_floor(&a, &b), vec![NoiseFloorEntry::OnlyInFirst(b"a".to_vec())]);
    }

    #[test]
    fn two_empty_captures_have_an_empty_noise_floor() {
        let a = RawEvidence::new(vec![]);
        let b = RawEvidence::new(vec![]);
        assert_eq!(noise_floor(&a, &b), vec![]);
    }
}
