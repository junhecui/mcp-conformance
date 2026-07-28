//! Harvest the overlay upper layer, connection log, seccomp audit log, cgroup counters, and exit status.
//!
//! **Must not:** Interpret anything.
//!
//! Contract: [architecture.md §3.1].
//!
//! P1-04 lands the first evidence surface: the overlay upper layer, via [`evtree`] (the
//! general ADR-009 walker) and [`harvest`] (which stores a capture into F-05's
//! [`store::BlobStore`] and reports exit/timeout information alongside it, uninterpreted).
//! Connection log, seccomp audit log, and cgroup counters are Phase 3/4 evidence surfaces
//! (network namespace and seccomp don't exist yet) and remain unimplemented here.
//!
//! Linux-only, same as `sandbox`: `evtree`'s xattr capture is a direct Linux/glibc syscall
//! pair (`llistxattr`/`lgetxattr`) with no portable equivalent, and there is nothing for this
//! crate to harvest without `sandbox`'s overlay existing in the first place.

#![cfg(target_os = "linux")]

pub mod connection_log;
pub mod evtree;

use std::path::Path;
use std::process::ExitStatus;

use datamodel::Digest;
use store::{BlobStore, StoreError};

/// What P1-05's integrity gate needs to know about one run, with zero interpretation of
/// what any of it means — deciding whether containment held well enough for the evidence to
/// count is that gate's job, not this crate's (architecture.md §3.1: this component's own
/// "must not" is "interpret anything").
#[derive(Debug)]
pub struct RunObservation {
    /// The digest [`store::BlobStore::put`] returned for this run's `evtree1` capture of
    /// the overlay upper layer — `EVIDENCE.blob_ref` for the `kind = "upper_layer"` row
    /// (architecture.md §6).
    pub upper_layer_digest: Digest,
    /// The sandboxed process's exit status, if it exited on its own. Mirrors
    /// `sandbox::SandboxOutcome::exit_status` exactly — this crate takes it as a plain
    /// parameter rather than depending on the `sandbox` crate's types, keeping the two
    /// components' contracts independent the way `discovery` and `probe` already are.
    pub exit_status: Option<ExitStatus>,
    /// Whether the sandbox's hard timeout fired, mirroring
    /// `sandbox::SandboxOutcome::timed_out`.
    pub timed_out: bool,
    /// What could be determined about descendant processes the sandboxed run may have left
    /// behind.
    pub orphan_state: OrphanState,
}

/// Whether a run left behind descendant processes the supervisor didn't account for.
///
/// Originally added in Phase 1 as an honest placeholder: with no PID namespace (P2-01), a
/// killed process's own children couldn't be enumerated at all —
/// `sandbox::supervisor`'s own tests demonstrated the gap directly (a `sh -c` grandchild
/// surviving a `SIGKILL` to its parent shell). P2-01 closed it structurally: the sandboxed
/// process now runs as PID 1 of its own PID namespace, so its death (natural or via
/// `SIGKILL`) makes the kernel unconditionally kill every other process left in that
/// namespace — not merely "checked and found none," but "cannot exist." Both states remain
/// distinguishable in this enum rather than collapsing the older, weaker claim into the
/// newer, stronger one after the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanState {
    /// No mechanism exists to enumerate a run's descendant processes at all, so orphan
    /// status is unknown — not a claim that none exist. What every run reported before
    /// P2-01, and what any future run without a PID namespace would still have to report.
    NotObservableAtThisPhase,
    /// The run executed inside a PID namespace (`sandbox::SandboxOutcome::orphans_impossible`,
    /// P2-01) — orphaned descendants are structurally impossible for this run, guaranteed by
    /// the kernel, not merely unobserved.
    ImpossibleByPidNamespace,
}

/// Why harvesting a run's evidence failed.
#[derive(Debug)]
pub enum HarvestError {
    /// Walking or serialising the upper layer failed.
    Io(std::io::Error),
    /// Storing the capture into the evidence store failed.
    Store(StoreError),
}

impl std::fmt::Display for HarvestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "failed to capture the upper layer: {e}"),
            Self::Store(e) => write!(f, "failed to store the capture: {e}"),
        }
    }
}

impl std::error::Error for HarvestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Store(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for HarvestError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<StoreError> for HarvestError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

/// Capture `upper` (the overlay upper layer, read from the host side) and store it,
/// content-addressed, in `store`. `exit_status`/`timed_out`/`orphans_impossible` pass
/// through verbatim from whatever ran the sandbox (`sandbox::SandboxOutcome`, in practice —
/// pass its own `orphans_impossible` field here) — this function adds no judgment of its
/// own about what they mean, and takes plain parameters rather than depending on the
/// `sandbox` crate's types, keeping the two components' contracts independent.
pub fn harvest(
    upper: &Path,
    exit_status: Option<ExitStatus>,
    timed_out: bool,
    orphans_impossible: bool,
    store: &BlobStore,
) -> Result<RunObservation, HarvestError> {
    let capture_bytes = evtree::capture(upper)?;
    let upper_layer_digest = store.put(&capture_bytes)?;
    let orphan_state = if orphans_impossible {
        OrphanState::ImpossibleByPidNamespace
    } else {
        OrphanState::NotObservableAtThisPhase
    };
    Ok(RunObservation { upper_layer_digest, exit_status, timed_out, orphan_state })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P1-04's literal exit criterion: harvest and store, content-addressed — proven by
    /// storing a capture and reading the identical bytes back out through the digest
    /// `harvest` returned.
    #[test]
    fn harvest_stores_the_capture_and_is_retrievable_by_its_digest() {
        let upper_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(upper_dir.path().join("new_file.txt"), b"written by the tool")
            .expect("write");
        let store_dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::open(store_dir.path()).expect("open store");

        let observation = harvest(upper_dir.path(), None, false, true, &store).expect("harvest");

        let expected_bytes = evtree::capture(upper_dir.path()).expect("capture directly");
        let stored_bytes =
            store.get(&observation.upper_layer_digest).expect("read back from the store");
        assert_eq!(stored_bytes, expected_bytes);
        assert!(!observation.timed_out);
        assert_eq!(observation.orphan_state, OrphanState::ImpossibleByPidNamespace);
    }

    #[test]
    fn harvest_reports_not_observable_when_told_orphans_are_not_impossible() {
        let upper_dir = tempfile::tempdir().expect("tempdir");
        let store_dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::open(store_dir.path()).expect("open store");

        let observation = harvest(upper_dir.path(), None, false, false, &store).expect("harvest");
        assert_eq!(observation.orphan_state, OrphanState::NotObservableAtThisPhase);
    }
}
