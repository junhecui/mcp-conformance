//! Finding and killing every process belonging to a run, and keeping a run's ids exclusive.
//!
//! # Why the uid, not only the mount namespace
//!
//! Until P2-01 there is no PID namespace, so "the run's processes" has no kernel-maintained
//! boundary. The first version of teardown identified them by mount namespace. That is
//! evadable: an unprivileged process can `unshare(CLONE_NEWUSER | CLONE_NEWNS)` (Ubuntu's
//! `apparmor_restrict_unprivileged_userns=1` does not stop the creation) and so acquire a
//! mount namespace of its own, invisible to a namespace scan — demonstrated in review, with
//! the escapee still writing the upper layer seconds after a "clean" record was returned.
//!
//! What the process cannot shed is its host uid. Credentials are fixed by the drop to
//! [`RunAs`](crate::RunAs) with every capability and the bounding set gone and
//! `no_new_privs` set; a user namespace changes how ids *read* inside it, not the kernel uid
//! the host sees. So, provided the uid belongs to exactly one live run ([`lock_uid`]), every
//! process under it is that run's. [`kill_run`] ends them in two complementary ways:
//!
//! * **The helper sweep** ([`uid_sweep`]): a forked helper loops `kill(-1, SIGKILL)` with
//!   `ruid = suid = helper uid` and `euid = run uid`. `kill(-1)` signals every process the
//!   caller may signal in one pass under the kernel's task-list lock, during which no fork can
//!   complete, and a pending `SIGKILL` aborts any fork in flight — a fork-and-exit chain that
//!   outruns a scan-and-kill loop cannot outrun that. The split credentials are what make the
//!   helper **unsignalable by the run**: the kernel lets a sender signal a target when the
//!   sender's real or effective uid equals the target's real or saved uid, so the helper's
//!   euid reaches the run's processes, while their uid matches neither the helper's real nor
//!   its saved uid — `kill`/`SIGSTOP` against it fail with `EPERM` (checked in the pinned guest;
//!   ADR-016). An earlier version ran the helper as the run uid, and a run process looping
//!   `kill(-1, SIGKILL)` could kill each helper as it started and outlive teardown. The helper
//!   uid is the run uid + 1, validated and locked together with it, so two runs never share
//!   one — each helper's `kill(-1)` also reaches processes whose real or saved uid is *its*
//!   real uid, which would otherwise mean other runs' helpers.
//!
//!   The helper cannot tell when it is done: Linux's `kill(-1)` counts every process it
//!   iterates, permitted or not, so it returns `ESRCH` only on a system with nothing else
//!   running. The **supervisor's** two consecutive empty `/proc` scans, 10 ms apart, are the
//!   stop condition; the killing is the helper's atomic passes.
//! * **Root, per pid**: after the helper, every process the scan still finds under the run uid
//!   is `SIGKILL`ed by the root supervisor through a pidfd ([`kill_if_uid`]), which no run
//!   process can resist or block.
//!
//! [`scan`] reports both views: processes still in the run's mount namespace, and processes
//! whose real, effective, saved or filesystem uid is the run's.
//!
//! What remains until P2-01 (PID namespace: the kernel kills the whole tree with its PID 1) and
//! P4-01 (seccomp denying `CLONE_NEWUSER`): this is a sweep, not a boundary. Run processes
//! other than the direct child run until the sweep reaches them, and can create nested
//! namespaces. The scans are not atomic: `/proc` is listed in batches, in pid order
//! (`next_tgid`). A forward fork-and-exit chain cannot hide from that: each child's pid is
//! above every pid allocated before it, so it lands at or ahead of the listing's cursor, and
//! the listing keeps finding a live member until the chain is dead. A miss needs a pid
//! allocated *below* the cursor. That is cyclic allocation after the allocator has wrapped,
//! which a host does on its own over time and a run can force with ~`pid_max` forks (4,194,304
//! on the pinned image). After a wrap, a chain can be missed by every scan. The helper still
//! kills it — its passes do not depend on the scan, and an empty scan only counts once two
//! passes have completed after it — but `residual_pids` can then come out empty for a run that
//! did have descendants. Hence the `proc_scan` basis P1-05 must carry (ADR-016); P2-02's cgroup
//! (`pids.current`, `cgroup.kill`) is the real fix. A process that forks faster than everything above (or exhausts pids so the
//! helper cannot fork) can defeat the kill; that ends with `uid_sweep_completed: false` or a
//! non-empty survivor list, never a clean record, after a time bounded by a
//! count of work, not a wall time: about two helper attempts of up to 5 s each (twice, if the first sweep completed) plus up to ~51 `/proc` scans, and each scan is O(processes) — which, with no cgroups yet, only `RLIMIT_NPROC` bounds.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;
use rustix::process::{
    Gid, Pid, PidfdFlags, Signal, Uid, WaitOptions, pidfd_open, pidfd_send_signal, waitpid,
};
use rustix::thread::{set_thread_groups, set_thread_res_gid, set_thread_res_uid};

/// The identity of a mount namespace: `(st_dev, st_ino)` of its nsfs inode. Meaningful only
/// while something pins the namespace (the supervisor holds an fd to it): nsfs inode numbers
/// are reused once a namespace dies.
pub(crate) type NsId = (u64, u64);

/// The ids one run owns: the run uid/gid and the helper uid (`run + 1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunIds {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) helper_uid: u32,
}

/// One snapshot of a run's live processes, by both identifications. Sorted pids.
///
/// **Invariant: a scan may report false positives, never a false empty.** Anything that could
/// hide a process — a `/proc` entry that cannot be listed, a status or task that cannot be
/// read or parsed, a namespace link of a live task that cannot be resolved, for any reason but
/// "it no longer exists" (`ENOENT`/`ESRCH`) — either fails the whole scan (`None`) or lands the
/// pid in [`Scan::unverifiable`], which counts toward [`Scan::is_empty`] and [`Scan::all`].
/// Each site's choice is documented where it is made. Three consecutive review rounds found a
/// scan that failed *open* (a nested namespace, a zombie leader, a non-UTF-8 `comm`); this is
/// the rule that replaces patching them one at a time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Scan {
    pub(crate) in_namespace: Vec<i32>,
    pub(crate) by_uid: Vec<i32>,
    /// Pids that exist but whose membership could not be established either way. Counted as
    /// present (never as absent), never killed (root kills only what it has verified).
    pub(crate) unverifiable: Vec<i32>,
}

impl Scan {
    pub(crate) fn is_empty(&self) -> bool {
        self.in_namespace.is_empty() && self.by_uid.is_empty() && self.unverifiable.is_empty()
    }

    /// Every pid in any view, unverifiable ones included, sorted and deduplicated.
    pub(crate) fn all(&self) -> Vec<i32> {
        let mut v: Vec<i32> = self
            .in_namespace
            .iter()
            .chain(&self.by_uid)
            .chain(&self.unverifiable)
            .copied()
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Run processes that are *not* in the run's mount namespace: they left it.
    pub(crate) fn outside_namespace(&self) -> Vec<i32> {
        self.by_uid.iter().copied().filter(|p| !self.in_namespace.contains(p)).collect()
    }
}

/// Whether a `/proc` error means "that process or task no longer exists". Everything else is
/// an unknown, and unknowns fail closed.
fn is_gone(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ESRCH))
}

fn ns_at(path: &Path) -> io::Result<NsId> {
    let m = fs::metadata(path)?;
    Ok((m.dev(), m.ino()))
}

/// What a `/proc/<pid>/status` or `/proc/<pid>/task/<tid>/status` body says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StatusFacts {
    /// `State:` is `Z` (zombie) or `X` (dead).
    pub(crate) exited: bool,
    /// `Threads:` — the group's thread count as read under the task's sighand lock, and `0`
    /// when that lock cannot be taken (a released task; see [`classify_leader`]).
    pub(crate) threads: u32,
    /// The four `Uid:` fields (real, effective, saved, filesystem).
    pub(crate) uids: [u32; 4],
}

/// Parse a status body **as bytes**, strictly. `None` — malformed, fail closed — unless there
/// is exactly one `State:` line with a state letter, exactly one `Threads:` line with a number,
/// and exactly one `Uid:` line with exactly four numbers.
///
/// Bytes, not `str`: the kernel escapes `comm` in the `Name:` line with only
/// `ESCAPE_SPACE | ESCAPE_SPECIAL`, so bytes `0x80..=0xFF` come through raw, and any process
/// can set them (`prctl(PR_SET_NAME, "\xff")`, or exec a file so named). A UTF-8 read of such a
/// status fails, and the round-3 scanner read that failure as "no process" — a false clean
/// (round-4 review). The `Name:` line's content is never interpreted; lines are matched by
/// byte prefix, and `comm` cannot contain a raw newline (`ESCAPE_SPECIAL` escapes it), so it
/// cannot forge another line. Pure, for unit testing.
pub(crate) fn parse_status(status: &[u8]) -> Option<StatusFacts> {
    fn numbers(rest: &[u8]) -> Option<Vec<u32>> {
        rest.split(u8::is_ascii_whitespace)
            .filter(|f| !f.is_empty())
            .map(|f| std::str::from_utf8(f).ok()?.parse().ok())
            .collect()
    }
    let (mut exited, mut threads, mut uids) = (None, None, None);
    for line in status.split(|&b| b == b'\n') {
        if let Some(rest) = line.strip_prefix(b"State:") {
            let letter = rest.iter().find(|b| !b.is_ascii_whitespace())?;
            if exited.replace(matches!(letter, b'Z' | b'X')).is_some() {
                return None;
            }
        } else if let Some(rest) = line.strip_prefix(b"Threads:") {
            let n = numbers(rest)?;
            if n.len() != 1 || threads.replace(n[0]).is_some() {
                return None;
            }
        } else if let Some(rest) = line.strip_prefix(b"Uid:") {
            let n: [u32; 4] = numbers(rest)?.try_into().ok()?;
            if uids.replace(n).is_some() {
                return None;
            }
        }
    }
    Some(StatusFacts { exited: exited?, threads: threads?, uids: uids? })
}

/// What a thread-group leader's status says about its process. See [`classify_leader`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Leader {
    /// `Threads: 0`, whatever `State:` says: a stale, released leader. The process is alive
    /// under this pid. **Present**, by the uids the stale leader shows.
    Stale,
    /// `State: Z`/`X` with `Threads: 1`: a real single-threaded zombie. It has exited.
    Zombie,
    /// Live, `Threads: 1`: judged by the leader alone.
    Single,
    /// `Threads > 1`: judged by any live task. The leader itself may be a zombie.
    Multi,
}

/// **Threads semantics.** `Threads:` is the group's `nr_threads`, printed only if the kernel
/// can take the task's sighand lock, and `0` otherwise. That makes the leader's count decisive:
///
/// * **A `Z` (or `X`) leader with `Threads: 1` is a real zombie**: every thread has exited, and
///   only the unreaped leader remains. Not ours.
/// * **A `Z` leader with `Threads: > 1` is alive** (round 3): a process can end its main
///   thread with a raw `SYS_exit` and keep other threads running.
/// * **`Threads: 0` is a stale leader and counts as present** (round 5). After the main
///   thread `SYS_exit`s, a surviving thread that `execve`s runs `de_thread`. That swaps the
///   thread onto the tgid and `release_task`s the old leader, whose sighand is then gone. A
///   `/proc/<pid>/status` read that took its reference before the swap prints the released
///   leader as `State: Z` or `X` with `Threads: 0`, while the process lives on under the same
///   pid. Reading that as "exited" was a false empty, which the round-5 review reproduced on
///   about 0.1% of reads. A genuine zombie never reads 0: `__exit_signal` decrements
///   `nr_threads` and clears `sighand` under the same lock, so a reader sees either the
///   count including the leader or no lock at all.
///
/// Pure, so unit tests pin the rule [`process_view`] applies.
pub(crate) fn classify_leader(f: &StatusFacts) -> Leader {
    match (f.threads, f.exited) {
        (0, _) => Leader::Stale,
        (1, true) => Leader::Zombie,
        (1, false) => Leader::Single,
        _ => Leader::Multi,
    }
}

/// Whether a *process* status body (the thread-group leader's) counts toward `uid` on its own;
/// `None` if it is malformed. Everything but a real zombie ([`Leader::Zombie`]) counts by the
/// uids it shows. Test-only: the production path is [`process_view`], which applies the same
/// [`classify_leader`] rule and, for a multithreaded process, also walks its tasks.
#[cfg(test)]
pub(crate) fn status_matches_uid(status: &[u8], uid: u32) -> Option<bool> {
    let f = parse_status(status)?;
    Some(classify_leader(&f) != Leader::Zombie && f.uids.contains(&uid))
}

/// What one `/proc/<pid>` says about the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    /// It no longer exists (`ENOENT`/`ESRCH` on its own entry).
    Gone,
    /// Established, either way.
    Known { in_namespace: bool, by_uid: bool },
    /// It exists, but something about it could not be read or parsed. Fails closed.
    Unverifiable,
}

/// Classify process `pid` against the run.
///
/// The leader's status decides how, by [`classify_leader`] (its doc has the `Threads:`
/// semantics):
///
/// * [`Leader::Stale`] (`Threads: 0`) → PRESENT, by the uids the stale leader still shows.
///   It is never gone; the round-5 review reproduced a false clean here. `ns/mnt` is the dead
///   leader's link, so it is not resolved.
/// * [`Leader::Zombie`] (exited, `Threads: 1`) → a real zombie, not ours.
/// * [`Leader::Single`] → judged by the leader.
/// * [`Leader::Multi`] → judged by **any live task** under `task/`: the leader may be a zombie
///   fronting live threads (whose `ns/mnt` then does not resolve), threads need not share the
///   leader's mount namespace, and a thread can have its own `comm`; the leader's own uid still
///   counts whatever the tasks say.
///
/// Every read is fail-closed: `ENOENT`/`ESRCH` on the process entry means [`View::Gone`]; on
/// the `task/` directory it means the process died, but the leader's uid view is kept; on a
/// task or a task's namespace link it means that task exited (skipped, leader's view kept); any
/// other error, and any malformed status, makes the process [`View::Unverifiable`].
fn process_view(pid: i32, ns: Option<NsId>, uid: u32) -> View {
    let dir = PathBuf::from(format!("/proc/{pid}"));
    let status = match fs::read(dir.join("status")) {
        Ok(bytes) => bytes,
        Err(e) if is_gone(&e) => return View::Gone,
        Err(_) => return View::Unverifiable,
    };
    let Some(leader) = parse_status(&status) else { return View::Unverifiable };
    let mut by_uid = leader.uids.contains(&uid);
    let mut in_namespace = false;
    match classify_leader(&leader) {
        // A released leader: the process lives on under this pid (round 5). Present by the
        // uids it shows; its `ns/mnt` is the dead leader's, so not resolved.
        Leader::Stale => return View::Known { in_namespace: false, by_uid },
        // A real, single-threaded zombie: it has exited and holds nothing. Not ours.
        Leader::Zombie => return View::Known { in_namespace: false, by_uid: false },
        Leader::Single => {
            if let Some(ns) = ns {
                match ns_at(&dir.join("ns/mnt")) {
                    Ok(id) => in_namespace = id == ns,
                    // Exiting between the two reads: the uid view keeps what the status said.
                    Err(e) if is_gone(&e) => {}
                    Err(_) => return View::Unverifiable,
                }
            }
            return View::Known { in_namespace, by_uid };
        }
        Leader::Multi => {}
    }
    let tasks = match fs::read_dir(dir.join("task")) {
        Ok(tasks) => tasks,
        // The process died, but the leader showed the uid; per "a leader that shows the uid
        // counts" (and fail-closed), keep that rather than discarding it as gone.
        Err(e) if is_gone(&e) => return View::Known { in_namespace, by_uid },
        Err(_) => return View::Unverifiable,
    };
    for task in tasks {
        let Ok(task) = task else { return View::Unverifiable };
        let status = match fs::read(task.path().join("status")) {
            Ok(bytes) => bytes,
            Err(e) if is_gone(&e) => continue,
            Err(_) => return View::Unverifiable,
        };
        let Some(t) = parse_status(&status) else { return View::Unverifiable };
        if t.exited {
            continue;
        }
        by_uid |= t.uids.contains(&uid);
        if let Some(ns) = ns {
            match ns_at(&task.path().join("ns/mnt")) {
                Ok(id) => in_namespace |= id == ns,
                Err(e) if is_gone(&e) => {}
                Err(_) => return View::Unverifiable,
            }
        }
    }
    View::Known { in_namespace, by_uid }
}

/// Snapshot the run's processes, or `None` if `/proc` cannot be listed — including an error
/// on any single directory entry (fail closed: a listing with a hole in it is not a scan).
/// Entries whose name is not a pid are not processes and are skipped.
///
/// Not atomic: `/proc` is listed in batches, in pid order. A forward fork chain stays at or
/// ahead of the cursor and is always found. A miss needs a pid allocated below the cursor,
/// which is cyclic allocation after a pid wrap (the module doc has the detail). The helper's
/// `kill(-1)` still kills such a chain — it does not rely on the scan — but `residual_pids` can
/// then come out empty: until P2-02's cgroup (`pids.current`, `cgroup.kill`), an empty scan
/// means "no run process was observed", not "none existed".
pub(crate) fn scan(ns: Option<NsId>, uid: u32) -> Option<Scan> {
    let mut out = Scan::default();
    for entry in fs::read_dir("/proc").ok()? {
        let entry = entry.ok()?;
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        match process_view(pid, ns, uid) {
            View::Gone => {}
            View::Known { in_namespace, by_uid } => {
                if in_namespace {
                    out.in_namespace.push(pid);
                }
                if by_uid {
                    out.by_uid.push(pid);
                }
            }
            View::Unverifiable => out.unverifiable.push(pid),
        }
    }
    out.in_namespace.sort_unstable();
    out.by_uid.sort_unstable();
    out.unverifiable.sort_unstable();
    Some(out)
}

/// `SIGKILL` `pid` from root if it is **verified** to still run under `uid`.
///
/// Fail-closed counting never widens what root kills: an [`View::Unverifiable`] pid is counted
/// as present by [`scan`] but is not signalled here — killing something root could not verify as
/// the run's could kill an unrelated host process.
///
/// Pid reuse: the pidfd is opened first and refers to whichever process held `pid` at that
/// instant; the uid is then re-read from `/proc/<pid>`. While the pidfd's process lives (even as
/// a zombie) its pid cannot be reused, so a matching `/proc` entry *is* that process. If it died
/// between open and re-read, the signal goes to a dead process and fails harmlessly with
/// `ESRCH` — it can never reach whatever reused the pid.
pub(crate) fn kill_if_uid(pid: i32, uid: u32) {
    let Some(raw) = Pid::from_raw(pid) else { return };
    let Ok(fd) = pidfd_open(raw, PidfdFlags::empty()) else { return };
    // `process_view` sees a zombie leader with live threads; the pidfd (opened on the leader,
    // which exists until the whole group is gone) delivers a group-wide SIGKILL.
    if matches!(process_view(pid, None, uid), View::Known { by_uid: true, .. }) {
        let _ = pidfd_send_signal(&fd, Signal::KILL);
    }
}

/// Poll `pidfd` until the process exits (`true`) or `deadline` passes (`false`). An
/// unexpected poll failure returns `false`, so callers fail toward killing.
pub(crate) fn poll_until(pidfd: &OwnedFd, deadline: Instant) -> bool {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let remaining = deadline - now;
        let timeout = Timespec {
            tv_sec: i64::try_from(remaining.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(remaining.subsec_nanos()),
        };
        let mut fds = [PollFd::new(pidfd, PollFlags::IN)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) | Err(Errno::INTR) => {}
            Ok(_) => return true,
            Err(_) => return false,
        }
    }
}

/// Whether the process behind `pidfd` has exited, without blocking. A poll failure counts
/// as exited, so callers stop relying on that process.
pub(crate) fn has_exited(pidfd: &OwnedFd) -> bool {
    let zero = Timespec { tv_sec: 0, tv_nsec: 0 };
    let mut fds = [PollFd::new(pidfd, PollFlags::IN)];
    !matches!(poll(&mut fds, Some(&zero)), Ok(0))
}

/// Reap `pid` (our own unreaped child), retrying on `EINTR`.
pub(crate) fn reap(pid: Pid) -> io::Result<i32> {
    loop {
        match waitpid(Some(pid), WaitOptions::empty()) {
            Ok(Some((_, status))) => return Ok(status.as_raw()),
            Ok(None) => return Err(io::Error::other("waitpid returned no status")),
            Err(Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

const HELPER_ATTEMPTS: usize = 2;
/// How long one helper may run before the supervisor gives up on it.
const HELPER_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause between the supervisor's completion scans.
const SCAN_PAUSE: Duration = Duration::from_millis(10);
/// The helper's pause between `kill(-1)` passes. Each pass already reaches every process of
/// the uid that exists, so pausing loses nothing except CPU burned.
const HELPER_PASS_PAUSE_NS: libc::c_long = 1_000_000;
/// The helper's own lifetime cap, in passes (≈ a minute at one pass per millisecond): a
/// backstop in case the supervisor dies without killing it (its parent-death signal is the
/// primary one).
const HELPER_MAX_PASSES: u32 = 60_000;
/// Root follow-up rounds after the helper, and the pause between them.
const ROOT_ROUNDS: usize = 50;
const ROOT_PAUSE: Duration = Duration::from_millis(20);

/// End everything left of a run: snapshot it, run the helper sweep, then kill any stragglers
/// from root per pid until a scan finds nothing (bounded). At most one further helper sweep is
/// tried, and only if every sweep so far completed — once one has failed (the run is fighting
/// it), the root kills are what remains. The worst case is bounded by a count of work, not a
/// wall time: two helper attempts of up to 5 s (twice, if the first sweep completed), plus up to
/// ~51 scans with 20 ms pauses, and each scan is O(processes) — which, with no cgroups yet, only
/// `RLIMIT_NPROC` bounds.
///
/// Returns the snapshot taken before any killing (`None` if `/proc` was unreadable) and whether
/// every helper sweep completed (false also if any scan failed).
pub(crate) fn kill_run(ns: Option<NsId>, ids: RunIds) -> (Option<Scan>, bool) {
    let residual = scan(ns, ids.uid);
    let mut completed = uid_sweep(ids) && residual.is_some();
    let mut swept_again = false;
    for round in 0..=ROOT_ROUNDS {
        let Some(s) = scan(ns, ids.uid) else {
            completed = false;
            break;
        };
        if s.is_empty() || round == ROOT_ROUNDS {
            break;
        }
        for &pid in &s.by_uid {
            kill_if_uid(pid, ids.uid);
        }
        if completed && !swept_again {
            swept_again = true;
            completed = uid_sweep(ids);
        }
        std::thread::sleep(ROOT_PAUSE);
    }
    (residual, completed)
}

/// Kill every process running under `ids.uid` with a helper (module docs). Returns whether the
/// supervisor confirmed the uid empty twice (see [`supervise_helper`]) within a helper's time
/// bound, trying up to [`HELPER_ATTEMPTS`] helpers.
#[allow(unsafe_code)]
pub(crate) fn uid_sweep(ids: RunIds) -> bool {
    let parent = rustix::process::getpid();
    for _ in 0..HELPER_ATTEMPTS {
        // The helper reports each completed kill(-1) pass as one byte. Non-blocking on both
        // ends: a full buffer must never wedge the helper, and the supervisor polls.
        let Ok((passes_rx, passes_tx)) = UnixStream::pair() else { return false };
        if passes_rx.set_nonblocking(true).is_err() || passes_tx.set_nonblocking(true).is_err() {
            return false;
        }
        let report_fd = passes_tx.as_raw_fd();
        // SAFETY: `fork` in a multithreaded process is sound provided the child restricts
        // itself to async-signal-safe operations until it exits. `sweep_as_child` makes raw
        // syscalls only (rustix's raw-syscall credential setters, prctl and capget,
        // `close_range`, `kill`, `write`, `nanosleep`), never allocates, and ends in `_exit`,
        // which runs no destructors or atexit handlers.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            let code = sweep_as_child(ids, parent, report_fd);
            // SAFETY: see above; `_exit` is async-signal-safe.
            unsafe { libc::_exit(code) }
        }
        drop(passes_tx);
        if pid < 0 {
            return false;
        }
        if supervise_helper(pid, ids.uid, &passes_rx) {
            return true;
        }
    }
    false
}

/// The helper's body: lock the securebits, take `ruid = suid = helper uid, euid = run uid`,
/// **verify it holds no capability**, then `kill(-1, SIGKILL)` every millisecond until stopped,
/// writing one byte to `report_fd` after each completed pass. Exits without killing anything on
/// failure (2: credential change failed; 3: the supervisor is gone; 4: securebits could not be
/// set; 5: capabilities survived the change) or at the pass cap (1); normally the supervisor
/// `SIGKILL`s it.
///
/// The capability check is load-bearing, not hygiene: if the supervisor had inherited
/// `SECBIT_NO_SETUID_FIXUP` (systemd `SecureBits=`, some container runtimes), `setresuid` from
/// root would *keep* the effective set, and a `kill(-1)` with `CAP_KILL` signals every process
/// on the host. Setting the same locked securebits as the run child first makes that fail
/// loudly (code 4) where it can, and the `capget` afterwards catches anything else (code 5).
#[allow(unsafe_code)]
fn sweep_as_child(ids: RunIds, parent: Pid, report_fd: i32) -> i32 {
    // A plain fork never execs, so CLOEXEC does not help: without this the helper would hold
    // every descriptor the supervisor has open — other runs' uid locks (keeping those flocks
    // held) and other sandboxes' stdio pipes (delaying their EOF). It keeps only `report_fd`.
    // SAFETY: raw syscalls with integer arguments only. A `first > last` range (report_fd 0)
    // fails with EINVAL and closes nothing, which is what that case needs.
    unsafe {
        let fd = u32::try_from(report_fd).unwrap_or(0);
        if fd > 0 {
            libc::syscall(libc::SYS_close_range, 0_u32, fd - 1, 0_u32);
        }
        libc::syscall(libc::SYS_close_range, fd + 1, u32::MAX, 0_u32);
    }
    // SAFETY: `prctl` with integer arguments only.
    let rc = unsafe {
        libc::prctl(libc::PR_SET_SECUREBITS, crate::supervisor::SECUREBITS, 0_u64, 0_u64, 0_u64)
    };
    if rc != 0 {
        return 4;
    }
    let (run, helper, gid) =
        (Uid::from_raw(ids.uid), Uid::from_raw(ids.helper_uid), Gid::from_raw(ids.gid));
    if set_thread_groups(&[]).is_err()
        || set_thread_res_gid(gid, gid, gid).is_err()
        || set_thread_res_uid(helper, run, helper).is_err()
    {
        return 2;
    }
    match rustix::thread::capabilities(None) {
        Ok(c) if c.effective.is_empty() && c.permitted.is_empty() => {}
        _ => return 5,
    }
    // Armed after the credential change, which clears it.
    if rustix::process::set_parent_process_death_signal(Some(Signal::KILL)).is_err()
        || rustix::process::getppid() != Some(parent)
    {
        return 3;
    }
    let pause = libc::timespec { tv_sec: 0, tv_nsec: HELPER_PASS_PAUSE_NS };
    let byte = [1_u8];
    for _ in 0..HELPER_MAX_PASSES {
        // SAFETY: `kill` takes integers; `write` reads one byte from a stack array into a
        // non-blocking fd (EAGAIN on a full buffer is ignored — the supervisor drains it);
        // `nanosleep` reads a stack `timespec` and is given a null remainder pointer. All are
        // async-signal-safe.
        unsafe {
            libc::kill(-1, libc::SIGKILL);
            libc::write(report_fd, byte.as_ptr().cast(), 1);
            libc::nanosleep(&pause, std::ptr::null_mut());
        }
    }
    1
}

/// Wait until `n` more completed kill passes are reported on `passes`, counting only bytes
/// that arrive after this call drains what was already there. `false` on the deadline, or if
/// the helper's end closed (it exited).
fn passes_after(passes: &UnixStream, n: usize, deadline: Instant) -> bool {
    let mut buf = [0_u8; 256];
    let mut reader = passes;
    // Drain: anything already buffered completed before now.
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return false,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    let mut seen = 0;
    while seen < n {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let remaining = deadline - now;
        let timeout = Timespec {
            tv_sec: i64::try_from(remaining.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(remaining.subsec_nanos()),
        };
        let mut fds = [PollFd::new(passes, PollFlags::IN)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) | Err(Errno::INTR) => continue,
            Ok(_) => {}
            Err(_) => return false,
        }
        match reader.read(&mut buf) {
            Ok(0) => return false,
            Ok(k) => seen += k,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
            Err(_) => return false,
        }
    }
    true
}

/// Keep the helper running until the uid is **confirmed** empty twice in a row, or until the
/// bound; then kill and reap it. `true` iff confirmed.
///
/// A scan that finds nothing but the helper (its euid is the run uid, so the scan lists it)
/// confirms only once the helper reports two more completed `kill(-1)` passes after the scan
/// has ended: the second of those began after the first finished, hence after the scan. So a
/// scan cannot count while the helper is starved of CPU (there are no cgroups yet), and
/// anything the non-atomic scan missed was alive when a full pass began — and was killed by it.
fn supervise_helper(helper: i32, uid: u32, passes: &UnixStream) -> bool {
    let Some(p) = Pid::from_raw(helper) else { return false };
    let pidfd = pidfd_open(p, PidfdFlags::empty()).ok();
    let deadline = Instant::now() + HELPER_TIMEOUT;
    let mut empty_scans = 0;
    let emptied = loop {
        if Instant::now() >= deadline {
            break false;
        }
        // A helper that exited (credential failure, pass cap, or killed) is not sweeping.
        if pidfd.as_ref().is_none_or(has_exited) {
            break false;
        }
        match scan(None, uid) {
            Some(s) if s.all().iter().all(|&pid| pid == helper) => {
                if !passes_after(passes, 2, deadline) {
                    break false;
                }
                empty_scans += 1;
                if empty_scans >= 2 {
                    break true;
                }
            }
            Some(_) => empty_scans = 0,
            None => break false,
        }
        std::thread::sleep(SCAN_PAUSE);
    };
    match &pidfd {
        Some(fd) => {
            let _ = pidfd_send_signal(fd, Signal::KILL);
        }
        None => {
            let _ = rustix::process::kill_process(p, Signal::KILL);
        }
    }
    let _ = reap(p);
    emptied
}

/// How long [`lock_uid`] retries a held lock before reporting it in use.
const LOCK_RETRY: Duration = Duration::from_secs(1);

/// Root-owned directory holding one lock file per uid in use by a live sandbox.
pub(crate) const LOCK_DIR: &str = "/run/mcp-conformance-sandbox";

/// Take the exclusive lock for `uid`, or `Ok(None)` if another live sandbox holds it.
///
/// One `flock`ed file per uid under [`LOCK_DIR`] (created `0700`; must be root-owned and not
/// group/other-writable, checked without following symlinks). `flock` locks belong to the open
/// file description, so the lock lives exactly as long as the returned `File` — the whole run,
/// including teardown — and is released by the kernel if the supervisor dies. The descriptor
/// is `O_CLOEXEC` (std's default) and the child marks everything ≥ 3 close-on-exec anyway, so
/// the tool never holds it.
///
/// Retried for up to [`LOCK_RETRY`]: a lock released at the end of one run can still be held
/// for a moment by a *copy* of its descriptor in a sibling sandbox's child between `fork` and
/// `exec` (where CLOEXEC has not yet acted), which would otherwise surface as a spurious
/// [`RunAsInUse`](crate::SandboxError::RunAsInUse) in a supervisor running several sandboxes.
pub(crate) fn lock_uid(uid: u32) -> io::Result<Option<File>> {
    let dir = Path::new(LOCK_DIR);
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
        return Err(io::Error::other(format!(
            "{LOCK_DIR} must be a root-owned directory not writable by group or others"
        )));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(format!("uid-{uid}.lock")))?;
    let deadline = Instant::now() + LOCK_RETRY;
    loop {
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(Some(file)),
            Err(Errno::WOULDBLOCK) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(Errno::WOULDBLOCK) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    }
}

/// Whether a passwd- or group-format file (`name:x:id:...` lines) has an entry with numeric id
/// `id` in its third field. Pure, for unit testing. Reads the files only — not NSS.
pub(crate) fn database_has_id(db: &str, id: u32) -> bool {
    db.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split(':').nth(2))
        .any(|f| f.parse::<u32>() == Ok(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_uid_matching_reads_all_four_ids_and_ignores_zombies() {
        let live = b"Name:\tsleep\nState:\tS (sleeping)\nUid:\t5\t7\t2000000000\t5\nThreads:\t1\n";
        assert_eq!(status_matches_uid(live, 2_000_000_000), Some(true));
        assert_eq!(status_matches_uid(live, 7), Some(true));
        assert_eq!(status_matches_uid(live, 2_000_000_001), Some(false));
        let zombie = b"Name:\tsh\nState:\tZ (zombie)\nUid:\t2000000000\t2000000000\t2000000000\t2000000000\nThreads:\t1\n";
        assert_eq!(status_matches_uid(zombie, 2_000_000_000), Some(false));
    }

    #[test]
    fn a_zombie_leader_with_live_threads_is_live_and_a_lone_zombie_is_not() {
        let with = |threads: u32| {
            format!(
                "Name:\tzl\nState:\tZ (zombie)\nUid:\t2000000000\t2000000000\t2000000000\t2000000000\nThreads:\t{threads}\n"
            )
        };
        assert_eq!(status_matches_uid(with(2).as_bytes(), 2_000_000_000), Some(true), "Z, Threads: 2");
        assert_eq!(status_matches_uid(with(1).as_bytes(), 2_000_000_000), Some(false), "Z, Threads: 1");
        let x = b"State:\tX (dead)\nUid:\t5\t5\t5\t5\nThreads:\t3\n";
        assert_eq!(status_matches_uid(x, 5), Some(true));
        let f = parse_status(with(2).as_bytes()).unwrap();
        assert!(f.exited);
        assert_eq!(f.threads, 2);
        assert_eq!(f.uids, [2_000_000_000; 4]);
    }

    #[test]
    fn a_threads_0_leader_is_stale_and_present_and_only_threads_1_is_a_real_zombie() {
        // Round 5: `de_thread` can leave a status read printing the released leader as
        // `State: Z`/`X` with `Threads: 0` while the process lives on under the same pid.
        let st = |state: &str, threads: u32| {
            format!(
                "Name:\tsl\nState:\t{state}\nUid:\t2000000000\t2000000000\t2000000000\t2000000000\nThreads:\t{threads}\n"
            )
        };
        let u = 2_000_000_000;
        assert_eq!(status_matches_uid(st("X (dead)", 0).as_bytes(), u), Some(true), "X, Threads: 0");
        assert_eq!(status_matches_uid(st("Z (zombie)", 0).as_bytes(), u), Some(true), "Z, Threads: 0");
        assert_eq!(status_matches_uid(st("Z (zombie)", 1).as_bytes(), u), Some(false), "Z, Threads: 1");
        assert_eq!(status_matches_uid(st("X (dead)", 0).as_bytes(), 5), Some(false), "by its own uids");
        let class = |state: &str, threads| classify_leader(&parse_status(st(state, threads).as_bytes()).unwrap());
        assert_eq!(class("X (dead)", 0), Leader::Stale);
        assert_eq!(class("Z (zombie)", 0), Leader::Stale);
        assert_eq!(class("S (sleeping)", 0), Leader::Stale, "never a live leader, but present either way");
        assert_eq!(class("Z (zombie)", 1), Leader::Zombie);
        assert_eq!(class("X (dead)", 1), Leader::Zombie);
        assert_eq!(class("R (running)", 1), Leader::Single);
        assert_eq!(class("Z (zombie)", 2), Leader::Multi);
    }

    #[test]
    fn status_is_parsed_as_bytes_and_a_raw_high_byte_in_the_name_is_ignored() {
        // Round-4 review: `prctl(PR_SET_NAME, "\xff")` puts a raw 0xFF in the Name: line, which
        // made a UTF-8 read fail and the process vanish from every scan.
        let mut odd = b"Name:\t".to_vec();
        odd.extend_from_slice(&[0xff, 0xfe, b'x']);
        odd.extend_from_slice(b"\nUmask:\t0022\nState:\tS (sleeping)\nUid:\t42\t42\t42\t42\nThreads:\t1\n");
        assert!(std::str::from_utf8(&odd).is_err(), "precondition: not UTF-8");
        let f = parse_status(&odd).expect("a raw high byte in Name: must not make it unparseable");
        assert_eq!(f, StatusFacts { exited: false, threads: 1, uids: [42; 4] });
        // A Name: that tries to look like another field is still only the Name: line.
        let forged = b"Name:\tUid: 1 1 1 1\nState:\tR (running)\nUid:\t42\t42\t42\t42\nThreads:\t1\n";
        assert_eq!(parse_status(forged).unwrap().uids, [42; 4]);
    }

    #[test]
    fn a_malformed_status_fails_closed() {
        let ok = b"State:\tS (sleeping)\nUid:\t1\t2\t3\t4\nThreads:\t1\n";
        assert!(parse_status(ok).is_some(), "the control parses");
        for bad in [
            &b"State:\tS (sleeping)\nThreads:\t1\n"[..],                              // no Uid:
            b"State:\tS (sleeping)\nUid:\t1\t2\t3\nThreads:\t1\n",                    // 3 uid fields
            b"State:\tS (sleeping)\nUid:\t1\t2\t3\t4\t5\nThreads:\t1\n",              // 5 uid fields
            b"State:\tS (sleeping)\nUid:\t1\tx\t3\t4\nThreads:\t1\n",                 // non-numeric
            b"State:\tS (sleeping)\nUid:\t1\t2\t3\t4\n",                              // no Threads:
            b"State:\tS (sleeping)\nUid:\t1\t2\t3\t4\nThreads:\tmany\n",              // bad Threads:
            b"Uid:\t1\t2\t3\t4\nThreads:\t1\n",                                       // no State:
            b"State:\tS\nUid:\t1\t2\t3\t4\nUid:\t1\t2\t3\t4\nThreads:\t1\n",          // two Uid:
            b"",
        ] {
            assert_eq!(parse_status(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn unverifiable_pids_count_as_present_but_not_as_run_members() {
        let s = Scan { unverifiable: vec![99], ..Scan::default() };
        assert!(!s.is_empty(), "a scan with an unverifiable pid is never empty");
        assert_eq!(s.all(), vec![99]);
        assert!(s.outside_namespace().is_empty());
    }

    #[test]
    fn id_databases_match_on_the_numeric_third_field_only() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n# dev:x:70000:\ndev:x:1000:1000::/home/dev:/bin/bash\nweird:x:2000000000:5::/:/bin/false\n";
        assert!(database_has_id(passwd, 0));
        assert!(database_has_id(passwd, 1000));
        assert!(database_has_id(passwd, 2_000_000_000));
        assert!(!database_has_id(passwd, 70_000), "comment lines are not entries");
        assert!(!database_has_id(passwd, 5), "the gid field is not the uid field");
        assert!(!database_has_id("", 0));
    }

    #[test]
    fn scan_views_combine_and_separate_escapees() {
        let s = Scan { in_namespace: vec![10, 12], by_uid: vec![10, 11, 12, 30], ..Scan::default() };
        assert_eq!(s.all(), vec![10, 11, 12, 30]);
        assert_eq!(s.outside_namespace(), vec![11, 30]);
        assert!(!s.is_empty());
        assert!(Scan::default().is_empty());
    }

    // ---- Root-only: real processes under synthetic uids (none with an /etc/passwd entry, far
    // from the integration tests' range). Run in the pinned guest with the sudo runner, as for
    // `tests/supervisor.rs`.

    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Child, Command};

    fn require_root() {
        assert!(rustix::process::geteuid().is_root(), "needs root; see tests/supervisor.rs");
    }

    fn sleeper(uid: u32) -> Child {
        Command::new("/bin/sleep").arg("1000").uid(uid).gid(uid).spawn().expect("spawn sleep")
    }

    /// The terminating signal, if the child ends within 10 s. Bounded, so a regression fails
    /// the test instead of hanging it (the `kill_if_uid` mutation did, before this).
    fn ended_by(child: &mut Child) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                return status.signal();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        None
    }

    #[test]
    #[ignore = "needs root; see tests/supervisor.rs for the sudo runner command"]
    fn kill_if_uid_kills_a_process_of_that_uid_and_spares_any_other() {
        require_root();
        let (mine, other) = (2_000_090_000, 2_000_090_002);
        let mut target = sleeper(mine);
        let mut bystander = sleeper(other);
        // Let both finish exec so /proc shows their final credentials.
        std::thread::sleep(Duration::from_millis(500));

        kill_if_uid(i32::try_from(bystander.id()).unwrap(), mine);
        kill_if_uid(i32::try_from(target.id()).unwrap(), mine);

        assert_eq!(ended_by(&mut target), Some(9), "a matching uid is killed");
        std::thread::sleep(Duration::from_millis(200));
        assert!(bystander.try_wait().unwrap().is_none(), "a different uid must be spared");
        bystander.kill().unwrap();
        bystander.wait().unwrap();
    }

    #[test]
    #[ignore = "needs root; see tests/supervisor.rs for the sudo runner command"]
    fn a_zombie_leader_with_a_live_thread_is_seen_by_scans_and_killed_by_kill_if_uid() {
        require_root();
        let uid = 2_000_090_020;
        let out = std::env::temp_dir().join(format!("zl-{}", std::process::id()));
        // Null stdio and a kill-on-drop guard (`spawn_example`): if an assertion fails, the
        // process must not outlive the test — holding the test's output pipe, it hung
        // `cargo test` when this test was mutation-checked.
        let (mut guard, _dir) = spawn_example("zombie_leader", &[out.to_str().unwrap()], uid);
        let child = &mut guard.0;
        let pid = i32::try_from(child.id()).unwrap();
        // Wait for the main thread to have exited: leader Z, Threads 2.
        let deadline = Instant::now() + Duration::from_secs(10);
        let zombie_leader = loop {
            let status = fs::read(format!("/proc/{pid}/status")).unwrap_or_default();
            let f = parse_status(&status).unwrap_or_default();
            if f.exited && f.threads > 1 {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(zombie_leader, "the test program never reached the zombie-leader state");

        let s = scan(None, uid).expect("/proc readable");
        assert_eq!(s.by_uid, vec![pid], "the scan must see a zombie leader with a live thread");
        kill_if_uid(pid, uid);
        assert_eq!(ended_by(child), Some(9), "kill_if_uid must kill the whole group");
        assert!(scan(None, uid).unwrap().by_uid.is_empty());
        let _ = fs::remove_file(&out);
    }

    /// A child that is killed and reaped however the test ends, so a failing assertion can
    /// never leave it holding the test's output open (that hung `cargo test` once).
    struct Reaped(Child);
    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Run a copy of example `name` (the build tree is not traversable by a synthetic uid)
    /// under `uid`, with null stdio. The tempdir is returned so it outlives the process.
    fn spawn_example(name: &str, args: &[&str], uid: u32) -> (Reaped, tempfile::TempDir) {
        let exe = std::env::current_exe().unwrap();
        let src = exe.parent().unwrap().parent().unwrap().join("examples").join(name);
        assert!(src.exists(), "{} missing: `cargo test -p sandbox` builds it", src.display());
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();
        let copy = dir.path().join(name);
        fs::copy(&src, &copy).unwrap();
        // `ETXTBSY`: a concurrent test's `fork` can inherit the copy's write fd for the moment
        // until that child execs (the usual copy-then-exec race in a multithreaded binary; it
        // failed 4 of 5 guest runs once three tests did this). Retry briefly; it clears itself.
        let mut busy = 0;
        let child = loop {
            let spawned = Command::new(&copy)
                .args(args)
                .uid(uid)
                .gid(uid)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            match spawned {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && busy < 100 => {
                    busy += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                r => break r.expect("spawn example"),
            }
        };
        (Reaped(child), dir)
    }

    /// Whether any status under `/proc/<pid>` (the process's or one task's) carries a raw 0xFF.
    fn has_raw_ff_name(pid: u32) -> bool {
        let mut paths = vec![format!("/proc/{pid}/status")];
        if let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) {
            paths.extend(tasks.flatten().map(|t| t.path().join("status").display().to_string()));
        }
        paths.iter().any(|p| fs::read(p).is_ok_and(|b| b.contains(&0xff)))
    }

    #[test]
    #[ignore = "needs root; see tests/supervisor.rs for the sudo runner command"]
    fn a_process_or_thread_with_a_non_utf8_name_is_seen_verified_and_killed() {
        require_root();
        for (mode, uid) in [("process", 2_000_090_030_u32), ("thread", 2_000_090_032)] {
            let extra: &[&str] = if mode == "thread" { &["thread"] } else { &[] };
            let (mut guard, dir) = spawn_example("odd_name", &[&["/dev/null"][..], extra].concat(), uid);
            let pid = guard.0.id();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !has_raw_ff_name(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            assert!(has_raw_ff_name(pid), "{mode}: the test program never set its 0xFF name");
            if mode == "process" {
                let raw = fs::read(format!("/proc/{pid}/status")).unwrap();
                assert!(String::from_utf8(raw).is_err(), "precondition: the status is not UTF-8");
            }

            let s = scan(None, uid).expect("/proc readable");
            let me = i32::try_from(pid).unwrap();
            assert_eq!(s.by_uid, vec![me], "{mode}: verified as the uid's, not merely unverifiable");
            assert!(s.unverifiable.is_empty(), "{mode}: {s:?}");
            kill_if_uid(me, uid);
            assert_eq!(ended_by(&mut guard.0), Some(9), "{mode}: kill_if_uid must kill it");
            assert!(scan(None, uid).unwrap().is_empty());
            drop(dir);
        }
    }

    /// Read `/proc/<pid>/status` and return `(state_letter, threads)` for tallying the race.
    fn raw_state(pid: i32) -> Option<(u8, u32)> {
        let raw = fs::read(format!("/proc/{pid}/status")).ok()?;
        let f = parse_status(&raw)?;
        let letter = raw
            .split(|&b| b == b'\n')
            .find_map(|l| l.strip_prefix(b"State:"))
            .and_then(|r| r.iter().copied().find(|b| !b.is_ascii_whitespace()))?;
        Some((letter, f.threads))
    }

    #[test]
    #[ignore = "needs root; see tests/supervisor.rs for the sudo runner command"]
    fn a_stale_threads_0_leader_is_counted_present_throughout_the_de_thread_race() {
        require_root();
        let uid = 2_000_090_040;
        let (mut guard, _dir) = spawn_example("stale_leader", &[], uid);
        let pid = i32::try_from(guard.0.id()).unwrap();

        let (mut not_ours, mut z0, mut x0, mut views) = (0_u64, 0_u64, 0_u64, 0_u64);
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            // The process loops forever via execve, so this pid is always alive until we kill
            // it below; any view that is not "ours" and not "unverifiable" is a false negative.
            match process_view(pid, None, uid) {
                View::Known { by_uid: true, .. } | View::Unverifiable => {}
                View::Gone | View::Known { by_uid: false, .. } => not_ours += 1,
            }
            match raw_state(pid) {
                Some((b'Z', 0)) => z0 += 1,
                Some((b'X', 0)) => x0 += 1,
                _ => {}
            }
            views += 1;
        }
        // Still alive (the loop never exits on its own); confirm before judging the counts.
        assert!(guard.0.try_wait().unwrap().is_none(), "the test program exited early");
        eprintln!("stale-leader race over {views} views: (Z,0)={z0} (X,0)={x0} not_ours={not_ours}");
        assert_eq!(not_ours, 0, "a live stale-leader pid was seen as gone/not-ours {not_ours}x");
        // A sanity check the harness itself kills it, as the real sweep would.
        kill_if_uid(pid, uid);
        // execve keeps re-creating the group under the same pid; the pidfd group-kill plus the
        // bounded wait settle it.
        let _ = ended_by(&mut guard.0);
    }

    #[test]
    #[ignore = "needs root; see tests/supervisor.rs for the sudo runner command"]
    fn kill_run_ends_every_process_of_the_uid_and_reports_them() {
        require_root();
        let ids = RunIds { uid: 2_000_090_010, gid: 2_000_090_010, helper_uid: 2_000_090_011 };
        let mut kids: Vec<Child> = (0..3).map(|_| sleeper(ids.uid)).collect();
        std::thread::sleep(Duration::from_millis(500));

        let (residual, completed) = kill_run(None, ids);

        let residual = residual.expect("/proc readable");
        assert_eq!(residual.by_uid.len(), 3, "{residual:?}");
        assert!(completed);
        for kid in &mut kids {
            assert_eq!(ended_by(kid), Some(9));
        }
        assert!(scan(None, ids.uid).unwrap().by_uid.is_empty());
        assert!(scan(None, ids.helper_uid).unwrap().by_uid.is_empty(), "no helper left behind");
    }
}
