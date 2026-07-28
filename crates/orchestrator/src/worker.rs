//! P4-04: worker re-imaging policy (architecture.md §7 — "Workers are disposable and
//! re-imaged between servers, not between tools. Between-server re-imaging bounds the
//! damage from a successful escape.").
//!
//! # Scope, disclosed honestly
//!
//! Architecture.md §7's own deployment topology is a pool of Linux *hosts*, each re-imaged
//! (rebuilt from a clean base) between servers. That pool doesn't exist in this codebase yet
//! — `orchestrator`'s own crate doc comment already names "run queue, worker pool" as P5-01's
//! placeholder, still ahead. What this module lands instead is the *policy* a real worker-pool
//! scheduler will eventually enforce, made real and testable now, the same "decide the rule
//! before the infrastructure that runs it exists" move this project already made for
//! `integrity::RunSignals::resource_cap_hit`/`escape_class_syscall_denied` (both defined and
//! gated on well before P2-02/P4-01 landed their real producers).
//!
//! Concretely: every existing orchestrator entry point (`arms::one_session`,
//! `network::run_network_isolated_and_bridged`, and so on) already takes a fresh
//! `tempfile::tempdir()` from its own caller for *every single run* — there is no
//! cross-call persistence in this codebase today for a worker to leak *from*. That is
//! stronger than architecture.md §7 actually requires (which allows reuse **across tools of
//! the same server**, for real efficiency reasons — avoiding re-resolving/re-downloading a
//! package for every tool call the way P3-06's own `resolve_entry_point` finding already
//! showed matters), but never weaker than it. [`Worker`] is what a caller *choosing* to
//! reuse a workspace across tool calls needs to reach for, to get that reuse *and* the
//! "wiped between servers" guarantee architecture.md §7 requires of it, in one place.

use std::path::{Path, PathBuf};

/// Why re-imaging a worker's workspace failed.
#[derive(Debug)]
pub struct WorkerError(std::io::Error);

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "worker re-imaging error: {}", self.0)
    }
}

impl std::error::Error for WorkerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl From<std::io::Error> for WorkerError {
    fn from(e: std::io::Error) -> Self {
        Self(e)
    }
}

/// A disposable worker workspace, tracking which server it is currently assigned to.
///
/// A worker's own workspace directory (everything under [`Self::workspace`]) is exactly the
/// state architecture.md §7 is talking about: whatever a real scheduler chooses to let
/// persist across tool calls for efficiency (a resolved package, a warmed cache, ...) lives
/// here, and [`Self::assign_server`] is the one place that decides whether it survives or
/// gets wiped.
pub struct Worker {
    workspace: PathBuf,
    current_server: Option<String>,
}

impl Worker {
    /// A fresh worker with no server assigned yet and an empty workspace at `workspace`
    /// (created if it doesn't already exist).
    ///
    /// # Errors
    /// Creating `workspace` failed.
    pub fn new(workspace: PathBuf) -> Result<Self, WorkerError> {
        std::fs::create_dir_all(&workspace)?;
        Ok(Self { workspace, current_server: None })
    }

    /// The workspace directory this worker's currently-assigned server may use freely —
    /// reused, untouched, across every tool call for that same server; wiped and recreated
    /// empty the moment [`Self::assign_server`] is called with a *different* `server_id`.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The server this worker is currently assigned to, or `None` if [`Self::assign_server`]
    /// has never been called.
    #[must_use]
    pub fn current_server(&self) -> Option<&str> {
        self.current_server.as_deref()
    }

    /// Assign this worker to `server_id`.
    ///
    /// - First assignment, or re-assignment to the **same** server (architecture.md §7:
    ///   "not between tools" — every tool call against one server is exactly this case):
    ///   the workspace is left untouched. This is the reuse the policy explicitly allows.
    /// - Assignment to a **different** server than the one currently held (architecture.md
    ///   §7: "re-imaged between servers"): the workspace is deleted and recreated empty
    ///   *before* this call returns — a real re-image, not a bookkeeping-only server-id
    ///   swap, so nothing the previous server's tool ever wrote (including anything a
    ///   successful escape might have planted) survives into the next server's runs.
    ///
    /// # Errors
    /// Re-imaging the workspace (removing and recreating it) failed.
    pub fn assign_server(&mut self, server_id: &str) -> Result<(), WorkerError> {
        let is_a_different_server = self.current_server.as_deref().is_some_and(|current| current != server_id);
        if is_a_different_server {
            std::fs::remove_dir_all(&self.workspace)?;
            std::fs::create_dir_all(&self.workspace)?;
        }
        self.current_server = Some(server_id.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leave_a_mark(workspace: &Path, name: &str) {
        std::fs::write(workspace.join(name), b"left behind by a tool run").expect("write mark");
    }

    fn marks(workspace: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(workspace)
            .expect("read_dir")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// architecture.md §7's own literal exit criterion, both halves at once: state written
    /// during one server's tool calls survives a same-server re-assignment ("not between
    /// tools") but is gone the moment a *different* server is assigned ("between servers").
    #[test]
    fn workspace_survives_same_server_reassignment_but_is_wiped_on_a_different_server() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut worker = Worker::new(root.path().to_path_buf()).expect("new worker");

        worker.assign_server("server-a").expect("assign server-a");
        leave_a_mark(worker.workspace(), "tool-1-cache.bin");

        // Same server, a second (and third) tool call against it — the workspace this
        // server's tools share must not be touched.
        worker.assign_server("server-a").expect("re-assign server-a");
        assert_eq!(marks(worker.workspace()), vec!["tool-1-cache.bin"]);
        leave_a_mark(worker.workspace(), "tool-2-cache.bin");
        worker.assign_server("server-a").expect("re-assign server-a again");
        assert_eq!(
            marks(worker.workspace()),
            vec!["tool-1-cache.bin", "tool-2-cache.bin"],
            "reassigning the SAME server must never wipe what earlier tool calls left behind"
        );

        // A genuinely different server: the workspace must be re-imaged, not merely
        // relabeled — everything the previous server's tools left behind must be gone.
        worker.assign_server("server-b").expect("assign server-b");
        assert!(
            marks(worker.workspace()).is_empty(),
            "moving to a different server must wipe the workspace, got: {:?}",
            marks(worker.workspace())
        );
        assert_eq!(worker.current_server(), Some("server-b"));
    }

    /// The very first assignment must not require (or attempt) a wipe — there is nothing to
    /// re-image yet, and a worker that has never been assigned should not need special-case
    /// handling from its own caller to avoid one.
    #[test]
    fn the_first_assignment_never_wipes_anything() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut worker = Worker::new(root.path().to_path_buf()).expect("new worker");
        leave_a_mark(worker.workspace(), "pre-existing.bin");

        worker.assign_server("server-a").expect("first assignment");
        assert_eq!(
            marks(worker.workspace()),
            vec!["pre-existing.bin"],
            "the first ever assignment must not wipe a workspace with no prior server"
        );
    }

    /// A worker re-imaged between three different servers in sequence must wipe at every
    /// single transition, not just the first — proving this isn't a one-shot special case.
    #[test]
    fn re_imaging_happens_at_every_server_transition_not_just_the_first() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut worker = Worker::new(root.path().to_path_buf()).expect("new worker");

        for server_id in ["server-a", "server-b", "server-c"] {
            worker.assign_server(server_id).expect("assign");
            assert!(
                marks(worker.workspace()).is_empty(),
                "workspace must be empty on arrival at {server_id}"
            );
            leave_a_mark(worker.workspace(), &format!("{server_id}-mark.bin"));
        }
    }

    #[test]
    fn a_fresh_worker_has_no_current_server() {
        let root = tempfile::tempdir().expect("tempdir");
        let worker = Worker::new(root.path().to_path_buf()).expect("new worker");
        assert_eq!(worker.current_server(), None);
    }
}
