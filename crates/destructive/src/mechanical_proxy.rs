//! Q-01: partition a real upper-layer changeset into `deletions ∪ overwrites` versus `pure
//! additions` — a mechanical, structural proxy for "destructive vs. additive," never a
//! semantic judgement of what a tool's mutation *means*. ADR-006's whole reason this track
//! is quarantined: destructive-vs-additive is properly a semantic question (Q-03's model
//! classifier is the piece that actually attempts it), and this module deliberately answers
//! a narrower, purely structural question instead — did this path exist before, and is it
//! gone or changed now, versus genuinely new.
//!
//! **Why this can't be built as an extension of `normalise::normalise`'s own output.**
//! `datamodel::CanonicalChangeset`/`ClassifiedPath` carry only a path and its ADR-008
//! ephemeral/server_internal/user_state taxonomy — deliberately not the entry's original
//! `EntryKind` (whiteout-ness) or anything about the base layer it's being diffed against,
//! since no verification protocol before this one has ever needed either. Reusing that type
//! here would either lose the whiteout signal `normalise` already discards, or force a
//! breaking change onto the one pure crate ADR-005 protects most carefully for a
//! quarantined, out-of-band consumer. This module works from the same two real inputs a
//! real run actually has — the upper layer's own `RawEvidence` (whiteouts still present,
//! exactly as ADR-009 encodes them: a `CharDevice` at `dev_major = 0, dev_minor = 0`) and
//! the base layer's own path set — and reuses `normalise::pattern_matches` (already `pub`,
//! precisely for this kind of external reuse — see its own doc comment) to apply the same
//! ADR-008 ephemeral/server_internal exclusion `readOnlyHint`/`idempotentHint` already
//! restrict themselves to, so this proxy doesn't count a rotated lock file as "destructive."
//!
//! **What this does not attempt:** an opaque directory (`trusted.overlay.opaque`) is ADR-009's
//! other overlayfs-specific signal, alongside a whiteout — but `datamodel::EvidenceEntry`
//! doesn't carry xattrs at all yet (a pre-existing, already-disclosed scope boundary from
//! P1-04, not something this task newly introduces), so an opaque-directory replacement is
//! invisible to this proxy exactly as it is to every other verification protocol today.

use std::collections::BTreeSet;

use datamodel::{EntryKind, RawEvidence, Ruleset};

/// The mechanical (never semantic) classification this proxy assigns one changed path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// A whiteout: the path existed in the base layer and is now gone.
    Deletion,
    /// A regular (non-whiteout) entry whose path also existed in the base layer — its
    /// content has been replaced, not newly created.
    Overwrite,
    /// A regular entry whose path did not exist in the base layer at all.
    Addition,
}

/// One user-state path this proxy classified, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyClassifiedPath {
    /// The raw path bytes, exactly as captured.
    pub path: Vec<u8>,
    /// This proxy's mechanical classification.
    pub change: ChangeKind,
}

/// The exit criterion's own literal two buckets.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MechanicalProxyPartition {
    /// Every `user_state` path classified [`ChangeKind::Deletion`] or
    /// [`ChangeKind::Overwrite`].
    pub deletions_or_overwrites: Vec<ProxyClassifiedPath>,
    /// Every `user_state` path classified [`ChangeKind::Addition`].
    pub pure_additions: Vec<ProxyClassifiedPath>,
}

/// ADR-009: a whiteout is exactly a character-special device at `dev_major = 0, dev_minor =
/// 0` — the same structural fact `observe::evtree`'s own doc comment states, checked here
/// directly against the raw entry kind `normalise::normalise` itself never retains.
fn is_whiteout(entry: &datamodel::EvidenceEntry) -> bool {
    entry.kind == EntryKind::CharDevice && entry.dev_major == 0 && entry.dev_minor == 0
}

/// ADR-008's `user_state` bucket, recomputed independently of `normalise::normalise` (see
/// this module's own doc comment for why) via the same `pattern_matches` function that
/// crate already exposes for exactly this kind of external reuse.
fn is_user_state(path: &[u8], ruleset: &Ruleset) -> bool {
    let is_ephemeral =
        ruleset.ephemeral_globs.iter().any(|pattern| normalise::pattern_matches(pattern, path));
    let is_server_internal = ruleset
        .server_internal_globs
        .iter()
        .any(|pattern| normalise::pattern_matches(pattern, path));
    !is_ephemeral && !is_server_internal
}

/// Partition `upper_evidence`'s `user_state` paths (per `ruleset`, ADR-008) into `deletions ∪
/// overwrites` versus `pure_additions`, using `base_layer_paths` (every path that existed in
/// the sandbox's base layer before the run — see this module's own doc comment for how to
/// get one for real: `observe::evtree::capture` over the base layer root, before it's ever
/// mounted into an overlay) to tell an overwrite apart from a genuinely new path.
///
/// Both output lists are sorted by raw path bytes, the same determinism discipline
/// `normalise::normalise` already follows, for the same reason: two calls over the same
/// input must always agree.
#[must_use]
pub fn partition(
    upper_evidence: &RawEvidence,
    base_layer_paths: &BTreeSet<Vec<u8>>,
    ruleset: &Ruleset,
) -> MechanicalProxyPartition {
    let mut result = MechanicalProxyPartition::default();

    for entry in &upper_evidence.entries {
        if !is_user_state(&entry.path, ruleset) {
            continue;
        }

        let change = if is_whiteout(entry) {
            ChangeKind::Deletion
        } else if base_layer_paths.contains(&entry.path) {
            ChangeKind::Overwrite
        } else {
            ChangeKind::Addition
        };

        let classified = ProxyClassifiedPath { path: entry.path.clone(), change };
        match change {
            ChangeKind::Deletion | ChangeKind::Overwrite => {
                result.deletions_or_overwrites.push(classified);
            }
            ChangeKind::Addition => result.pure_additions.push(classified),
        }
    }

    result.deletions_or_overwrites.sort_by(|a, b| a.path.cmp(&b.path));
    result.pure_additions.sort_by(|a, b| a.path.cmp(&b.path));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use datamodel::EvidenceEntry;

    fn ruleset_v1() -> Ruleset {
        Ruleset::new(
            "v1".to_string(),
            vec!["/tmp/**".to_string(), "**/*.lock".to_string()],
            vec!["**/.cache/**".to_string()],
        )
    }

    fn regular_entry(path: &str) -> EvidenceEntry {
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

    fn whiteout_entry(path: &str) -> EvidenceEntry {
        EvidenceEntry { kind: EntryKind::CharDevice, ..regular_entry(path) }
    }

    fn base_layer(paths: &[&str]) -> BTreeSet<Vec<u8>> {
        paths.iter().map(|p| p.as_bytes().to_vec()).collect()
    }

    /// Q-01's own exit criterion, over every combination at once: a whiteout of a
    /// pre-existing path is a deletion, a regular entry at a pre-existing path is an
    /// overwrite, and a regular entry at a brand-new path is a pure addition.
    #[test]
    fn partitions_deletions_overwrites_and_additions_correctly() {
        let base = base_layer(&["home/user/existing.txt", "home/user/replaced.txt"]);
        let evidence = RawEvidence::new(vec![
            whiteout_entry("home/user/existing.txt"),
            regular_entry("home/user/replaced.txt"),
            regular_entry("home/user/new.txt"),
        ]);

        let result = partition(&evidence, &base, &ruleset_v1());

        assert_eq!(result.deletions_or_overwrites.len(), 2);
        assert_eq!(result.pure_additions.len(), 1);

        let deleted = result
            .deletions_or_overwrites
            .iter()
            .find(|p| p.path == b"home/user/existing.txt")
            .expect("deletion present");
        assert_eq!(deleted.change, ChangeKind::Deletion);

        let overwritten = result
            .deletions_or_overwrites
            .iter()
            .find(|p| p.path == b"home/user/replaced.txt")
            .expect("overwrite present");
        assert_eq!(overwritten.change, ChangeKind::Overwrite);

        assert_eq!(result.pure_additions[0].path, b"home/user/new.txt");
        assert_eq!(result.pure_additions[0].change, ChangeKind::Addition);
    }

    /// ADR-008 exclusion, proven directly: a whiteout or overwrite of an `ephemeral`/
    /// `server_internal` path (a rotated lock file, a cache directory entry) must never
    /// appear in either bucket — this proxy answers a question about the tool's own
    /// behaviour, not about noise intrinsic to process execution.
    #[test]
    fn ephemeral_and_server_internal_paths_are_excluded_entirely() {
        let base = base_layer(&["var/data.lock", "home/user/.cache/tool/x"]);
        let evidence = RawEvidence::new(vec![
            whiteout_entry("var/data.lock"),         // ephemeral deletion
            regular_entry("home/user/.cache/tool/x"), // server_internal overwrite
            regular_entry("tmp/new_scratch.txt"),     // ephemeral addition
        ]);

        let result = partition(&evidence, &base, &ruleset_v1());
        assert!(result.deletions_or_overwrites.is_empty());
        assert!(result.pure_additions.is_empty());
    }

    #[test]
    fn output_is_sorted_by_path_regardless_of_input_order() {
        let base = base_layer(&[]);
        let evidence =
            RawEvidence::new(vec![regular_entry("c"), regular_entry("a"), regular_entry("b")]);
        let result = partition(&evidence, &base, &ruleset_v1());
        let paths: Vec<&[u8]> = result.pure_additions.iter().map(|p| p.path.as_slice()).collect();
        assert_eq!(paths, vec![b"a".as_slice(), b"b".as_slice(), b"c".as_slice()]);
    }

    /// A whiteout for a path the base layer never had (structurally unusual, but not
    /// something this proxy should crash or silently misclassify over) is still a deletion
    /// — the whiteout marker itself is ADR-009's ground truth, not this module's own
    /// bookkeeping of `base_layer_paths`.
    #[test]
    fn a_whiteout_with_no_matching_base_layer_entry_is_still_a_deletion() {
        let base = base_layer(&[]);
        let evidence = RawEvidence::new(vec![whiteout_entry("home/user/mystery.txt")]);
        let result = partition(&evidence, &base, &ruleset_v1());
        assert_eq!(result.deletions_or_overwrites.len(), 1);
        assert_eq!(result.deletions_or_overwrites[0].change, ChangeKind::Deletion);
    }
}
