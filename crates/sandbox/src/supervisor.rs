//! P1-03 + P2-01 + P3-01: sandbox supervisor — mount, user, PID, and (optional) network
//! namespaces, overlayfs, hard timeout, PID-1 containment.
//!
//! Phase 1 landed "crudest containment that works" (design.md §10): a private mount
//! namespace plus an overlay mount, a hard wall-clock timeout, and a clean teardown, with
//! PID/user namespaces explicitly deferred. This module now also does what P2-01 asks:
//! the sandboxed process becomes PID 1 of its own PID namespace, so when it dies —
//! naturally or via this module's own `SIGKILL` — the kernel automatically kills every
//! descendant it ever spawned and tears the whole namespace down. Cgroups (P2-02) live in
//! their own module (`cgroup`), applied by whoever composes a run, not this one.
//!
//! # Network isolation (P3-01) is opt-in, not universal
//!
//! [`SandboxSpec::network_isolated`] unshares `CLONE_NEWNET` when `true`: a fresh network
//! namespace with no interfaces configured at all (not even loopback — see that field's own
//! doc comment for why), so there is no route to anywhere, including back out to the host.
//! Verified directly, not assumed: a raw `connect()` to an external address from inside such
//! a namespace fails in low single-digit milliseconds with `ENETUNREACH`, and `getaddrinfo`
//! fails just as fast with "temporary failure in name resolution" — genuine "no route out,"
//! not a slow-path timeout dressed up as one.
//!
//! **Deliberately opt-in, not applied to every `spawn()` call**, because of a second,
//! equally real finding: `npx -y <package> ...` — the exact invocation P1-08's and P2-10's
//! own real-corpus measurements already depend on — does not fail fast under this isolation.
//! Verified directly: with network entirely unreachable, `npx` hangs past a 15-second
//! timeout before this module's own hard-timeout watchdog would even fire on a real run,
//! almost certainly because its own registry freshness check (performed even against an
//! already-cached package, unless run with `--offline`) retries with backoff rather than
//! surfacing the same fast, unambiguous failure a raw socket call gets. Every existing
//! caller of `spawn()` sets `network_isolated: false`, preserving exactly the behaviour
//! those already-shipped measurements depend on; only a caller that actually wants P3-01's
//! containment property opts in, and accepts that an `npx`-resolved target needs pre-cached,
//! directly-invoked resolution (not `npx` itself) to run under it at all — a real
//! architectural constraint for whoever wires this into the measurement pipeline next
//! (P3-02 onward), not fixed by this module.
//!
//! # Seccomp-bpf (P4-01)
//!
//! Every real target gets a seccomp-bpf filter denying a fixed, disclosed set of
//! escape-class syscalls (`mount`, `ptrace`, `bpf`, `kexec_load`, and similar — see
//! `seccomp`'s own module doc comment for the full list and the reasoning behind each
//! category), installed in the second fork's child, immediately before its `execvp` so it
//! persists across exec by kernel design. Unconditional, unlike `network_isolated`: nothing
//! in this list is a syscall a legitimate MCP tool has a reason to call, so there is no
//! compatibility trade-off requiring an opt-out the way network isolation needed one.
//!

//! **Must not:** emit any verdict. Enforced by contract, and structurally by this crate's
//! own dependency graph: `sandbox` has no edge to `normalise` or `verdict` (see
//! `Cargo.toml`), so nothing here could reach either type even by accident.
//!
//! # Why this isn't built on `std::process::Command` anymore
//!
//! P1-03 used `Command::pre_exec` — simple, and correct for a mount-namespace-only sandbox.
//! It cannot express what P2-01 needs: `unshare(CLONE_NEWPID)` does **not** move the calling
//! process into the new PID namespace — only that process's *future children* join it, and
//! the first one becomes PID 1. A single `pre_exec` closure that unshares and then lets
//! `Command` exec in the same (unmoved) process would put the real target in the *old* PID
//! namespace, achieving nothing. The actual target program must be a **second** fork, born
//! after the `unshare` call — a shape `pre_exec`'s "one fork, one eventual exec" model
//! cannot express safely. This module forks and `execvp`s directly instead, with its own
//! pipe-based error channel mirroring what `std::process::Command` does internally for the
//! same purpose.
//!
//! # User-namespace UID/GID mapping — the real behavior, and the honest limitation found
//!
//! ADR-010 deferred the privileged-vs-rootless choice to this task. Intended behavior:
//! remap the sandboxed process's namespace-root to the unprivileged `nobody`/`nogroup`
//! identity (uid/gid 65534 — standard on every mainstream Linux distribution, not a
//! project-specific account), so that even though the supervisor itself runs as real root,
//! the sandboxed process is tagged as an unprivileged user from the host's point of view —
//! genuine defense in depth against a mount-namespace or kernel-level escape.
//!
//! **Verified empirically, not assumed, that this doesn't work in every environment this
//! project runs in.** In this project's own current dev/CI container, writing a non-identity
//! `/proc/self/uid_map` entry (`"0 65534 1"`) fails with `EPERM`, even as real root with
//! `CAP_SETUID` present in the bounding set — some outer confinement layer (this container
//! is itself a Firecracker microVM, per its own `process_api --firecracker-init`) restricts
//! non-identity UID remapping specifically, while leaving `CLONE_NEWUSER`/`CLONE_NEWPID`
//! creation, and an *identity* mapping (`"0 0 1"`), fully working. Confirmed directly with a
//! standalone test program before writing this module's real code, not inferred from a
//! single failure. This module therefore **attempts the `nobody`/`nogroup` remap first and
//! falls back to an identity mapping on `EPERM` specifically** (any other error still fails
//! the spawn loudly) — real defense-in-depth where the host allows it, a disclosed,
//! narrower guarantee where it doesn't, never a silent downgrade with no record of which
//! happened.
//!
//! # What "clean teardown" means now
//!
//! Unlike Phase 1 (single process, no PID namespace, so a process that itself forked
//! children before dying could leave real orphans — demonstrated directly by this module's
//! own earlier tests), the sandboxed process is now genuinely PID 1 of its own PID
//! namespace. When PID 1 exits or is `SIGKILL`ed, the kernel unconditionally kills every
//! other process in that namespace and tears it down — there is no configuration surface
//! that could leave a survivor. `orphans_detected` on [`SandboxOutcome`] records this
//! guarantee explicitly rather than leaving a caller to assume it.

use std::ffi::CString;
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nix::fcntl::OFlag;
use nix::mount::{mount, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{fork, ForkResult, Pid};

/// The unprivileged identity the sandboxed process's namespace-root maps to when the host
/// allows non-identity remapping. Standard `nobody`/`nogroup`.
const MAPPED_UID: libc::uid_t = 65534;
const MAPPED_GID: libc::gid_t = 65534;

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
    /// P3-01: whether the sandboxed process runs in its own network namespace with no
    /// interfaces configured at all — not even loopback, since nothing this module's own
    /// tests or any real corpus tool measured so far has needed it, and hand-rolling the
    /// `ioctl` to bring an interface up (the mainline `libc` crate does not expose
    /// `ifreq`/`SIOCSIFFLAGS` for generic Linux) is not worth doing speculatively. See this
    /// module's own doc comment for why this defaults to `false` at every existing call
    /// site rather than being applied universally.
    pub network_isolated: bool,
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
///
/// `init_pid` is the outer, "init" process this module itself forks (see the module doc
/// comment on the double-fork this crate uses) — from the host's point of view it is an
/// ordinary process, PID-namespaced the same as everything else it supervises; it is not
/// the PID-1-inside-the-namespace process the sandboxed program actually runs as.
pub struct SandboxHandle {
    init_pid: Pid,
    target_pid: Pid,
    upper: PathBuf,
    timed_out: Arc<AtomicBool>,
    network_isolated: bool,
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
    /// Always `true`: the sandboxed process runs as PID 1 of its own PID namespace (P2-01),
    /// so orphaned descendants are structurally impossible, not merely undetected — the
    /// kernel kills every remaining process in the namespace the moment PID 1 dies. Kept as
    /// an explicit field (not silently assumed) so a caller composing `integrity::RunSignals`
    /// has an actual value to point at for `containment_uncertain`, the same way
    /// `observe::OrphanState` made Phase 1's weaker, honest "not observable" claim a value
    /// rather than an assumption.
    pub orphans_impossible: bool,
    /// Echoes `SandboxSpec::network_isolated` — kept on the outcome, not just the spec, so a
    /// caller composing evidence has a structural record of whether "no route out" actually
    /// applied to this run, the same "record the guarantee as a value" discipline
    /// `orphans_impossible` already established for PID-namespace containment.
    pub network_isolated: bool,
}

/// Construct the mount, user, and PID namespaces plus the overlay, launch `spec.program`
/// inside them as PID 1 of its own PID namespace, and return a handle for supervising it
/// plus its stdin/stdout pipes for driving MCP over the boundary (architecture.md §5: "the
/// protocol crosses the boundary over stdio"). The hard timeout is armed from this call
/// onward — `wait` is what observes whether it fired.
///
/// # `unsafe_code`
///
/// F-03 already anticipated this exact moment: the workspace's `unsafe_code = "warn"` lint
/// "becomes a hard error once P1-03 adds real syscall code to `sandbox`, forcing an explicit
/// `#[allow(unsafe_code)]` per block — consistent with this codebase's existing pattern of
/// demanding explicit justification for risky code." `fork()` itself is `unsafe` (the
/// module doc comment explains why this module calls it directly rather than going through
/// `std::process::Command`); everything after it, until the exec that replaces the child's
/// process image, is held to the same "only narrow, well-understood operations" discipline
/// `Command::pre_exec`'s own safety contract already demanded in P1-03.
///
/// # Errors
///
/// Returns [`SpawnError`] if creating the overlay directories or namespaces, mounting the
/// overlay, or launching `spec.program` inside it fails.
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

    let program = cstring_from_path(&spec.program)?;
    let mut argv = Vec::with_capacity(spec.args.len() + 1);
    argv.push(program.clone());
    for arg in &spec.args {
        argv.push(CString::new(arg.as_bytes())
            .map_err(|e| SpawnError(io::Error::new(io::ErrorKind::InvalidInput, e)))?);
    }

    let (stdin_read, stdin_write) = nix::unistd::pipe().map_err(nix_err)?;
    let (stdout_read, stdout_write) = nix::unistd::pipe().map_err(nix_err)?;
    // `O_CLOEXEC` on the write end: it closes automatically the moment the child's `execvp`
    // succeeds, which is exactly the signal the parent's read loop below needs — bytes on
    // this pipe mean "setup failed before exec," EOF with no bytes means "exec happened."
    let (err_read, err_write) = nix::unistd::pipe2(OFlag::O_CLOEXEC).map_err(nix_err)?;
    // Carries the *real target's* PID back to this process — see this function's own doc
    // comment on why the timeout watchdog must kill that PID, not `init_pid`.
    let (pid_read, pid_write) = nix::unistd::pipe2(OFlag::O_CLOEXEC).map_err(nix_err)?;

    // SAFETY: forking a single-threaded-at-this-point call site is always safe; everything
    // the child branch does below is restricted to direct syscalls and the same class of
    // simple, non-allocating-where-it-matters setup `Command::pre_exec` already did in
    // P1-03, per this function's own doc comment.
    match unsafe { fork() }.map_err(nix_err)? {
        ForkResult::Parent { child: init_pid } => {
            drop(stdin_read);
            drop(stdout_write);
            drop(err_write);
            drop(pid_write);

            let mut err_reader = std::fs::File::from(err_read);
            let mut err_message = Vec::new();
            err_reader.read_to_end(&mut err_message)?;
            if !err_message.is_empty() {
                return Err(SpawnError(io::Error::other(String::from_utf8_lossy(&err_message).into_owned())));
            }

            let mut pid_reader = std::fs::File::from(pid_read);
            let mut pid_bytes = [0u8; 4];
            pid_reader.read_exact(&mut pid_bytes).map_err(|e| {
                SpawnError(io::Error::other(format!(
                    "did not receive the sandboxed target's PID before it closed the reporting pipe: {e}"
                )))
            })?;
            let target_pid = Pid::from_raw(i32::from_le_bytes(pid_bytes));

            // SAFETY: these fds are freshly created pipe ends this function alone owns at
            // this point (the child's copies were closed above), so wrapping them is not
            // aliasing anything else.
            let stdin = std::process::ChildStdin::from(stdin_write);
            let stdout = std::process::ChildStdout::from(stdout_read);

            let timed_out = Arc::new(AtomicBool::new(false));
            let timeout = spec.timeout;
            let watcher_timed_out = Arc::clone(&timed_out);
            std::thread::spawn(move || {
                std::thread::sleep(timeout);
                // Best-effort, same posture as `discovery::ChildProcessTransport`'s
                // watchdog. Kills `target_pid` — the actual PID-1-of-namespace process —
                // not `init_pid`: `init_pid` is merely that process's OS-level parent from
                // the *outer* namespace, and killing it does not touch the PID namespace's
                // own membership at all. Only the death of PID 1 *inside* the namespace
                // triggers the kernel's automatic teardown of everything else in it; this
                // was gotten wrong once during development (killing `init_pid` left the
                // real target running, undetected until a stray process turned up in `ps`)
                // and is exactly why the two PIDs are tracked and killed separately here.
                if signal::kill(target_pid, Signal::SIGKILL).is_ok() {
                    watcher_timed_out.store(true, Ordering::SeqCst);
                }
            });

            let handle = SandboxHandle {
                init_pid,
                target_pid,
                upper: spec.overlay.upper.clone(),
                timed_out,
                network_isolated: spec.network_isolated,
            };
            Ok((handle, stdin, stdout))
        }
        ForkResult::Child => {
            drop(stdin_write);
            drop(stdout_read);
            let target = TargetSetup {
                mount_options,
                mountpoint,
                program,
                argv,
                network_isolated: spec.network_isolated,
            };
            run_sandboxed_init(stdin_read, stdout_write, err_write, pid_write, target)
        }
    }
}

fn cstring_from_path(path: &std::path::Path) -> Result<CString, SpawnError> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|e| SpawnError(io::Error::new(io::ErrorKind::InvalidInput, e)))
}

fn nix_err(e: nix::Error) -> SpawnError {
    SpawnError(io::Error::from(e))
}

/// Report `context: e` on `err_write` and terminate immediately. Used for every failure
/// between `fork` and the real target's `execvp` — none of that window is safe to unwind
/// or panic through, so every fallible step in [`run_sandboxed_init`] routes here instead of
/// using `?` against a function that could ever return normally on the error path.
#[allow(unsafe_code)]
fn die(err_write: &OwnedFd, context: &str, e: impl std::fmt::Display) -> ! {
    let message = format!("{context}: {e}");
    let _ = nix::unistd::write(err_write, message.as_bytes());
    // SAFETY: `_exit` is async-signal-safe and appropriate here specifically because we are
    // not the original process std::process::Command would have exec'd — this is a forked
    // child that must never run the parent's atexit handlers or flush its buffered stdio.
    unsafe { libc::_exit(1) }
}

/// The real target program to launch, in the shape the raw `mount`/`execvp` calls need
/// rather than [`SandboxSpec`]'s own (grouped into one type purely to keep
/// [`run_sandboxed_init`]'s argument count sane).
struct TargetSetup {
    mount_options: String,
    mountpoint: PathBuf,
    program: CString,
    argv: Vec<CString>,
    network_isolated: bool,
}

/// Everything the outer forked process ("fork-1" in the module doc comment) does: redirect
/// stdio, create the user/PID/mount namespaces, mount the overlay, fork a second time (the
/// only way `CLONE_NEWPID` actually takes effect for the real target — see the module doc
/// comment), and become that second fork's minimal "init," waiting for it and mirroring its
/// exit status. Never returns: every path here ends in `_exit`, `execvp`, or [`die`].
#[allow(unsafe_code)]
fn run_sandboxed_init(
    stdin_read: OwnedFd,
    stdout_write: OwnedFd,
    err_write: OwnedFd,
    pid_write: OwnedFd,
    target: TargetSetup,
) -> ! {
    let TargetSetup { mount_options, mountpoint, program, argv, network_isolated } = target;
    if let Err(e) = nix::unistd::dup2_stdin(&stdin_read) {
        die(&err_write, "dup2 stdin", e);
    }
    if let Err(e) = nix::unistd::dup2_stdout(&stdout_write) {
        die(&err_write, "dup2 stdout", e);
    }
    drop(stdin_read);
    drop(stdout_write);

    let mut clone_flags = CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWNS;
    if network_isolated {
        // P3-01: a fresh network namespace with no interfaces configured — see the module
        // doc comment for why this is opt-in per spawn rather than unconditional, and why
        // loopback is deliberately left down too.
        clone_flags |= CloneFlags::CLONE_NEWNET;
    }
    if let Err(e) = unshare(clone_flags) {
        die(&err_write, "unshare(user+pid+mount[+net])", e);
    }

    if let Err(e) = std::fs::write("/proc/self/setgroups", b"deny") {
        die(&err_write, "write /proc/self/setgroups", e);
    }
    // Real defense in depth where the host allows it; an identity mapping (which this
    // environment's own outer confinement permits — see the module doc comment) otherwise.
    // `EPERM` specifically is the only error this falls back on; anything else (a map
    // already set, a malformed write, ...) is a real failure and is reported as one.
    let uid_map = format!("0 {MAPPED_UID} 1\n");
    if let Err(e) = std::fs::write("/proc/self/uid_map", &uid_map) {
        if e.kind() != io::ErrorKind::PermissionDenied {
            die(&err_write, "write /proc/self/uid_map", e);
        }
        if let Err(e) = std::fs::write("/proc/self/uid_map", b"0 0 1\n") {
            die(&err_write, "write /proc/self/uid_map (identity fallback)", e);
        }
    }
    let gid_map = format!("0 {MAPPED_GID} 1\n");
    if let Err(e) = std::fs::write("/proc/self/gid_map", &gid_map) {
        if e.kind() != io::ErrorKind::PermissionDenied {
            die(&err_write, "write /proc/self/gid_map", e);
        }
        if let Err(e) = std::fs::write("/proc/self/gid_map", b"0 0 1\n") {
            die(&err_write, "write /proc/self/gid_map (identity fallback)", e);
        }
    }

    // Make the whole mount tree private, recursively, *before* mounting anything. Without
    // this, the overlay mount below would propagate into the host's mount namespace via the
    // shared-subtree default most distros ship — exactly the containment failure a private
    // mount namespace is supposed to prevent.
    if let Err(e) =
        mount(None::<&str>, "/", None::<&str>, MsFlags::MS_REC | MsFlags::MS_PRIVATE, None::<&str>)
    {
        die(&err_write, "mount make-rprivate", e);
    }
    if let Err(e) = mount(
        Some("overlay"),
        mountpoint.as_path(),
        Some("overlay"),
        MsFlags::empty(),
        Some(mount_options.as_str()),
    ) {
        die(&err_write, "mount overlay", e);
    }

    // Second fork: `CLONE_NEWPID` above only affects children born after this point, so
    // the process created here — not this one — becomes PID 1 of the new namespace. This
    // process becomes that namespace's minimal "init": it owns no other job than waiting
    // for the real target and mirroring its exit, and its own death is what triggers the
    // kernel's automatic, unconditional teardown of every process left in the namespace.
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child: real_target }) => {
            // Report the real target's PID back to the supervisor *before* anything else —
            // this is the one piece of information the timeout watchdog needs to kill the
            // right process (see `spawn`'s own doc comment on why `init_pid` is the wrong
            // target). Do this ahead of the wait loop so it happens exactly once, regardless
            // of how many times that loop below iterates on stop/continue events.
            let pid_bytes = real_target.as_raw().to_le_bytes();
            if nix::unistd::write(&pid_write, &pid_bytes).is_err() {
                // The supervisor is gone (its end of the pipe closed) — nothing left to
                // report to or wait for; take the real target down with us rather than
                // leaving it running unsupervised.
                let _ = signal::kill(real_target, Signal::SIGKILL);
                unsafe { libc::_exit(1) }
            }
            drop(pid_write);
            // This process (unlike the real target) never `execvp`s, so `err_write`'s
            // `O_CLOEXEC` flag never fires for it — without dropping it explicitly here, this
            // copy of the write end would stay open for as long as the loop below runs
            // (i.e. the entire lifetime of the sandboxed run), and the supervisor's
            // `read_to_end` on the other end would then block for that entire duration
            // instead of returning as soon as the real target's own `execvp` succeeds. Found
            // by hand: an early version of this function hung every `spawn()` call until the
            // sandboxed process finished, exactly this way.
            drop(err_write);

            loop {
                match waitpid(real_target, None) {
                    Ok(WaitStatus::Exited(_, code)) => unsafe { libc::_exit(code) },
                    Ok(WaitStatus::Signaled(_, sig, _)) => unsafe { libc::_exit(128 + sig as i32) },
                    // Ok(_): stopped/continued, not terminal. Err(EINTR): waitpid itself was
                    // interrupted by a signal. Neither ends the loop; keep waiting either way.
                    Ok(_) | Err(nix::Error::EINTR) => continue,
                    Err(_) => unsafe { libc::_exit(1) },
                }
            }
        }
        Ok(ForkResult::Child) => {
            drop(pid_write);
            if let Err(e) = nix::unistd::chdir(mountpoint.as_path()) {
                die(&err_write, "chdir", e);
            }
            // P4-01: the last thing before this process becomes the real target's own code
            // — a seccomp filter installed here persists across `execvp` by kernel design,
            // so the target can never shed it.
            if let Err(e) = crate::seccomp::install_escape_class_denylist() {
                die(&err_write, "install seccomp filter", e);
            }
            match nix::unistd::execvp(&program, &argv) {
                Ok(infallible) => match infallible {},
                Err(e) => die(&err_write, "execvp", e),
            }
        }
        Err(e) => die(&err_write, "second fork (pid namespace init)", e),
    }
}

impl SandboxHandle {
    /// The namespace's long-lived "init" process (fork-1 — see the module doc comment).
    /// P3-02's own `NetworkBridge::set_up` takes exactly this PID: fork-1 and the real
    /// target share the identical network namespace (the target inherits it unchanged from
    /// fork-1's own `unshare`), but only fork-1 is guaranteed to survive for the run's
    /// entire duration, so referencing it instead of the real target's own PID removes a
    /// real race (a trivially fast target could already have exited) rather than accepting
    /// it.
    #[must_use]
    pub const fn init_pid(&self) -> Pid {
        self.init_pid
    }

    /// The real target's own PID, as seen from the host's (initial) PID namespace — not its
    /// namespace-local self-view (which is always `1`, since it is PID 1 of its own
    /// namespace, per P2-01). P4-02's `observe::seccomp_audit` needs exactly this PID:
    /// confirmed directly that a `SECCOMP_AUDIT` record's own `pid=` field reports a denying
    /// process's PID in the initial namespace, matching what this getter returns, not the
    /// namespace-local value the process sees for itself.
    #[must_use]
    pub const fn target_pid(&self) -> Pid {
        self.target_pid
    }

    /// Block until the sandboxed process exits (naturally, or via the timeout watchdog's
    /// `SIGKILL`), reap it, and report what happened. Consumes the handle. If the caller is
    /// still holding the stdin pipe `spawn` returned, drop it first — same requirement as
    /// waiting on any `std::process::Child` whose stdin is still open.
    ///
    /// # `unsafe_code`
    ///
    /// `libc::waitpid` is used directly (rather than `nix`'s parsed wrapper) specifically to
    /// get the raw wait status integer `std::os::unix::process::ExitStatusExt::from_raw`
    /// needs — this module's `init_pid` was never a `std::process::Child` to begin with (see
    /// the module doc comment), so there is no higher-level `wait()` to call instead.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if `waitpid` itself fails.
    #[allow(unsafe_code)]
    pub fn wait(self) -> io::Result<SandboxOutcome> {
        use std::os::unix::process::ExitStatusExt;

        let mut raw_status: libc::c_int = 0;
        // SAFETY: `init_pid` is a real, live (or already-zombie) child of this process —
        // `spawn` created it directly via `fork()` and nothing else reaps it — and
        // `&mut raw_status` is a valid, uniquely-owned local the kernel writes into.
        let ret = unsafe { libc::waitpid(self.init_pid.as_raw(), &mut raw_status, 0) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        let exit_status = ExitStatusExt::from_raw(raw_status);

        // The watchdog thread may fire concurrently with a process that is *also* about to
        // exit naturally; reading the flag after `wait()` returns (rather than racing it
        // against the sleep) means "timed out" is only ever true once the kill has actually
        // been attempted, never speculatively.
        let timed_out = self.timed_out.load(Ordering::SeqCst);
        Ok(SandboxOutcome {
            upper: self.upper,
            exit_status: Some(exit_status),
            timed_out,
            orphans_impossible: true,
            network_isolated: self.network_isolated,
        })
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
            network_isolated: false,
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
    ///
    /// `/bin/sleep` directly, not `sh -c "sleep ..."`: at the time this test was first
    /// written (P1-03), a shell wrapping a single command forking a grandchild rather than
    /// `exec`-ing in place would have leaked a real, long-lived orphan process on every run,
    /// since Phase 1 had no PID namespace to catch it. P2-01 has since closed that gap —
    /// `a_grandchild_the_target_abandons_is_killed_by_pid_namespace_teardown` below proves it
    /// directly with exactly the `sh -c` shape this test originally had to avoid — but this
    /// test is kept on plain `sleep` anyway: it exists to prove the *timeout* path in
    /// isolation, and mixing in a shell/grandchild would just be testing two things in one
    /// place for no added coverage the dedicated test doesn't already provide.
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
            network_isolated: false,
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
            network_isolated: false,
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

    /// Scans `/proc` (from this test's own, unsandboxed perspective on the host) for any
    /// live process whose command line matches `sleep 3600` — the same technique used by
    /// hand while diagnosing this module's own PID-reporting bug during development, now
    /// turned into an assertion instead of a manual `ps` check.
    fn a_sleep_3600_process_is_running() -> bool {
        let Ok(entries) = std::fs::read_dir("/proc") else { return false };
        for entry in entries.flatten() {
            let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else { continue };
            let text = String::from_utf8_lossy(&cmdline);
            if text.contains("sleep") && text.contains("3600") {
                return true;
            }
        }
        false
    }

    /// P2-01's literal exit criterion, demonstrated rather than only claimed by
    /// `orphans_impossible`: recreate the exact scenario P1-03's own tests had to sidestep
    /// (a shell forks a background process and exits without waiting for it — see the
    /// `a_hung_process_is_killed...` test's own doc comment for that history), and confirm
    /// the abandoned grandchild does not survive as a real, host-visible process. Before
    /// P2-01, it did — this is precisely the gap that made those earlier tests avoid `sh -c`
    /// wrapping in the first place.
    #[test]
    fn a_grandchild_the_target_abandons_is_killed_by_pid_namespace_teardown() {
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
            args: vec!["-c".to_string(), "sleep 3600 & exit 0".to_string()],
            timeout: Duration::from_secs(10),
            network_isolated: false,
        };

        let (handle, stdin, _stdout) = spawn(&spec).expect("spawn");
        drop(stdin);
        let outcome = handle.wait().expect("wait");

        assert!(!outcome.timed_out);
        assert!(outcome.exit_status.expect("has a status").success());
        assert!(outcome.orphans_impossible);

        // The kernel's teardown of a dying PID namespace's remaining members is not
        // necessarily instantaneous relative to `waitpid` returning for PID 1; poll briefly
        // rather than asserting on the very first instant, to avoid a rare, spurious
        // failure without weakening what's actually being proven.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while a_sleep_3600_process_is_running() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            !a_sleep_3600_process_is_running(),
            "the grandchild the shell abandoned must have been killed by PID-namespace \
             teardown, not left running as a real host process"
        );
    }

    /// P3-01's literal exit criterion: with `network_isolated: true`, a real egress attempt
    /// from inside the sandbox fails, and fails *fast* — not a slow-path timeout dressed up
    /// as "no route." Runs a real Python process (not a synthetic assertion about what a
    /// namespace "should" do) attempting both a raw TCP `connect()` and a DNS lookup against
    /// real external destinations, and asserts on the exact, immediate kernel-level failures
    /// this module's own doc comment already found empirically: `ENETUNREACH` for the
    /// connect, "temporary failure in name resolution" for the lookup — both in comfortably
    /// under a second, proving this is genuine "no route exists" containment rather than a
    /// connection that merely never got a reply.
    #[test]
    fn network_isolated_run_has_no_route_out_and_fails_fast() {
        let lower_dir = tempfile::tempdir().expect("tempdir");
        build_trivial_lower(lower_dir.path());
        let scratch = tempfile::tempdir().expect("tempdir");

        let script = "\
import socket
try:
    socket.create_connection(('8.8.8.8', 53), timeout=5)
    print('CONNECT: unexpectedly succeeded')
except OSError as e:
    print('CONNECT:', e)
try:
    socket.getaddrinfo('example.com', 443)
    print('DNS: unexpectedly succeeded')
except OSError as e:
    print('DNS:', e)
";

        let spec = SandboxSpec {
            overlay: OverlaySpec {
                lower: lower_dir.path().to_path_buf(),
                upper: scratch.path().join("upper"),
                work: scratch.path().join("work"),
                mountpoint: scratch.path().join("merged"),
            },
            program: PathBuf::from("/usr/local/bin/python3"),
            args: vec!["-c".to_string(), script.to_string()],
            timeout: Duration::from_secs(10),
            network_isolated: true,
        };

        let start = std::time::Instant::now();
        let (handle, stdin, mut stdout) = spawn(&spec).expect("spawn");
        drop(stdin);
        let mut output = String::new();
        stdout.read_to_string(&mut output).expect("read sandboxed stdout");
        let outcome = handle.wait().expect("wait");
        let elapsed = start.elapsed();

        assert!(!outcome.timed_out, "a network-isolated egress attempt must fail fast, not hang to the timeout");
        assert!(
            elapsed < Duration::from_secs(5),
            "the whole run (spawn, two failed egress attempts, teardown) took {elapsed:?}; \
             a genuine 'no route' failure is a low-millisecond kernel decision, not a slow path"
        );
        assert!(outcome.exit_status.expect("has a status").success(), "python's own script must exit cleanly: {output}");
        assert!(outcome.network_isolated, "the outcome must record that isolation was actually applied");

        let connect_line = output.lines().find(|l| l.starts_with("CONNECT:")).unwrap_or_default();
        assert!(
            connect_line.to_ascii_lowercase().contains("network is unreachable"),
            "a raw connect() to an external address must fail with ENETUNREACH, not: {connect_line:?} (full output: {output})"
        );
        let dns_line = output.lines().find(|l| l.starts_with("DNS:")).unwrap_or_default();
        assert!(
            dns_line.to_ascii_lowercase().contains("name resolution")
                || dns_line.to_ascii_lowercase().contains("name or service not known"),
            "DNS resolution must fail immediately with no route to any resolver, not: {dns_line:?} (full output: {output})"
        );
    }

    /// P4-01's literal exit criterion, proven against a real sandboxed process, not a
    /// synthetic namespace check: `ptrace`, `mount`, and `bpf` — three of the categories the
    /// escape-class denylist covers — each fail with `EPERM` from *inside* the sandbox, and
    /// the process survives to report all three results, rather than being killed by the
    /// first one (`SECCOMP_RET_ERRNO`, not `SECCOMP_RET_TRAP`/`KILL` — see `seccomp`'s own
    /// module doc comment for why that choice matters for P4-05's "every attempt appears in
    /// evidence"). A syscall *not* on the denylist (`getpid`) still succeeds normally in the
    /// same process, proving the filter denies specifically what it targets rather than
    /// coincidentally breaking everything.
    #[test]
    fn escape_class_syscalls_are_denied_with_eperm_and_the_process_survives() {
        let lower_dir = tempfile::tempdir().expect("tempdir");
        build_trivial_lower(lower_dir.path());
        let scratch = tempfile::tempdir().expect("tempdir");

        #[allow(clippy::literal_string_with_formatting_args)] // embedded Python source (an f-string), not a Rust format string
        let script = "\
import ctypes, os
libc = ctypes.CDLL(None, use_errno=True)

def try_syscall(nr, name):
    ret = libc.syscall(nr, 0, 0, 0, 0, 0, 0)
    err = ctypes.get_errno()
    print(f'{name}: ret={ret} errno={err}')

try_syscall(101, 'ptrace')       # SYS_ptrace
try_syscall(165, 'mount')        # SYS_mount
try_syscall(321, 'bpf')          # SYS_bpf
print(f'getpid: {os.getpid()}')  # not denied -- must still work
";

        let spec = SandboxSpec {
            overlay: OverlaySpec {
                lower: lower_dir.path().to_path_buf(),
                upper: scratch.path().join("upper"),
                work: scratch.path().join("work"),
                mountpoint: scratch.path().join("merged"),
            },
            program: PathBuf::from("/usr/local/bin/python3"),
            args: vec!["-c".to_string(), script.to_string()],
            timeout: Duration::from_secs(10),
            network_isolated: false,
        };

        let (handle, stdin, mut stdout) = spawn(&spec).expect("spawn");
        drop(stdin);
        let mut output = String::new();
        stdout.read_to_string(&mut output).expect("read sandboxed stdout");
        let outcome = handle.wait().expect("wait");

        assert!(!outcome.timed_out, "sandboxed output: {output}");
        assert!(
            outcome.exit_status.expect("has a status").success(),
            "the process must survive every denial and exit cleanly, not be killed by the \
             first one: {output}"
        );

        for name in ["ptrace", "mount", "bpf"] {
            let line = output.lines().find(|l| l.starts_with(&format!("{name}:"))).unwrap_or_default();
            assert!(
                line.contains("ret=-1") && line.contains(&format!("errno={}", libc::EPERM)),
                "{name} must be denied with EPERM (ret=-1, errno={}), got: {line:?} (full \
                 output: {output})",
                libc::EPERM
            );
        }

        let getpid_line = output.lines().find(|l| l.starts_with("getpid:")).unwrap_or_default();
        assert!(
            getpid_line.strip_prefix("getpid: ").and_then(|s| s.trim().parse::<u32>().ok()).is_some_and(|pid| pid > 0),
            "a syscall not on the denylist must still work normally: {getpid_line:?} (full \
             output: {output})"
        );
    }
}
