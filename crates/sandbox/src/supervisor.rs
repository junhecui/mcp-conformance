//! P1-03: the sandbox supervisor — mount namespace, overlayfs, hard timeout, teardown.
//!
//! Phase 1's "crudest containment that works" (design.md §10), and deliberately no more:
//! PID/user namespaces are P2-01, cgroups P2-02, the network namespace P3-01, seccomp P4-01.
//! Where each of those slots into the launch sequence is marked below with its task id.
//!
//! **Must not:** emit any verdict. [`RunRecord`] is a statement of what happened — exit
//! status, whether the hard timeout fired, what teardown found — and where the upper layer
//! is. It is not harvested or interpreted here (P1-04 harvests, P1-05 gates, P1-07 decides).
//!
//! # Topology (architecture.md §5)
//!
//! The MCP **client stays on the host**; the server runs inside the sandbox; the protocol
//! crosses over the child's stdio. [`spawn`] returns a [`SandboxStdio`] holding the host ends
//! of all three pipes, and nothing else connects the two sides.
//!
//! # Launch sequence
//!
//! Host side, before `fork`: validate the spec (including that [`RunAs`]'s uid, its helper uid
//! `uid + 1`, and its gid are ≥ 65 536 with no entry in `/etc/passwd` or `/etc/group`), take
//! the exclusive locks on the run uid and the helper uid, and refuse if any process is already
//! running under either. Then, in the forked child, between `fork` and `exec`:
//!
//! 1. `unshare(CLONE_NEWNS)` — a private mount namespace.
//! 2. `mount --make-rprivate /` — nothing mounted from here on propagates back to the host,
//!    and nothing the host mounts later propagates in. Load-bearing: without it the overlay
//!    leaks into the host's namespace (ADR-016).
//! 3. Hand the supervisor an fd for the new namespace (`/proc/self/ns/mnt`, over
//!    `SCM_RIGHTS`). Holding it pins the namespace, which is what keeps its identity
//!    meaningful: nsfs inode numbers are **reused** once a namespace dies, and an identity
//!    compared after that matched a *different, newer* sandbox's namespace in testing - and
//!    teardown killed that sandbox's process. The fd is released only after the final scan.
//! 4. Mount the overlay on `<scratch>/merged`: privileged (ADR-016), `nosuid,nodev`, with
//!    exactly ADR-010's pinned options and no `userxattr` ([`crate::overlay::mount_data`]).
//! 5. `pivot_root` into the merged view and detach the old root (`pivot_root(".", ".")` then
//!    `umount2(".", MNT_DETACH)`, the sequence pivot_root(2) documents), then `chdir("/")`.
//!    After this the host filesystem — upper and work directories included — has no path
//!    from inside. `umask 022`, so upper-layer modes do not depend on the harness's umask.
//! 6. Mark every fd ≥ 3 close-on-exec (`close_range(3, ~0, CLOSE_RANGE_CLOEXEC)`).
//! 7. Empty the capability **bounding set** (`PR_CAPBSET_DROP` for every cap up to
//!    `cap_last_cap`, read before `fork`) and lock the securebits: `NOROOT`,
//!    `NO_CAP_AMBIENT_RAISE`, and the `_LOCKED` bits for those plus `KEEP_CAPS` (off) and
//!    `NO_SETUID_FIXUP` (off) — so the setuid fixup that clears capabilities on the next step
//!    stays in force and cannot be disabled later.
//! 8. Empty supplementary groups, `setresgid`, `setresuid` to [`RunAs`] — every capability
//!    set, bounding included, is now zero, and `capget` **confirms** it (the launch fails
//!    otherwise) rather than assuming the kernel's setuid fixup ran. `setsid`, so the run has
//!    its own session (a process may `SIGCONT` anything in its session regardless of uid).
//!    Then `chdir` to the spec's cwd, *after* the drop, so the unprivileged id's permission
//!    checks apply. Then `PR_SET_NO_NEW_PRIVS`.
//! 9. `PR_SET_PDEATHSIG(SIGKILL)`, then confirm the parent is still the supervisor.
//!
//! The environment is cleared and replaced with [`SandboxSpec::env`] verbatim.
//!
//! # What runs between fork and exec
//!
//! The parent is multithreaded, so the child may only do async-signal-safe work: it must not
//! allocate, because another thread may have held the allocator's lock at the instant of
//! `fork`. Every string the child needs is built as a `CString` *before* `fork`; rustix's
//! raw-syscall backend does not allocate when handed a `&CStr`; `prctl` and `close_range` go
//! through libc's thin syscall wrappers; errors become `io::Error::from(Errno)`, which is
//! allocation-free.
//!
//! # The timeout, and the end of a run
//!
//! A watchdog thread owns the end of the run. It waits on the child's pidfd until the child
//! exits or the deadline passes; at the deadline it `SIGKILL`s the child. Either way it then
//! reaps the child, snapshots any of the run's processes still alive, and kills them
//! ([`crate::sweep`]): a helper that the run cannot signal loops `kill(-1, SIGKILL)` over the
//! run's uid until the supervisor's scans see it empty, and anything a scan still finds is
//! `SIGKILL`ed from root through a pidfd. So the timeout bounds the *whole run*, not just the
//! direct child, and the run's other processes are killed as soon as the direct child ends: a
//! descendant holding the stdio pipes dies then, and the host's reads see EOF then — not when
//! the descendant chooses to exit, and not only once [`Sandbox::wait`] is called. If the run
//! defeats the kill (it would have to outpace a per-uid `kill(-1)` and root's per-pid kills),
//! the attempt still ends in a bounded time and the record says so
//! ([`TeardownReport::uid_sweep_completed`], [`TeardownReport::confirmed_empty`]). A
//! descendant alive when the direct child ended is a recorded fact
//! ([`TeardownReport::residual_pids`]), not a timeout.
//!
//! # Why the child is not root, and what still is not contained
//!
//! A mount namespace alone contains nothing against a root process: real root with
//! `CAP_SYS_ADMIN` and `CAP_MKNOD` can mount a fresh procfs (whose `/proc/1/root` is the host
//! root) or `mknod` the host's disk and mount it. Steps 7–8 are what make the mount namespace
//! a boundary at all. For the same reason **no procfs, sysfs or `/dev` is mounted inside**: a
//! procfs without a PID namespace exposes every host process's `root` link. P2-01's PID
//! namespace is the precondition for mounting one.
//!
//! Known gaps, all owned by later tasks rather than papered over here:
//!
//! * **No PID namespace (P2-01), no `CLONE_NEWUSER` denial (P4-01).** Run processes can fork
//!   and can create nested user+mount namespaces, which take them out of the run's mount
//!   namespace. The uid sweep catches them regardless (they keep the host uid), and
//!   [`TeardownReport::residual_outside_namespace`] reports any that did this. But it is a
//!   sweep, not a boundary: until it reaches them, run processes run, and may write the upper
//!   layer. P1-05 must read a run as `containment_uncertain` if `residual_pids` is non-empty,
//!   `uid_sweep_completed` is false, or `confirmed_empty` is false. A server started through
//!   a launcher that forks (`npx` → `node`) therefore always reads as uncertain until P2-01.
//! * **"Clean" means "no descendant observed".** The scans fail closed — a `/proc` entry that
//!   exists but cannot be read or parsed counts as present (`unverified_pids`), never as
//!   absent — but they are not atomic, so until P2-02's cgroup accounting a clean record means
//!   no descendant was *observed*, not that none existed. A fork-and-exit chain whose pids are
//!   allocated below the readdir cursor (cyclic allocation after a pid wrap, which a run can
//!   force with ~`pid_max` forks) can be missed by every scan. The helper's `kill(-1)` still
//!   kills it, but `residual_pids` comes out empty. A forward chain without a wrap is always
//!   found (`sweep` module docs). P1-05 must carry that qualifier with every verdict.
//! * **Shared network, IPC and UTS namespaces.** The child has full network access, can reach
//!   abstract-namespace Unix sockets and SysV IPC on the host. P3-01 (network); IPC/UTS are
//!   one flag each and belong with P2-01's namespace work.
//! * **No resource limits** (P2-02). A fork bomb or memory hog affects the host.
//! * **No syscall filter** (P4-01).
//!
//! And none of this is a substitute for F-08's "Preconditions on the transition to running
//! untrusted code" — a separate scratch VM, network isolation of the VM itself, snapshot and
//! revert — which bind before P2-06 and P4-05 run untrusted registry code.

use std::ffi::{CString, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::io::{IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags};
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};
use rustix::io::Errno;
use rustix::mount::{MountFlags, MountPropagationFlags, UnmountFlags, mount, mount_change, unmount};
use rustix::process::{
    Gid, Pid, PidfdFlags, Signal, Uid, chdir, geteuid, getpid, getppid, pidfd_open,
    pidfd_send_signal, pivot_root, set_parent_process_death_signal, umask,
};
use rustix::thread::{
    UnshareFlags, set_no_new_privs, set_thread_groups, set_thread_res_gid, set_thread_res_uid,
    unshare_unsafe,
};

use crate::overlay::{self, PathRejection};
use crate::sweep::{self, NsId, RunIds, Scan};

/// The unprivileged identity the tool runs as, after the mount setup that needs root.
///
/// **A run owns two uids, exclusively: `uid` and [`helper_uid`](Self::helper_uid)
/// (`uid + 1`).** Teardown identifies the run's processes by `uid`, and kills them with a
/// helper whose real and saved uid is the helper uid so the run cannot signal it (see
/// [`crate::sweep`]). [`spawn`] takes a lock on both for the whole run, refuses either if
/// another live sandbox holds it ([`SandboxError::RunAsInUse`]), and refuses either if it
/// already has processes ([`SandboxError::RunAsOccupied`]). Concurrent sandboxes therefore
/// need uids at least 2 apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunAs {
    /// Real, effective and saved uid. At least [`RunAs::MIN_ID`] and below `u32::MAX - 1`;
    /// neither it nor `uid + 1` may have an entry in `/etc/passwd`.
    pub uid: u32,
    /// Real, effective and saved gid. At least [`RunAs::MIN_ID`], with no entry in
    /// `/etc/group`.
    pub gid: u32,
}

impl RunAs {
    /// The smallest acceptable id. Everything below is the conventional territory of real
    /// accounts, system users, `nobody` (65 534) and systemd's dynamic ranges; a run under a
    /// real account's uid could signal that account's processes.
    pub const MIN_ID: u32 = 65_536;

    /// A dedicated id with no `/etc/passwd` entry, chosen to collide with nothing on a stock host:
    /// above [`RunAs::MIN_ID`] and above the conventional subordinate-id allocations
    /// (`/etc/subuid` starts at 100 000 in 65 536 blocks), and below 2³¹ so nothing that
    /// treats ids as signed 32-bit misreads it.
    pub const DEFAULT: Self = Self { uid: 2_000_000_000, gid: 2_000_000_000 };

    /// The run's helper uid, `uid + 1` — reserved for the teardown helper and never used by
    /// the tool. Saturating only so the call cannot panic; [`spawn`] refuses a `uid` for which
    /// it would saturate.
    #[must_use]
    pub const fn helper_uid(self) -> u32 {
        self.uid.saturating_add(1)
    }

    const fn ids(self) -> RunIds {
        RunIds { uid: self.uid, gid: self.gid, helper_uid: self.helper_uid() }
    }
}

/// One launch of one program inside the sandbox.
#[derive(Debug, Clone)]
pub struct SandboxSpec {
    /// The read-only lower layer: a directory on the host, e.g. one P1-02's
    /// `world::base_layer` materialised. Never written (overlayfs guarantees it). Must exist
    /// and be disjoint from `scratch`.
    pub lower: PathBuf,
    /// A directory that must **not** exist yet; the supervisor creates it (mode `0700`) and
    /// owns everything under it: `upper/` (the changeset, preserved after teardown and handed
    /// to P1-04), `work/` (overlayfs's own scratch) and `merged/` (the mount point), the last
    /// two removed at teardown. Refusing an existing directory is what guarantees a run never
    /// starts with a previous run's upper layer.
    pub scratch: PathBuf,
    /// Absolute path of the program **inside the sandbox root** — it must exist in the lower
    /// layer. Never resolved through `PATH`.
    pub program: PathBuf,
    /// Arguments after `argv[0]`.
    pub args: Vec<OsString>,
    /// The complete environment. Nothing from the supervisor's own environment is inherited.
    pub env: Vec<(OsString, OsString)>,
    /// Working directory inside the sandbox root (`/` is allowed). Entered after the
    /// credential drop, so it must be searchable by `run_as`.
    pub cwd: PathBuf,
    /// The identity the program runs as. Exclusive to this run; see [`RunAs`].
    pub run_as: RunAs,
    /// Hard wall-clock limit from launch, for the **whole run**: on expiry the direct child is
    /// `SIGKILL`ed and every other process under `run_as` is swept, and
    /// [`RunRecord::timed_out`] is set. It therefore also bounds how long a host-side read of
    /// [`SandboxStdio`] can block. Not optional and not configurable off (ADR-004).
    pub timeout: Duration,
}

/// The host ends of the sandboxed process's stdio — the only channel between the MCP client
/// on the host and the server in the sandbox.
///
/// Reads see EOF once the run has ended: when the direct child exits, any run process still
/// holding the pipes is killed straight away, and at the deadline everything is. `stderr` is
/// piped rather than inherited so hostile output cannot reach the harness's own terminal (the
/// P0-11 lesson). The caller should drain it — `discovery`'s bounded, escape-stripping relay
/// is the model — or drop it, in which case the server's writes fail with `EPIPE`.
#[derive(Debug)]
pub struct SandboxStdio {
    /// Writes reach the server's stdin.
    pub stdin: ChildStdin,
    /// The server's stdout.
    pub stdout: ChildStdout,
    /// The server's stderr.
    pub stderr: ChildStderr,
}

/// What the end of the run found and did. Facts only; P1-05 decides what they mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeardownReport {
    /// Host pids of the run's processes (under `run_as.uid`, or in the run's mount namespace)
    /// that were still alive when the direct child had ended and been reaped — before any
    /// sweep. Phase 1 has no PID namespace (P2-01), so ending the direct child does not end
    /// these; the sweep then kills them. Non-empty means processes other than the direct
    /// child ran, possibly writing the upper layer, until the sweep reached them.
    pub residual_pids: Vec<i32>,
    /// The subset of `residual_pids` that was **not** in the run's mount namespace: processes
    /// that left it, e.g. by creating a nested user+mount namespace. Only the uid
    /// identifies them.
    pub residual_outside_namespace: Vec<i32>,
    /// Pids in `residual_pids` or `survivors` that existed but could not be verified either
    /// way (an unreadable or malformed `/proc` entry). Counted as present — a scan may report
    /// false positives, never a false empty — and never killed from root, because root kills
    /// only what it has verified as the run's.
    pub unverified_pids: Vec<i32>,
    /// Whether every helper sweep this run needed completed — the supervisor saw the uid
    /// empty on two consecutive scans within the helper's time bound — and every `/proc`
    /// scan along the way could be read. `false` means the kill was resisted or could not be
    /// verified.
    pub uid_sweep_completed: bool,
    /// Run processes still found by the final scan, by either identification. Empty both
    /// when nothing is left **and** when `/proc` could not be read; `confirmed_empty`
    /// distinguishes the two.
    pub survivors: Vec<i32>,
    /// `true` only when the final scan was readable and found **no** process in the run's
    /// mount namespace **and no** process under `run_as.uid`.
    pub confirmed_empty: bool,
    /// Whether `work/` and `merged/` were removed. Only attempted when `confirmed_empty`.
    pub scratch_removed: bool,
    /// Why removal failed, if it was attempted and did.
    pub removal_error: Option<String>,
}

/// The outcome of one run, as observed by the supervisor. **Not a verdict.**
#[derive(Debug)]
pub struct RunRecord {
    /// `<scratch>/upper` on the host: the overlay's upper layer exactly as the run left it,
    /// for P1-04 to harvest. Not read or interpreted here. Owned by the caller from now on.
    ///
    /// **Everything under it was written by the tool** and is hostile input: it may hold
    /// FIFOs, sockets, symlinks pointing anywhere, sparse or enormous files, and setuid/setgid
    /// files owned by the run's uid. The harvester must `lstat`/`O_NOFOLLOW` and never follow,
    /// open for blocking I/O, or execute anything there, and must bound what it reads.
    pub upper: PathBuf,
    /// How the direct child ended, from `waitpid`.
    pub status: ExitStatus,
    /// Whether the hard timeout fired: set when the watchdog's `SIGKILL` was delivered. A
    /// process that exits on its own in the instant before the kill lands can still be
    /// recorded as timed out, which errs toward `unverifiable`, never toward a verdict.
    pub timed_out: bool,
    /// Whether the caller ended the run through [`Sandbox::kill_and_wait`]. Like `timed_out`,
    /// set when the signal was delivered, which can include a child that had already exited
    /// but was not yet reaped.
    pub killed_on_request: bool,
    /// Launch to reap of the direct child.
    pub wall_time: Duration,
    /// What the end of the run found.
    pub teardown: TeardownReport,
}

/// Why a sandbox could not be launched, or could not be waited on. A tool misbehaving once
/// it is running is never an `Err` — that is what [`RunRecord`] reports.
#[derive(Debug)]
pub enum SandboxError {
    /// A spec field is unusable; the message says which and why.
    InvalidSpec(&'static str),
    /// A path failed [`overlay::validate_path`].
    UnsafePath {
        /// Which spec field (or derived path).
        what: &'static str,
        /// The offending path.
        path: PathBuf,
        /// Why.
        reason: PathRejection,
    },
    /// The lower layer and the scratch directory overlap, so the upper layer would be
    /// reachable from inside the sandbox through the lower layer.
    LayersOverlap {
        /// Canonical lower-layer path.
        lower: PathBuf,
        /// Canonical scratch path.
        scratch: PathBuf,
    },
    /// `scratch` already exists.
    ScratchExists(PathBuf),
    /// The supervisor is not running as root, which the privileged overlay mount (ADR-016)
    /// requires.
    NotPrivileged,
    /// Another live sandbox holds this uid.
    RunAsInUse(u32),
    /// Processes are already running under this uid (left over from an earlier run whose
    /// teardown did not complete, or something else entirely); refusing rather than adopting
    /// or killing them.
    RunAsOccupied {
        /// The uid.
        uid: u32,
        /// What was found.
        pids: Vec<i32>,
    },
    /// A host-side filesystem or process operation failed.
    Io {
        /// What was being done.
        context: &'static str,
        /// The error.
        source: io::Error,
    },
    /// The child could not be launched. `step` names the launch-sequence step that failed,
    /// when the child got far enough to report it; `None` usually means `exec` itself
    /// failed (e.g. `program` does not exist in the lower layer).
    Launch {
        /// The failing step, if known.
        step: Option<&'static str>,
        /// The error.
        source: io::Error,
    },
    /// Supervision itself failed after launch (the watchdog thread died). The run was
    /// killed and torn down; the teardown is reported so its findings are not lost.
    Supervision {
        /// What failed.
        what: &'static str,
        /// The teardown that ran anyway.
        teardown: Box<TeardownReport>,
    },
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSpec(why) => write!(f, "invalid sandbox spec: {why}"),
            Self::UnsafePath { what, path, reason } => {
                write!(f, "unsafe {what} path {}: {reason}", path.display())
            }
            Self::LayersOverlap { lower, scratch } => write!(
                f,
                "lower layer {} and scratch {} overlap; the upper layer would be reachable \
                 from inside the sandbox",
                lower.display(),
                scratch.display()
            ),
            Self::ScratchExists(p) => {
                write!(f, "scratch directory {} already exists; refusing to reuse it", p.display())
            }
            Self::NotPrivileged => f.write_str(
                "the sandbox supervisor must run as root (privileged overlay mount, ADR-016)",
            ),
            Self::RunAsInUse(uid) => write!(f, "uid {uid} is held by another live sandbox"),
            Self::RunAsOccupied { uid, pids } => {
                write!(f, "uid {uid} already has running processes {pids:?}; refusing to launch")
            }
            Self::Io { context, source } => write!(f, "{context}: {source}"),
            Self::Launch { step: Some(step), source } => {
                write!(f, "sandbox launch failed at step '{step}': {source}")
            }
            Self::Launch { step: None, source } => write!(f, "sandbox launch failed: {source}"),
            Self::Supervision { what, teardown } => {
                write!(f, "sandbox supervision failed ({what}); teardown: {teardown:?}")
            }
        }
    }
}

impl std::error::Error for SandboxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::Launch { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn io_err(context: &'static str) -> impl FnOnce(io::Error) -> SandboxError {
    move |source| SandboxError::Io { context, source }
}

/// The scratch layout, all canonical absolute paths.
#[derive(Debug, Clone)]
struct Layout {
    scratch: PathBuf,
    upper: PathBuf,
    work: PathBuf,
    merged: PathBuf,
}

/// Report-channel tags written by the child before `exec`.
const TAG_NS: u8 = b'N';
const TAG_FAILED: u8 = b'F';

/// Launch-sequence steps, in order. The index is the byte the child reports on failure.
const STEPS: [&str; 20] = [
    "unshare(CLONE_NEWNS)",
    "make / rprivate",
    "record mount namespace identity",
    "mount overlay",
    "chdir to merged",
    "pivot_root",
    "detach old root",
    "chdir to /",
    "close_range(CLOEXEC)",
    "drop capability bounding set",
    "lock securebits",
    "setgroups([])",
    "setresgid",
    "setresuid",
    "verify no capabilities",
    "setsid",
    "chdir to cwd",
    "PR_SET_NO_NEW_PRIVS",
    "PR_SET_PDEATHSIG",
    "parent still alive",
];

/// `NOROOT` and `NO_CAP_AMBIENT_RAISE` on; those two locked, and `KEEP_CAPS` and
/// `NO_SETUID_FIXUP` locked **off**, so the root-to-non-root transition keeps clearing every
/// capability set and nothing later can change that.
pub(crate) const SECUREBITS: libc::c_ulong = (libc::SECBIT_NOROOT
    | libc::SECBIT_NOROOT_LOCKED
    | libc::SECBIT_NO_SETUID_FIXUP_LOCKED
    | libc::SECBIT_KEEP_CAPS_LOCKED
    | libc::SECBIT_NO_CAP_AMBIENT_RAISE
    | libc::SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED) as libc::c_ulong;

/// The unused trailing `prctl` arguments, typed as the kernel reads them.
const ZERO: libc::c_ulong = 0;

/// Everything the child needs, prepared before `fork` so the child never allocates.
struct ChildPlan {
    merged: CString,
    mount_data: CString,
    cwd: CString,
    uid: Uid,
    gid: Gid,
    cap_last: u32,
    parent: Pid,
    /// Write end of the report channel. `SOCK_CLOEXEC` (std's default), so it closes at
    /// `exec`; owned by the `pre_exec` closure, so the parent's copy is released when the
    /// `Command` is dropped.
    report: UnixStream,
}

impl ChildPlan {
    /// Run one launch step; on failure, report its index and pass the errno through.
    fn step<T>(&self, index: u8, result: rustix::io::Result<T>) -> io::Result<T> {
        result.map_err(|errno| {
            let _ = rustix::io::write(&self.report, &[TAG_FAILED, index]);
            io::Error::from(errno)
        })
    }

    /// Turn a libc-style `-1`/errno return into a step result.
    fn libc_step(&self, index: u8, rc: libc::c_long) -> io::Result<()> {
        if rc == -1 {
            let errno = io::Error::last_os_error().raw_os_error().unwrap_or(libc::EINVAL);
            return self.step(index, Err(Errno::from_raw_os_error(errno)));
        }
        Ok(())
    }

    /// The child-side launch sequence (module docs, steps 1–9). Runs between `fork` and
    /// `exec`; must not allocate.
    #[allow(unsafe_code)]
    fn run(&self) -> io::Result<()> {
        // SAFETY: `unshare_unsafe` is unsafe only for `CLONE_FILES`, which can strand other
        // threads' fds; we pass `CLONE_NEWNS` alone, and the child is single-threaded anyway.
        self.step(0, unsafe { unshare_unsafe(UnshareFlags::NEWNS) })?;
        // P2-01 slots in here: CLONE_NEWPID (with a second fork so the tool is PID 1 of its
        // namespace) and CLONE_NEWIPC | CLONE_NEWUTS; CLONE_NEWUSER only *after* the overlay
        // mount below, which must stay a trusted-mode mount (ADR-016). P3-01: CLONE_NEWNET.
        self.step(1, mount_change(c"/", MountPropagationFlags::PRIVATE | MountPropagationFlags::REC))?;

        let ns = self.step(
            2,
            rustix::fs::open(c"/proc/self/ns/mnt", OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()),
        )?;
        // Stack buffers only: `SendAncillaryBuffer` writes into `space`, and `sendmsg` builds
        // its `msghdr` on the stack.
        let mut space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        let fds = [ns.as_fd()];
        control.push(SendAncillaryMessage::ScmRights(&fds));
        self.step(2, sendmsg(&self.report, &[IoSlice::new(&[TAG_NS])], &mut control, SendFlags::empty()))?;
        drop(ns);

        self.step(
            3,
            mount(
                c"overlay",
                self.merged.as_c_str(),
                c"overlay",
                MountFlags::NOSUID | MountFlags::NODEV,
                self.mount_data.as_c_str(),
            ),
        )?;
        self.step(4, chdir(self.merged.as_c_str()))?;
        self.step(5, pivot_root(c".", c"."))?;
        self.step(6, unmount(c".", UnmountFlags::DETACH))?;
        self.step(7, chdir(c"/"))?;
        umask(Mode::from_raw_mode(0o022));

        // SAFETY: a raw `close_range(2)` syscall with integer arguments only; it touches no
        // memory. `CLOSE_RANGE_CLOEXEC` marks rather than closes, which leaves std's own
        // exec-error pipe (already CLOEXEC) working.
        let rc = unsafe {
            libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, libc::CLOSE_RANGE_CLOEXEC)
        };
        self.libc_step(8, rc)?;

        // P2-02 slots in around here: joining the run's cgroup (or clone3 CLONE_INTO_CGROUP).
        for cap in 0..=self.cap_last {
            // SAFETY: `prctl` with integer arguments only; touches no memory.
            let rc = unsafe {
                libc::prctl(libc::PR_CAPBSET_DROP, libc::c_ulong::from(cap), ZERO, ZERO, ZERO)
            };
            self.libc_step(9, libc::c_long::from(rc))?;
        }
        // SAFETY: as above.
        let rc = unsafe { libc::prctl(libc::PR_SET_SECUREBITS, SECUREBITS, ZERO, ZERO, ZERO) };
        self.libc_step(10, libc::c_long::from(rc))?;

        self.step(11, set_thread_groups(&[]))?;
        self.step(12, set_thread_res_gid(self.gid, self.gid, self.gid))?;
        self.step(13, set_thread_res_uid(self.uid, self.uid, self.uid))?;
        // The kernel clears every set on this transition unless SECBIT_NO_SETUID_FIXUP is in
        // force; step 10 locked it off, but confirm rather than assume (a supervisor that
        // inherited the bit *locked on* fails step 10, and anything else lands here).
        let caps = self.step(14, rustix::thread::capabilities(None))?;
        if !(caps.effective.is_empty() && caps.permitted.is_empty() && caps.inheritable.is_empty()) {
            return self.step(14, Err(Errno::PERM));
        }
        // Its own session: otherwise the run shares the supervisor's session, and the kernel
        // lets a process send SIGCONT to anything in its session regardless of uid.
        self.step(15, rustix::process::setsid())?;
        self.step(16, chdir(self.cwd.as_c_str()))?;
        self.step(17, set_no_new_privs(true))?;
        // P4-01 slots in here: the seccomp filter, last before exec, after no_new_privs.
        self.step(18, set_parent_process_death_signal(Some(Signal::KILL)))?;
        if getppid() != Some(self.parent) {
            return self.step(19, Err(Errno::SRCH));
        }
        Ok(())
    }
}

/// Validate `spec` and build the scratch layout. Everything here happens before any
/// privilege check so that spec mistakes are reported the same way on any host.
fn prepare(spec: &SandboxSpec) -> Result<(PathBuf, Layout), SandboxError> {
    if spec.timeout.is_zero() {
        return Err(SandboxError::InvalidSpec("timeout must be non-zero"));
    }
    if spec.run_as.uid < RunAs::MIN_ID || spec.run_as.gid < RunAs::MIN_ID {
        return Err(SandboxError::InvalidSpec(
            "run_as uid and gid must be at least 65536: a run must not share an id with a \
             real account or system user, nor be root",
        ));
    }
    // `u32::MAX` is `(uid_t)-1`, "no change" to setresuid; the helper uid must stay below it.
    if spec.run_as.uid >= u32::MAX - 1 {
        return Err(SandboxError::InvalidSpec("run_as uid must be below u32::MAX - 1"));
    }
    // Likewise `(gid_t)-1`: setresgid(-1, -1, -1) changes nothing and the run would keep gid 0.
    if spec.run_as.gid == u32::MAX {
        return Err(SandboxError::InvalidSpec("run_as gid must be below u32::MAX"));
    }
    let passwd = fs::read_to_string("/etc/passwd").map_err(io_err("reading /etc/passwd"))?;
    if sweep::database_has_id(&passwd, spec.run_as.uid)
        || sweep::database_has_id(&passwd, spec.run_as.helper_uid())
    {
        return Err(SandboxError::InvalidSpec(
            "run_as uid or uid + 1 has an /etc/passwd entry; a run must not share a uid with a \
             real account",
        ));
    }
    let group = fs::read_to_string("/etc/group").map_err(io_err("reading /etc/group"))?;
    if sweep::database_has_id(&group, spec.run_as.gid) {
        return Err(SandboxError::InvalidSpec("run_as gid has an /etc/group entry"));
    }
    overlay::validate_path(&spec.program).map_err(|reason| SandboxError::UnsafePath {
        what: "program",
        path: spec.program.clone(),
        reason,
    })?;
    if spec.cwd.as_os_str() != "/" {
        overlay::validate_path(&spec.cwd).map_err(|reason| SandboxError::UnsafePath {
            what: "cwd",
            path: spec.cwd.clone(),
            reason,
        })?;
    }
    for (key, _) in &spec.env {
        let k = key.as_bytes();
        if k.is_empty() || k.contains(&b'=') || k.contains(&0) {
            return Err(SandboxError::InvalidSpec(
                "environment variable names must be non-empty and contain no '=' or NUL",
            ));
        }
    }

    let lower = fs::canonicalize(&spec.lower).map_err(io_err("resolving the lower layer"))?;
    if !fs::metadata(&lower).map_err(io_err("stat of the lower layer"))?.is_dir() {
        return Err(SandboxError::InvalidSpec("lower must be a directory"));
    }
    overlay::validate_path(&lower).map_err(|reason| SandboxError::UnsafePath {
        what: "lower",
        path: lower.clone(),
        reason,
    })?;

    let name = spec
        .scratch
        .file_name()
        .ok_or(SandboxError::InvalidSpec("scratch must name a directory"))?;
    let parent = spec
        .scratch
        .parent()
        .ok_or(SandboxError::InvalidSpec("scratch must have a parent directory"))?;
    let scratch = fs::canonicalize(parent).map_err(io_err("resolving scratch's parent"))?.join(name);
    if fs::symlink_metadata(&scratch).is_ok() {
        return Err(SandboxError::ScratchExists(scratch));
    }
    let layout = Layout {
        upper: scratch.join("upper"),
        work: scratch.join("work"),
        merged: scratch.join("merged"),
        scratch,
    };
    for (what, path) in [
        ("scratch", &layout.scratch),
        ("upper", &layout.upper),
        ("work", &layout.work),
        ("merged", &layout.merged),
    ] {
        overlay::validate_path(path).map_err(|reason| SandboxError::UnsafePath {
            what,
            path: path.clone(),
            reason,
        })?;
    }
    if !overlay::disjoint(&lower, &layout.scratch) {
        return Err(SandboxError::LayersOverlap { lower, scratch: layout.scratch });
    }
    Ok((lower, layout))
}

fn make_dir(path: &Path, mode: u32) -> Result<(), SandboxError> {
    fs::create_dir(path).map_err(io_err("creating a scratch directory"))?;
    // Explicit, so the result does not depend on the harness's umask.
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(io_err("setting scratch directory permissions"))
}

/// Launch `spec.program` inside a fresh sandbox. Returns the supervising handle and the
/// host ends of the program's stdio. The hard timeout is armed from this call.
///
/// Must be called as root (ADR-016). The calling *thread* matters: the child is armed with
/// `PR_SET_PDEATHSIG(SIGKILL)`, which Linux ties to the thread that forked it, so if that
/// thread exits while the sandbox is running, the sandbox is killed. Spawn from a thread that
/// outlives the run (in practice: the one that will call [`Sandbox::wait`]).
///
/// # Errors
///
/// [`SandboxError`] for an invalid spec, missing privilege, a uid in use or occupied, a
/// host-side I/O failure, or a launch failure. A launch that fails after the child exec'd is
/// torn down fully (uid sweep included) before the error is returned; the scratch directory
/// is removed only when that teardown found nothing left running.
pub fn spawn(spec: &SandboxSpec) -> Result<(Sandbox, SandboxStdio), SandboxError> {
    let (lower, layout) = prepare(spec)?;
    if !geteuid().is_root() {
        return Err(SandboxError::NotPrivileged);
    }
    let mut locks = Vec::with_capacity(2);
    for uid in [spec.run_as.uid, spec.run_as.helper_uid()] {
        let lock = sweep::lock_uid(uid)
            .map_err(io_err("taking the run's uid lock"))?
            .ok_or(SandboxError::RunAsInUse(uid))?;
        locks.push(lock);
        match sweep::scan(None, uid) {
            // Fail closed: a pid whose membership cannot be verified counts as occupying.
            Some(s) if s.is_empty() => {}
            Some(s) => return Err(SandboxError::RunAsOccupied { uid, pids: s.all() }),
            None => {
                return Err(SandboxError::Io {
                    context: "listing /proc to check the run's uids are unoccupied",
                    source: io::Error::other("/proc is not readable"),
                });
            }
        }
    }
    let cap_last = fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .map_err(io_err("reading cap_last_cap"))?
        .trim()
        .parse::<u32>()
        .map_err(|_| SandboxError::InvalidSpec("unparseable /proc/sys/kernel/cap_last_cap"))?;

    make_dir(&layout.scratch, 0o700)?;
    match launch(spec, &lower, &layout, cap_last, locks) {
        Ok(ok) => Ok(ok),
        Err((error, scratch_safe_to_remove)) => {
            if scratch_safe_to_remove {
                let _ = fs::remove_dir_all(&layout.scratch);
            }
            Err(error)
        }
    }
}

/// Launch, or fail with the error and whether removing the scratch directory is safe (no
/// run process can still be using it).
#[allow(unsafe_code)]
fn launch(
    spec: &SandboxSpec,
    lower: &Path,
    layout: &Layout,
    cap_last: u32,
    uid_locks: Vec<File>,
) -> Result<(Sandbox, SandboxStdio), (SandboxError, bool)> {
    // `upper` is 0755 because the merged root directory takes its attributes from the
    // topmost layer that has it — the upper root — and the unprivileged tool must be able to
    // traverse `/`. `work` and `merged` are never visible from inside.
    for (dir, mode) in [(&layout.upper, 0o755), (&layout.work, 0o700), (&layout.merged, 0o700)] {
        make_dir(dir, mode).map_err(|e| (e, true))?;
    }

    let mount_data = overlay::mount_data(lower, &layout.upper, &layout.work).map_err(
        |(role, reason)| {
            let (what, path) = match role {
                overlay::Role::Lower => ("lower", lower.to_path_buf()),
                overlay::Role::Upper => ("upper", layout.upper.clone()),
                overlay::Role::Work => ("work", layout.work.clone()),
            };
            (SandboxError::UnsafePath { what, path, reason }, true)
        },
    )?;
    // Validated paths contain no NUL.
    let to_c = |p: &Path| CString::new(p.as_os_str().as_bytes()).expect("validated path");
    let (report_rx, report_tx) = UnixStream::pair()
        .map_err(|e| (SandboxError::Io { context: "creating the launch report channel", source: e }, true))?;
    let plan = ChildPlan {
        merged: to_c(&layout.merged),
        mount_data,
        cwd: to_c(&spec.cwd),
        uid: Uid::from_raw(spec.run_as.uid),
        gid: Gid::from_raw(spec.run_as.gid),
        cap_last,
        parent: getpid(),
        report: report_tx,
    };

    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: `pre_exec`'s contract is that the closure runs in the forked child of a
    // possibly multithreaded parent, so it must restrict itself to async-signal-safe work.
    // `ChildPlan::run` makes raw syscalls on pre-built `CStr`s and stack buffers only, and
    // never allocates or takes a lock (module docs, "What runs between fork and exec").
    unsafe {
        command.pre_exec(move || plan.run());
    }

    let started = Instant::now();
    let spawned = command.spawn();
    // Drops the pre_exec closure and with it the parent's copy of the report channel's write
    // end; the child's copy closed at exec (or exit), so the read below sees EOF. A read
    // failure is not propagated with `?`: the child may be running, and a report without the
    // namespace record is handled below by tearing the run down.
    drop(command);
    let (ns_fd, report) = read_report(&report_rx);
    let (saw_ns, failed_step) = parse_report(&report);
    // The identity is read from the fd we now hold, so it stays this namespace's for as long
    // as the fd lives. Anything that is not a mount-namespace fd is treated as no fd at all,
    // which fails the launch below.
    let ns_fd = ns_fd.filter(|fd| saw_ns && is_mount_namespace(fd));
    let ns: Option<NsId> =
        ns_fd.as_ref().and_then(|fd| rustix::fs::fstat(fd).ok()).map(|st| (st.st_dev, st.st_ino));

    let mut child = match spawned {
        // The child died before exec, having forked nothing: nothing can be left running,
        // and its namespace (with the overlay mount) died with it.
        Err(source) => {
            let step = failed_step.and_then(|i| STEPS.get(usize::from(i)).copied());
            return Err((SandboxError::Launch { step, source }, true));
        }
        Ok(child) => child,
    };
    // From here the child exec'd the tool. It is reaped only through `pid` (never through the
    // std `Child`, which is dropped below without waiting), so it cannot be recycled while
    // the pidfd or the watchdog refers to it.
    let pid = Pid::from_child(&child);
    let stdio = SandboxStdio {
        stdin: child.stdin.take().expect("piped"),
        stdout: child.stdout.take().expect("piped"),
        stderr: child.stderr.take().expect("piped"),
    };
    drop(child);

    let mut ns_fd = ns_fd;
    let mut failed = |error: SandboxError, pidfd: Option<&OwnedFd>| {
        match pidfd {
            Some(fd) => {
                let _ = pidfd_send_signal(fd, Signal::KILL);
            }
            None => {
                let _ = rustix::process::kill_process(pid, Signal::KILL);
            }
        }
        let _ = sweep::reap(pid);
        let (residual, completed) = sweep::kill_run(ns, spec.run_as.ids());
        let report = teardown(ns, ns_fd.take(), spec.run_as, layout, residual, completed);
        (error, report.confirmed_empty)
    };

    let Some(ns) = ns else {
        return Err(failed(
            SandboxError::Launch {
                step: Some(STEPS[2]),
                source: io::Error::other(
                    "the child exec'd without handing back a mount-namespace fd",
                ),
            },
            None,
        ));
    };
    let pidfd = match pidfd_open(pid, PidfdFlags::empty()) {
        Ok(fd) => fd,
        Err(e) => {
            return Err(failed(SandboxError::Io { context: "pidfd_open", source: e.into() }, None));
        }
    };
    let watchdog_fd = match pidfd.try_clone() {
        Ok(fd) => fd,
        Err(e) => {
            return Err(failed(
                SandboxError::Io { context: "duplicating the pidfd", source: e },
                Some(&pidfd),
            ));
        }
    };
    let deadline = started + spec.timeout;
    let run_as = spec.run_as;
    let watchdog = std::thread::Builder::new()
        .name("sandbox-watchdog".into())
        .spawn(move || end_of_run(&watchdog_fd, pid, ns, run_as.ids(), deadline, started));
    let watchdog = match watchdog {
        Ok(handle) => handle,
        // No watchdog means no timeout; refuse to run without one (ADR-004).
        Err(source) => {
            return Err(failed(
                SandboxError::Io { context: "starting the timeout watchdog", source },
                Some(&pidfd),
            ));
        }
    };

    Ok((
        Sandbox {
            pid,
            pidfd,
            watchdog: Some(watchdog),
            ns,
            ns_fd,
            run_as,
            layout: layout.clone(),
            killed_on_request: false,
            finished: false,
            _uid_locks: uid_locks,
        },
        stdio,
    ))
}

/// Read the child's launch report to EOF: the bytes, and the namespace fd if one came with
/// them. Bounded; a read error ends the report (whatever arrived is still used).
fn read_report(rx: &UnixStream) -> (Option<OwnedFd>, Vec<u8>) {
    let mut bytes = Vec::new();
    let mut ns_fd = None;
    while bytes.len() < 64 {
        let mut buf = [0u8; 64];
        let mut space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let mut iov = [IoSliceMut::new(&mut buf)];
        match recvmsg(rx, &mut iov, &mut control, RecvFlags::CMSG_CLOEXEC) {
            Ok(r) if r.bytes == 0 => break,
            Ok(r) => {
                bytes.extend_from_slice(&buf[..r.bytes]);
                for msg in control.drain() {
                    if let RecvAncillaryMessage::ScmRights(fds) = msg {
                        for fd in fds {
                            ns_fd.get_or_insert(fd);
                        }
                    }
                }
            }
            Err(Errno::INTR) => {}
            Err(_) => break,
        }
    }
    (ns_fd, bytes)
}

/// `NS_GET_NSTYPE` from `<linux/nsfs.h>`: `_IO(0xb7, 0x3)`.
const NS_GET_NSTYPE: libc::c_ulong = 0xb703;

/// Whether `fd` is an nsfs fd for a **mount** namespace. Only nsfs files answer this ioctl, so
/// any other kind of fd fails it.
#[allow(unsafe_code)]
fn is_mount_namespace(fd: &OwnedFd) -> bool {
    // SAFETY: an argument-less ioctl on an fd we own; it reads and writes no user memory.
    let kind = unsafe { libc::ioctl(fd.as_raw_fd(), NS_GET_NSTYPE) };
    kind == libc::CLONE_NEWNS
}

/// Parse the report bytes: whether the namespace record arrived, and the failed step if any.
fn parse_report(report: &[u8]) -> (bool, Option<u8>) {
    let mut ns = false;
    let mut failed = None;
    let mut rest = report;
    while let Some((&tag, tail)) = rest.split_first() {
        match tag {
            TAG_NS => {
                ns = true;
                rest = tail;
            }
            TAG_FAILED if !tail.is_empty() => {
                failed = Some(tail[0]);
                rest = &tail[1..];
            }
            _ => break,
        }
    }
    (ns, failed)
}

/// What the watchdog observed at the end of the run.
struct EndOfRun {
    status: io::Result<ExitStatus>,
    timed_out: bool,
    wall_time: Duration,
    /// The run's processes alive after the direct child was reaped, before any killing.
    residual: Option<Scan>,
    sweep_completed: bool,
}

/// The watchdog: wait for the direct child to exit or the deadline; at the deadline kill
/// it. Then reap it and kill what is left of the run ([`sweep::kill_run`]), whichever way it
/// ended (module docs, "The timeout").
fn end_of_run(
    pidfd: &OwnedFd,
    pid: Pid,
    ns: NsId,
    ids: RunIds,
    deadline: Instant,
    started: Instant,
) -> EndOfRun {
    let exited = sweep::poll_until(pidfd, deadline);
    let timed_out = !exited && pidfd_send_signal(pidfd, Signal::KILL).is_ok();
    let status = sweep::reap(pid).map(ExitStatus::from_raw);
    let wall_time = started.elapsed();
    // Always swept, not only when the snapshot finds something: a snapshot of /proc is not
    // atomic, and the sweep is what is sound.
    let (residual, sweep_completed) = sweep::kill_run(Some(ns), ids);
    EndOfRun { status, timed_out, wall_time, residual, sweep_completed }
}

/// Final teardown, after [`sweep::kill_run`] has done the killing: one confirming scan, then
/// remove `work/` and `merged/` if and only if it confirmed nothing is left. `ns_fd` is the fd
/// pinning `ns` (`None` with it), held until that scan. `residual` and `sweep_completed` are
/// what `kill_run` returned.
///
/// The killing is no longer repeated here. A process that `kill_run`'s helper sweep, its
/// per-pid root kills and its own confirming scans all missed is not one more round from
/// being caught; it is reported, not chased (worst-case return time is `kill_run`'s).
fn teardown(
    ns: Option<NsId>,
    ns_fd: Option<OwnedFd>,
    run_as: RunAs,
    layout: &Layout,
    residual: Option<Scan>,
    mut sweep_completed: bool,
) -> TeardownReport {
    let last = sweep::scan(ns, run_as.uid);
    if last.is_none() || residual.is_none() {
        // An unreadable /proc can never count as clean.
        sweep_completed = false;
    }
    let residual = residual.unwrap_or_default();
    let confirmed_empty = last.as_ref().is_some_and(Scan::is_empty);
    // The scans are done, so the identity no longer needs pinning. Released before the
    // scratch directories go, so the now process-less namespace and its overlay mount go
    // first.
    drop(ns_fd);
    let (scratch_removed, removal_error) = if confirmed_empty {
        match fs::remove_dir_all(&layout.work).and_then(|()| fs::remove_dir(&layout.merged)) {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        }
    } else {
        (false, None)
    };
    let mut unverified_pids: Vec<i32> = residual
        .unverifiable
        .iter()
        .chain(last.as_ref().map_or(&[][..], |s| &s.unverifiable[..]))
        .copied()
        .collect();
    unverified_pids.sort_unstable();
    unverified_pids.dedup();
    TeardownReport {
        residual_pids: residual.all(),
        residual_outside_namespace: residual.outside_namespace(),
        unverified_pids,
        uid_sweep_completed: sweep_completed,
        survivors: last.map(|s| s.all()).unwrap_or_default(),
        confirmed_empty,
        scratch_removed,
        removal_error,
    }
}

/// A running sandboxed process. Holds the run's uid lock until it is dropped. Dropping it
/// without calling [`wait`](Self::wait) or [`kill_and_wait`](Self::kill_and_wait) kills the
/// run and tears down, discarding the record.
#[derive(Debug)]
pub struct Sandbox {
    pid: Pid,
    pidfd: OwnedFd,
    watchdog: Option<JoinHandle<EndOfRun>>,
    ns: NsId,
    /// Pins the run's mount namespace so `ns` cannot be reused by another namespace.
    /// Released by teardown, after its final scan.
    ns_fd: Option<OwnedFd>,
    run_as: RunAs,
    layout: Layout,
    killed_on_request: bool,
    finished: bool,
    /// The run uid's and the helper uid's locks. Released after teardown, when the struct is
    /// dropped (fields drop after `Drop::drop`).
    _uid_locks: Vec<File>,
}

impl fmt::Debug for EndOfRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EndOfRun").field("timed_out", &self.timed_out).finish_non_exhaustive()
    }
}

impl Sandbox {
    /// The sandboxed process's pid in the host's PID namespace.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid.as_raw_nonzero().get().unsigned_abs()
    }

    /// Where the upper layer is (and will remain after teardown).
    #[must_use]
    pub fn upper(&self) -> &Path {
        &self.layout.upper
    }

    /// Wait for the run to end — the direct child exiting on its own, or the timeout — then
    /// tear down. Drop [`SandboxStdio::stdin`] first if the program waits for EOF.
    ///
    /// # Errors
    ///
    /// [`SandboxError::Supervision`] if the watchdog died or reaping the direct child failed.
    /// Teardown runs on every path, including these and `Drop`; the error carries its report.
    pub fn wait(mut self) -> Result<RunRecord, SandboxError> {
        self.finish()
    }

    /// `SIGKILL` the direct child now (e.g. the host-side client is done with the server);
    /// the rest of the run is swept as for any other end. Recorded as
    /// [`RunRecord::killed_on_request`], distinct from a timeout.
    ///
    /// # Errors
    ///
    /// As for [`wait`](Self::wait).
    pub fn kill_and_wait(mut self) -> Result<RunRecord, SandboxError> {
        self.killed_on_request = pidfd_send_signal(&self.pidfd, Signal::KILL).is_ok();
        self.finish()
    }

    fn finish(&mut self) -> Result<RunRecord, SandboxError> {
        self.finished = true;
        let end = self.watchdog.take().map(JoinHandle::join);
        let Some(Ok(end)) = end else {
            // The watchdog died. Make sure the child is dead and reaped — through the pidfd, so
            // that if the watchdog had already reaped it this fails with ECHILD instead of
            // reaping some other child that inherited the pid — then tear down regardless.
            let _ = pidfd_send_signal(&self.pidfd, Signal::KILL);
            let _ = rustix::process::waitid(
                rustix::process::WaitId::PidFd(self.pidfd.as_fd()),
                rustix::process::WaitIdOptions::EXITED,
            );
            let (residual, completed) = sweep::kill_run(Some(self.ns), self.run_as.ids());
            let teardown = teardown(
                Some(self.ns),
                self.ns_fd.take(),
                self.run_as,
                &self.layout,
                residual,
                completed,
            );
            return Err(SandboxError::Supervision {
                what: "the timeout watchdog thread panicked",
                teardown: Box::new(teardown),
            });
        };
        let teardown = teardown(
            Some(self.ns),
            self.ns_fd.take(),
            self.run_as,
            &self.layout,
            end.residual,
            end.sweep_completed,
        );
        let Ok(status) = end.status else {
            return Err(SandboxError::Supervision {
                what: "waitpid on the direct child failed",
                teardown: Box::new(teardown),
            });
        };
        Ok(RunRecord {
            upper: self.layout.upper.clone(),
            status,
            timed_out: end.timed_out,
            killed_on_request: self.killed_on_request,
            wall_time: end.wall_time,
            teardown,
        })
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if !self.finished {
            let _ = pidfd_send_signal(&self.pidfd, Signal::KILL);
            let _ = self.finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(lower: &Path, scratch: &Path) -> SandboxSpec {
        SandboxSpec {
            lower: lower.to_path_buf(),
            scratch: scratch.to_path_buf(),
            program: PathBuf::from("/bin/sh"),
            args: vec![],
            env: vec![],
            cwd: PathBuf::from("/"),
            run_as: RunAs::DEFAULT,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn launch_report_parses_both_records_and_tolerates_truncation() {
        assert_eq!(parse_report(&[TAG_NS, TAG_FAILED, 3]), (true, Some(3)));
        assert_eq!(parse_report(&[TAG_FAILED, 0]), (false, Some(0)));
        assert_eq!(parse_report(&[TAG_FAILED]), (false, None));
        assert_eq!(parse_report(&[b'?', TAG_NS]), (false, None));
        assert_eq!(parse_report(&[]), (false, None));
        assert_eq!(STEPS[3], "mount overlay");
    }

    #[test]
    fn securebits_lock_the_setuid_fixup_and_keep_caps_off() {
        assert_eq!(SECUREBITS & libc::SECBIT_NO_SETUID_FIXUP as libc::c_ulong, 0);
        assert_eq!(SECUREBITS & libc::SECBIT_KEEP_CAPS as libc::c_ulong, 0);
        for bit in [libc::SECBIT_NO_SETUID_FIXUP_LOCKED, libc::SECBIT_KEEP_CAPS_LOCKED, libc::SECBIT_NOROOT] {
            assert_ne!(SECUREBITS & bit as libc::c_ulong, 0);
        }
    }

    #[test]
    fn spec_validation_runs_before_any_privilege_check() {
        let lower = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let scratch = parent.path().join("run");

        for run_as in [
            RunAs { uid: 0, gid: RunAs::DEFAULT.gid },
            RunAs { uid: 1000, gid: RunAs::DEFAULT.gid },
            RunAs { uid: 65_534, gid: RunAs::DEFAULT.gid },
            RunAs { uid: RunAs::DEFAULT.uid, gid: 100 },
            RunAs { uid: u32::MAX - 1, gid: RunAs::DEFAULT.gid },
            RunAs { uid: u32::MAX, gid: RunAs::DEFAULT.gid },
            RunAs { uid: RunAs::DEFAULT.uid, gid: u32::MAX },
        ] {
            let mut s = spec(lower.path(), &scratch);
            s.run_as = run_as;
            assert!(matches!(spawn(&s), Err(SandboxError::InvalidSpec(_))), "{run_as:?}");
        }

        let mut s = spec(lower.path(), &scratch);
        s.timeout = Duration::ZERO;
        assert!(matches!(spawn(&s), Err(SandboxError::InvalidSpec(_))));

        let mut s = spec(lower.path(), &scratch);
        s.program = PathBuf::from("sh");
        assert!(matches!(
            spawn(&s),
            Err(SandboxError::UnsafePath { what: "program", reason: PathRejection::NotAbsolute, .. })
        ));

        let mut s = spec(lower.path(), &scratch);
        s.env = vec![("A=B".into(), "c".into())];
        assert!(matches!(spawn(&s), Err(SandboxError::InvalidSpec(_))));

        // Nothing was created by any rejected spec.
        assert!(!scratch.exists());
    }

    #[test]
    fn scratch_inside_lower_is_refused_as_overlapping() {
        let lower = tempfile::tempdir().unwrap();
        let s = spec(lower.path(), &lower.path().join("run"));
        assert!(matches!(spawn(&s), Err(SandboxError::LayersOverlap { .. })));
        assert!(!lower.path().join("run").exists());
    }

    #[test]
    fn an_existing_scratch_directory_is_refused() {
        let lower = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let s = spec(lower.path(), scratch.path());
        assert!(matches!(spawn(&s), Err(SandboxError::ScratchExists(_))));
    }

    #[test]
    fn a_non_root_supervisor_is_refused_without_side_effects() {
        // Meaningful only when not root; as root (the VM's `sudo` run) spawn would proceed
        // and fail on the missing /bin/sh, which the integration tests cover.
        if geteuid().is_root() {
            return;
        }
        let lower = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let scratch = parent.path().join("run");
        assert!(matches!(spawn(&spec(lower.path(), &scratch)), Err(SandboxError::NotPrivileged)));
        assert!(!scratch.exists());
    }
}
