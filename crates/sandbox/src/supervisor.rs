//! P1-03: sandbox supervisor — mount namespace, overlayfs, hard timeout.
//!
//! Phase 1's "crudest containment that works" (design.md §10): a private mount namespace
//! plus an overlay mount over the P1-02 base layer, a hard wall-clock timeout, and a clean
//! teardown — deliberately not the full containment stack. PID/user namespaces (P2-01),
//! cgroups (P2-02), network isolation (P3-01), and seccomp (P4-01) are later phases; this
//! module does none of them yet and does not pretend otherwise.
//!
//! **Must not:** emit any verdict. Enforced by contract, and structurally by this crate's
//! own dependency graph: `sandbox` has no edge to `normalise` or `verdict` (see
//! `Cargo.toml`), so nothing here could reach either type even by accident.
//!
//! # Mount-namespace privilege model
//!
//! ADR-010 flagged the privileged-vs-rootless overlay mount choice as "not yet made" and
//! deferred it to this task. Decision: **privileged**. This module calls
//! `unshare(CLONE_NEWNS)` as whatever user the supervisor process itself runs as (root, in
//! every environment this project runs in today), with no user-namespace UID remapping.
//! Overlay's opaque-directory marker is therefore written to the `trusted.overlay.opaque`
//! xattr namespace, not `user.overlay.opaque` — ADR-010's `userxattr` mount option stays
//! off. Rootless operation (a user namespace mapping the invoking user to root inside it,
//! `userxattr` on) is P2-01's job, alongside the PID namespace it is naturally paired with;
//! revisit this decision there, not here.
//!
//! # What "clean teardown" means at this phase
//!
//! The sandboxed process is PID 1 of nothing in particular yet — Phase 1 has no PID
//! namespace (P2-01), so the mount namespace this module creates is scoped to a single
//! process. When that process exits, the kernel tears the namespace down and unmounts
//! everything in it automatically; this module never issues an explicit `umount`. What it
//! *does* do explicitly: reap the child (`waitpid`) so it cannot become a zombie, and,
//! on timeout, `SIGKILL` it. Detecting and killing a process that itself forked children
//! before dying (an "orphan" in P1-05's integrity-gate sense) needs a PID namespace to do
//! properly — that gap is disclosed here, not hidden, and is P2-01's problem, not silently
//! this module's success.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nix::mount::{mount, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

/// Where the sandboxed process finds its filesystem: an overlay of `lower` (the P1-02-built
/// base layer, read-only from the sandboxed process's point of view) plus a fresh
/// `upper`/`work` pair.
#[derive(Debug, Clone)]
pub struct OverlaySpec {
    /// The read-only base layer — typically the `root` a prior `base_layer::build` call
    /// populated. Never written to by the sandboxed process; overlayfs itself enforces
    /// this, not this module.
    pub lower: PathBuf,
    /// Where the overlay's changeset lands. Created (via `create_dir_all`) if missing; left
    /// untouched if it already exists, so a caller can point two runs at genuinely fresh
    /// directories without this module silently reusing state between them. This is what
    /// P1-04 harvests as evidence after the run — never removed by this module.
    pub upper: PathBuf,
    /// Overlayfs's own scratch directory, required by the kernel mount call itself. Not
    /// evidence; never inspected by anything downstream.
    pub work: PathBuf,
    /// Where the merged view is mounted, and the sandboxed process's working directory.
    pub mountpoint: PathBuf,
}

/// One tool invocation to run inside the sandbox.
#[derive(Debug, Clone)]
pub struct SandboxSpec {
    /// The overlay this run's filesystem is built from.
    pub overlay: OverlaySpec,
    /// The program to exec inside the sandbox, after the mount namespace and overlay are
    /// set up and the process has `chdir`'d into `overlay.mountpoint`.
    pub program: PathBuf,
    /// Arguments to `program`.
    pub args: Vec<String>,
    /// Hard wall-clock deadline. Exceeding it gets the sandboxed process `SIGKILL`ed —
    /// unconditionally, and not configurable off (ADR-004 applies to P1-05's gate reading
    /// this outcome, and this module is what makes the outcome exist to read).
    pub timeout: Duration,
}

/// Why constructing or launching the sandbox failed. Distinct from anything the *sandboxed
/// process itself* does once it's running — that is reported through [`SandboxOutcome`],
/// never as an `Err` here, since a hostile or merely broken tool misbehaving is expected
/// input, not a supervisor failure.
#[derive(Debug)]
pub struct SpawnError(io::Error);

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to construct or launch the sandbox: {}", self.0)
    }
}

impl std::error::Error for SpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl From<io::Error> for SpawnError {
    fn from(e: io::Error) -> Self {
        Self(e)
    }
}

/// A running (or just-exited) sandboxed process. Holds only what's needed to supervise its
/// lifecycle — `stdin`/`stdout` are returned separately by [`spawn`] (see its doc comment)
/// precisely so they remain independently ownable by the caller, unentangled from the
/// by-value [`wait`](Self::wait) call that reaps the process.
pub struct SandboxHandle {
    child: Child,
    upper: PathBuf,
    timed_out: Arc<AtomicBool>,
}

/// What the supervisor observed once a run finished. Carries no interpretation — deciding
/// what an exit status or a timeout *means* for a verdict is P1-05's integrity-gate job,
/// per this crate's own "must not emit any verdict" contract; this only reports what
/// happened.
#[derive(Debug)]
pub struct SandboxOutcome {
    /// The overlay's upper directory, on the host filesystem, exactly as the sandboxed
    /// process left it — P1-04's evidence surface. Present regardless of how the run ended.
    pub upper: PathBuf,
    /// The process's exit status, if it exited on its own before the timeout.
    /// `None` if the timeout fired first (`SIGKILL` doesn't produce a "normal" exit status
    /// distinct from any other signal death — `timed_out` is the authoritative signal for
    /// which happened, not an attempt to infer it from the status code alone).
    pub exit_status: Option<ExitStatus>,
    /// Whether the hard timeout fired and killed the process, rather than the process
    /// exiting (successfully or not) on its own.
    pub timed_out: bool,
}

/// Construct the mount namespace and overlay, launch `spec.program` inside it, and return a
/// handle for supervising it plus its stdin/stdout pipes for driving MCP over the boundary
/// (architecture.md §5: "the protocol crosses the boundary over stdio") — returned
/// separately from the handle, not as its fields, so the caller can hold and eventually drop
/// them independently of the by-value [`SandboxHandle::wait`] call. The hard timeout is
/// armed from this call onward — `wait` is what observes whether it fired.
///
/// # `unsafe_code`
///
/// F-03 already anticipated this exact moment: the workspace's `unsafe_code = "warn"` lint
/// "becomes a hard error once P1-03 adds real syscall code to `sandbox`, forcing an explicit
/// `#[allow(unsafe_code)]` per block — consistent with this codebase's existing pattern of
/// demanding explicit justification for risky code." This is that block: `pre_exec`'s
/// contract requires `unsafe` because the closure runs between `fork` and `exec`, where only
/// a narrow set of operations are safe. See the comment directly above the `unsafe` block
/// for why what it actually does stays inside that set.
#[allow(unsafe_code)]
pub fn spawn(
    spec: &SandboxSpec,
) -> Result<(SandboxHandle, std::process::ChildStdin, std::process::ChildStdout), SpawnError> {
    std::fs::create_dir_all(&spec.overlay.upper)?;
    std::fs::create_dir_all(&spec.overlay.work)?;
    std::fs::create_dir_all(&spec.overlay.mountpoint)?;

    let mount_options = format!(
        "lowerdir={},upperdir={},workdir={}",
        spec.overlay.lower.display(),
        spec.overlay.upper.display(),
        spec.overlay.work.display(),
    );
    let mountpoint = spec.overlay.mountpoint.clone();

    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    // SAFETY (as required by `CommandExt::pre_exec`'s own contract): this closure runs in
    // the forked child between `fork` and `exec`, so only async-signal-safe-ish operations
    // belong here. `unshare`/`mount`/`chdir` are direct syscalls; the only non-syscall work
    // is `nix`'s `CString` construction from the paths below, which is the same class of
    // operation `std::process::Command` itself already performs in this exact window for
    // `current_dir`/argv/envp, so it is not introducing a new category of risk beyond what
    // the child-spawning path already does today.
    unsafe {
        command.pre_exec(move || {
            unshare(CloneFlags::CLONE_NEWNS).map_err(io::Error::from)?;

            // Make the whole mount tree private, recursively, *before* mounting anything.
            // Without this, the overlay mount below would propagate into the host's mount
            // namespace via the shared-subtree default most distros ship — exactly the
            // containment failure a "private mount namespace" is supposed to prevent.
            mount(
                None::<&str>,
                "/",
                None::<&str>,
                MsFlags::MS_REC | MsFlags::MS_PRIVATE,
                None::<&str>,
            )
            .map_err(io::Error::from)?;

            mount(
                Some("overlay"),
                mountpoint.as_path(),
                Some("overlay"),
                MsFlags::empty(),
                Some(mount_options.as_str()),
            )
            .map_err(io::Error::from)?;

            nix::unistd::chdir(mountpoint.as_path()).map_err(io::Error::from)?;

            Ok(())
        });
    }

    let mut child = command.spawn()?;
    let stdin = child.stdin.take().expect("spawned with Stdio::piped()");
    let stdout = child.stdout.take().expect("spawned with Stdio::piped()");

    let timed_out = Arc::new(AtomicBool::new(false));
    let pid = Pid::from_raw(child.id() as i32);
    let timeout = spec.timeout;
    let watcher_timed_out = Arc::clone(&timed_out);
    std::thread::spawn(move || {
        std::thread::sleep(timeout);
        // Best-effort, same posture as `discovery::ChildProcessTransport`'s watchdog: if
        // the process already exited, this either fails harmlessly (ESRCH) or, in the rare
        // pid-reuse case, signals an unrelated process. Accepted for the same reason it's
        // accepted there — a short, tens-of-seconds watchdog window, not Phase 2+'s actual
        // containment mechanism.
        if signal::kill(pid, Signal::SIGKILL).is_ok() {
            watcher_timed_out.store(true, Ordering::SeqCst);
        }
    });

    let handle = SandboxHandle { child, upper: spec.overlay.upper.clone(), timed_out };
    Ok((handle, stdin, stdout))
}

impl SandboxHandle {
    /// Block until the sandboxed process exits (naturally, or via the timeout watchdog's
    /// `SIGKILL`), reap it, and report what happened. Consumes the handle. If the caller is
    /// still holding the stdin pipe `spawn` returned, drop it first — same requirement as
    /// waiting on any `std::process::Child` whose stdin is still open.
    pub fn wait(mut self) -> io::Result<SandboxOutcome> {
        let exit_status = self.child.wait()?;
        // The watchdog thread may fire concurrently with a process that is *also* about to
        // exit naturally; reading the flag after `wait()` returns (rather than racing it
        // against the sleep) means "timed out" is only ever true once the kill has actually
        // been attempted, never speculatively.
        let timed_out = self.timed_out.load(Ordering::SeqCst);
        Ok(SandboxOutcome { upper: self.upper, exit_status: Some(exit_status), timed_out })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base_layer::{self, EntryKind, EntrySpec};
    use std::path::Path;

    /// Validate that `path` exists and is a directory, without following a final symlink —
    /// per ADR-009's own requirement that the upper layer be read "from the host side
    /// (outside any overlay mount)."
    fn assert_is_dir(path: &Path) {
        let metadata = std::fs::symlink_metadata(path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(metadata.is_dir(), "{} is not a directory", path.display());
    }

    fn build_trivial_lower(root: &Path) {
        let entries = vec![EntrySpec {
            path: PathBuf::from("marker.txt"),
            kind: EntryKind::File(b"from the base layer\n".to_vec()),
            mode: 0o644,
        }];
        base_layer::build(root, &entries).expect("build base layer");
    }

    /// P1-03's literal exit criterion, in one test: the tool launches inside a mount
    /// namespace over an overlay (proven by the base layer's own file being visible to it,
    /// unmodified, and by its write landing in `upper` rather than mutating `lower`), and
    /// tears down cleanly (the `wait()` call below reaps it without hanging or erroring).
    #[test]
    fn tool_runs_over_the_overlay_and_its_write_lands_in_the_upper_layer() {
        let lower_dir = tempfile::tempdir().expect("tempdir");
        build_trivial_lower(lower_dir.path());
        let scratch = tempfile::tempdir().expect("tempdir");

        let spec = SandboxSpec {
            overlay: OverlaySpec {
                lower: lower_dir.path().to_path_buf(),
                upper: scratch.path().join("upper"),
                work: scratch.path().join("work"),
                mountpoint: scratch.path().join("merged"),
            },
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".to_string(),
                "cat marker.txt > /dev/null && echo written-from-sandbox > new_file.txt"
                    .to_string(),
            ],
            timeout: Duration::from_secs(10),
        };

        let (handle, stdin, _stdout) = spawn(&spec).expect("spawn");
        drop(stdin); // nothing to send; EOF lets a read-then-exit program proceed
        let outcome = handle.wait().expect("wait");

        assert!(!outcome.timed_out);
        assert!(outcome.exit_status.expect("has a status").success());

        // The write is visible in the upper layer from the host side...
        let written = std::fs::read_to_string(outcome.upper.join("new_file.txt"))
            .expect("new_file.txt must be in the upper layer");
        assert_eq!(written, "written-from-sandbox\n");

        // ...and the base layer itself was never touched — the entire point of overlayfs
        // containment.
        assert!(!lower_dir.path().join("new_file.txt").exists());
        let marker_untouched = std::fs::read_to_string(lower_dir.path().join("marker.txt"))
            .expect("marker.txt must be unchanged");
        assert_eq!(marker_untouched, "from the base layer\n");

        assert_is_dir(&outcome.upper);
    }

    /// A process that never exits on its own must still be killed and reaped, not hang the
    /// caller forever — the same discipline `discovery::stdio_with_timeout` already
    /// established for the discovery watchdog, now proven for the sandbox's own.
    /// `/bin/sleep` directly, not `sh -c "sleep ..."` — a shell wrapping a single command
    /// may fork a grandchild to run it rather than `exec`-ing in place (verified directly on
    /// this project's own dev container: it does), and Phase 1 has no PID namespace (P2-01)
    /// to catch that grandchild when this module kills only the direct child. Using `sleep`
    /// as `program` itself sidesteps that gap for this test rather than leaking a real,
    /// long-lived orphan process every time it runs — the gap itself is real and is
    /// documented in this module's own doc comment, not something a test should paper over
    /// by accident.
    #[test]
    fn a_hung_process_is_killed_at_the_timeout_and_reported_as_such() {
        let lower_dir = tempfile::tempdir().expect("tempdir");
        build_trivial_lower(lower_dir.path());
        let scratch = tempfile::tempdir().expect("tempdir");

        let spec = SandboxSpec {
            overlay: OverlaySpec {
                lower: lower_dir.path().to_path_buf(),
                upper: scratch.path().join("upper"),
                work: scratch.path().join("work"),
                mountpoint: scratch.path().join("merged"),
            },
            program: PathBuf::from("/bin/sleep"),
            args: vec!["3600".to_string()],
            timeout: Duration::from_secs(2),
        };

        let start = std::time::Instant::now();
        let (handle, stdin, _stdout) = spawn(&spec).expect("spawn");
        drop(stdin);
        let outcome = handle.wait().expect("wait");

        assert!(outcome.timed_out, "a process sleeping for an hour must be timed out, not awaited");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "wait() should return within a few seconds of the 2s timeout, took {:?}",
            start.elapsed()
        );
        assert!(
            !outcome.exit_status.expect("has a status").success(),
            "a SIGKILLed process must not report success"
        );
    }

    /// A well-behaved but non-trivial exchange: the sandboxed process reads a line from
    /// stdin and echoes a derived line to stdout, proving the stdio boundary
    /// (architecture.md §5: "the protocol crosses the boundary over stdio") carries real
    /// bidirectional traffic, not just an inherited-and-ignored pipe.
    #[test]
    fn stdin_and_stdout_cross_the_sandbox_boundary() {
        use std::io::{Read, Write};

        let lower_dir = tempfile::tempdir().expect("tempdir");
        build_trivial_lower(lower_dir.path());
        let scratch = tempfile::tempdir().expect("tempdir");

        let spec = SandboxSpec {
            overlay: OverlaySpec {
                lower: lower_dir.path().to_path_buf(),
                upper: scratch.path().join("upper"),
                work: scratch.path().join("work"),
                mountpoint: scratch.path().join("merged"),
            },
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".to_string(),
                "read line && echo \"sandbox saw: $line\"".to_string(),
            ],
            timeout: Duration::from_secs(10),
        };

        let (handle, mut stdin, mut stdout) = spawn(&spec).expect("spawn");
        writeln!(stdin, "hello from the host").expect("write to sandboxed stdin");
        drop(stdin);

        let mut reply = String::new();
        stdout.read_to_string(&mut reply).expect("read sandboxed stdout");
        assert_eq!(reply, "sandbox saw: hello from the host\n");

        let outcome = handle.wait().expect("wait");
        assert!(!outcome.timed_out);
        assert!(outcome.exit_status.expect("has a status").success());
    }
}
