# ADR-016: Privileged overlay mount, and an upper layer the tool cannot reach

**Status:** Accepted
**Date:** 2026-10-29
**Author:** Jun Cui
**Closes:** the privileged-vs-rootless mount choice that
[ADR-010](010-pinned-linux-environment.md) left to
[P1-03](../tasks.md#p1-03-sandbox-supervisor--mount-namespace-overlayfs-timeout)
**Related:** [ADR-011](011-normaliser-semantics.md) decision 5 and its `user.overlay.*`
finding (which this follows), [ADR-009](009-evidence-tree-serialisation.md) (the format the
upper layer is captured in), [F-08's preconditions on running untrusted code](../tasks.md#preconditions-on-the-transition-to-running-untrusted-code),
`docs/HANDOFF.md` §4.6, architecture.md §5, design.md §3

---

## Context

ADR-010 pinned three overlayfs options unconditionally (`redirect_dir=off`, `metacopy=off`,
`index=off`) and a fourth, `userxattr`, *conditionally*: required if the overlay is mounted
unprivileged inside the sandbox's own user namespace, forbidden if it is mounted by real root.
It deferred the choice to P1-03.

ADR-011 then measured why the choice matters for evidence integrity, not just for mount
mechanics. Overlayfs records an opaque directory with an `overlay.opaque` xattr. In trusted
mode that xattr lives in `trusted.overlay.*`, which only `CAP_SYS_ADMIN` can write or remove:
on a live mount with the pinned options, an unprivileged tool that did
`rm -rf Documents && mkdir Documents` could **not** remove the kernel's
`trusted.overlay.opaque=y` (`EPERM`, both through the merged view and directly on the
upperdir). Under `userxattr` the same marker lives in `user.overlay.*`, in the namespace the
file's owner controls — and the tool owns everything it creates in the upper layer. Removing
a real opaque marker is the attack: it makes a directory replacement look like a set of
ordinary changes, which `normalise` reads as less destructive than what happened.

ADR-011 drew two consequences for P1-03: prefer the privileged mount, and *in either case*
assert that the upperdir is unreachable from inside the sandbox — because the `EPERM` above is
the kernel refusing a `trusted.*` write, not a guarantee that the upper-layer files are out of
the tool's reach.

## Decision

### 1. The overlay is mounted by real root, in a fresh mount namespace, before any credential drop

The supervisor runs as root. In the forked child, before `exec`, it unshares a mount
namespace, makes `/` recursively private, mounts the overlay with
`lowerdir=…,upperdir=…,workdir=…,redirect_dir=off,metacopy=off,index=off` and
`MS_NOSUID|MS_NODEV`, pivots into it, and only then drops to an unprivileged uid/gid with
`no_new_privs`. **`userxattr` is not set**, and `crates/sandbox/src/overlay.rs::mount_data`
has no parameter through which it could be; the kernel reports `nouserxattr` in the mount's
super options, which the integration test asserts.

| Option | Opaque/redirect markers live in | Tool can remove a real marker? | Needs | Verdict |
|---|---|---|---|---|
| **A: privileged mount (chosen)** | `trusted.overlay.*` | No — `EPERM` (measured, ADR-011) | root on the supervisor | Accepted |
| B: rootless, inside a user namespace, `userxattr` | `user.overlay.*` | Yes — the tool owns the inode | user namespace (P2-01), unprivileged-overlay restrictions | Rejected |

Option B's only advantage is that the supervisor itself need not be root. That is not worth
much here: the harness already requires a dedicated guest (F-08) and root inside it is the
norm, whereas B's cost lands on the evidence itself.

**Mount propagation is load-bearing, not hygiene.** Removing the `make-rprivate` step was
tried as a mutation test in the pinned guest: `pivot_root` then fails with `EINVAL` (its
parent mount is shared), and — the instructive half — the overlay mounts made before that
failure **propagated into the guest's own mount namespace and stayed there** (eight leaked
mounts from one test run, removed by hand). With the step in place a failure at any later step
takes the mount down with the child's namespace.

### 2. The upper and work directories are unreachable from inside, by construction and by test

The containment that makes this true, in the order it is applied:

1. **`pivot_root` into the merged view, then detach the old root.** After this the child's
   mount table is exactly one mount, the overlay at `/` — asserted from the host by reading
   `/proc/<pid>/mountinfo`. No host path, the upper and work directories included, has a name.
2. **No procfs, sysfs or `/dev` inside.** A procfs without a PID namespace exposes
   `/proc/<host-pid>/root` — the host root — for every host process. Mounting one waits for
   P2-01's PID namespace.
3. **Credential drop to a dedicated unprivileged id, with `no_new_privs`.** A root process in
   a mount namespace is not contained: it can mount a fresh procfs or `mknod` and mount the
   host's disk. Before the drop the child empties the capability **bounding set**
   (`PR_CAPBSET_DROP` up to `cap_last_cap`) and locks the securebits (`NOROOT`,
   `NO_CAP_AMBIENT_RAISE`, and `KEEP_CAPS`/`NO_SETUID_FIXUP` locked *off*, so the drop's
   capability clearing cannot be disabled). Every set — inheritable, permitted, effective,
   bounding, ambient — then reads zero, asserted from `/proc/<pid>/status`. The id must be
   ≥ 65 536 with no entry in `/etc/passwd` or `/etc/group`, so it is never a real account's
   (the files only; NSS sources such as LDAP are not consulted).
4. **Scratch directory `0700`, root-owned.** Independent of the above: the mutation test that
   removed `pivot_root` showed the dropped uid still could not see `upper`/`work` beneath it.
5. **Lower layer and scratch must be disjoint.** An upperdir inside the lowerdir would be
   visible through the lower layer; `spawn` refuses the spec.

`the_upper_and_work_directories_are_unreachable_from_inside` probes the upper, work, merged,
scratch and lower paths plus `/proc`, `/proc/1/root`, `/sys`, `/dev` and `/tmp` from inside,
with a positive control so a broken probe cannot pass vacuously; with `pivot_root` removed it
fails and lists them.

### 3. One lower directory, supplied as a path; how it is built is not the supervisor's concern

`SandboxSpec::lower` is a single existing directory. P1-02's builder produces one for the
tests. HANDOFF.md §4.6's recommendation for real servers — an OCI image unpacked by digest as
the immutable lower layer, with P1-02 specs only for seeded data on top — is **not** built
here: it needs an image fetch by digest, OCI-whiteout (`.wh.*`) to overlay-whiteout
conversion, and either multiple lower layers or a merge, none of which the P1-03 exit
criterion asks for, and its first consumer is P1-08. It is recorded on P1-08 in
`docs/tasks.md`. When it lands, multiple lower layers are an explicit list in `mount_data`,
never a caller-supplied `:`-joined string — `:` is already a forbidden byte for exactly that
reason.

### 4. A run is identified by an exclusive uid, and ended by a sweep it cannot resist

*Added after review (two rounds).* Phase 1 has no PID namespace, so the kernel draws no
boundary around a run's processes. The first version identified them by mount namespace and
killed what a `/proc` scan found in it. Review showed that is evadable: a run process ran
`unshare -Um --propagation unchanged …` as the dropped uid (Ubuntu 24.04's
`apparmor_restrict_unprivileged_userns=1` does not prevent the creation), acquired a mount
namespace of its own, was invisible to the scan, and kept writing the upper layer ~8 s after a
record saying `residual_pids: [], namespace_empty: true` was returned. Review also showed the
timeout bounded only the direct child: a background grandchild holding stdout blocked a
host-side `read_to_end` for its whole lifetime (25 s; forever with `sleep 1e9`).

What a run process cannot shed is its host uid — no capabilities, no bounding set,
`no_new_privs`, and a user namespace changes how ids read inside it, not the kernel uid. So:

- **A run owns two uids exclusively: its uid `u` and a helper uid `h = u + 1`.** `spawn` takes
  a `flock` on `/run/mcp-conformance-sandbox/uid-<u>.lock` and `uid-<h>.lock` (directory created
  `0700`, required root-owned and not group/other writable, checked with `lstat`; files opened
  `O_NOFOLLOW`) and holds both until teardown is finished. Either uid held elsewhere is refused
  (`RunAsInUse`); either uid with a live process is refused (`RunAsOccupied`) rather than
  adopted or killed. Both must be ≥ 65 536 with no `/etc/passwd` entry. Chosen over a
  supervisor-managed uid pool as the simplest sound option: callers name the uid, the kernel
  releases the locks if the supervisor dies, the occupancy check then catches anything that
  outlived it, and concurrent runs simply need uids at least 2 apart (overlap is refused, and
  tested).
- **The helper sweep, unsignalable by the run.** When the direct child exits — or at the
  deadline, after killing it — the watchdog reaps it and forks a helper with
  `setresuid(h, u, h)` that loops `kill(-1, SIGKILL)` with a 1 ms pause. Linux lets a sender
  signal a target when the sender's real or effective uid equals the target's real or saved
  uid: the helper's euid `u` reaches every run process, while a run process (`u`/`u`/`u`)
  matches neither the helper's real nor its saved uid. Verified in the pinned guest: a run-uid
  process gets `EPERM` for `SIGSTOP` and `SIGKILL` against such a helper and its `kill(-1)`
  leaves it alive, while the helper kills run processes. (The first version ran the helper as
  `u/u/u`; round-2 review showed a run process looping `kill(-1, SIGKILL)` or `SIGSTOP` could
  then defeat every helper and outlive teardown — fail-safe in the record, but a hostile
  process persisting on the worker.) Each pass signals every process of the uid under the
  task-list lock, so fork-and-exit chains cannot outrun it. The helper's own `kill(-1)` also
  reaches processes whose real or saved uid is `h` — that is why `h` is per-run and locked.
  Linux's `kill(-1)` never returns `ESRCH` while anything else runs, so the helper cannot
  know it is done: **the supervisor's two consecutive empty scans, 10 ms apart, are the stop
  condition**, bounded at 5 s per helper, two helpers. The helper closes every inherited
  descriptor first (it is a fork that never execs, and would otherwise hold other runs' locks
  and pipes).
- **Root kills what the helper leaves.** After the helper, the root supervisor scans and
  `SIGKILL`s every remaining process under `u` through a pidfd — open the pidfd, re-read the
  uid from `/proc/<pid>`, then signal, so a reused pid can never be hit — for up to 50 rounds.
  At most one further helper sweep is tried, and only if every sweep so far completed; once one
  has failed the run is fighting it and root's kills are what remain. The worst case is
  bounded by a count of work, not a wall time: about two helper attempts of up to 5 s each (twice, if the first sweep completed) plus up to ~51 `/proc` scans, and each scan is O(processes) — which, with no cgroups yet, only `RLIMIT_NPROC` bounds.
- **The mount-namespace identity is pinned.** The child passes an fd for its namespace over
  `SCM_RIGHTS` — checked with `NS_GET_NSTYPE` to be a mount namespace, or the launch fails — held
  until the final scan. nsfs inode numbers are reused once a namespace dies: before this, a
  stale identity matched a newer sandbox's namespace and teardown killed that sandbox's process
  — found by tracing `signal_generate` with bpftrace when a test flaked.
- **Both views are reported.** `TeardownReport` gives `residual_pids` (alive when the direct
  child ended, before any killing), `residual_outside_namespace` (of those, the ones only the
  uid found), `uid_sweep_completed` (every helper sweep completed and every scan was
  readable), `survivors` (the final scan) and `confirmed_empty` (the final scan was readable
  and found nothing in the namespace **or** under the uid; `scratch_removed` only then).

So the timeout now bounds the whole run, and the run's other processes are killed as soon as
the direct child ends — the host's stdio reads see EOF then — unless the run defeats a per-uid
`kill(-1)` from a helper it cannot signal *and* root's per-pid kills, in which case the attempt
still ends in bounded time and the record says it failed. What this does **not** make it is a
boundary: until the kill reaches them, run processes run and can write the upper layer, and
they can still create nested namespaces. P2-01's PID namespace (the kernel kills the tree with
its PID 1) and P4-01's seccomp denial of `CLONE_NEWUSER` close that.

### 5. Round-3 corrections: zombie leaders, a confirmed stop condition, verified privilege

*Added after the third review.*

- **A zombie thread-group leader is not dead while it has live threads.** A run descendant can
  start a thread and end its main thread with a raw `SYS_exit`: the leader then reads
  `State: Z`, `Threads: 2`, its `ns/mnt` link does not resolve, and the thread keeps running
  and writing the upper layer. Every scan treated `Z` as dead, so such a process was missing
  from `residual_pids`, `survivors`, the helper's stop scans, `kill_if_uid` and the
  `RunAsOccupied` preflight — a false clean the reviewer reproduced. Now a multithreaded
  process is judged by **any live task** under `/proc/<pid>/task/*` (uid and mount namespace
  alike — a thread need not share its leader's namespace), and its leader counts under the uid
  even if the task list cannot be read. `kill_if_uid` signals such a process through the
  leader's pidfd, which delivers a group-wide `SIGKILL`.
- **The helper's stop condition is confirmed by its own passes.** Two empty scans 10 ms apart
  could both happen before a CPU-starved helper (there are no cgroups) had run a single
  `kill(-1)` pass. The helper now writes one byte to a non-blocking socketpair end — the only descriptor
  it keeps — after each completed pass, and an empty scan counts only once two more passes are
  reported after the scan ended: the second began after the first finished, hence after the
  scan, so anything the scan missed was alive when a full pass began.
- **Privilege is verified, not assumed.** If the supervisor inherited
  `SECBIT_NO_SETUID_FIXUP` (systemd `SecureBits=`, some container runtimes), `setresuid` from
  root keeps the effective set — and a helper `kill(-1)` with `CAP_KILL` would kill every
  process on the host. The helper now sets the same locked securebits as the run child while
  still root (exit without killing if it cannot), and after `setresuid` checks with `capget`
  that its effective and permitted sets are empty (exit without killing otherwise). The run
  child gained the same `capget` check after its own `setresuid`; a supervisor whose bits are
  locked the wrong way already failed the child's securebits step. Asserted in the guest: the
  observed helper reads `CapEff`/`CapPrm` `0000000000000000`.
- **The run has its own session** (`setsid` before exec), closing the same-session `SIGCONT`
  exception to the uid rule.
- **`gid == u32::MAX` is refused**, matching the uid check: `setresgid(-1, -1, -1)` changes
  nothing, so the run would have kept gid 0.
- **Wording corrected:** a scan can miss a fast fork-and-exit chain at the readdir frontier
  repeatedly, not only on pid wraparound; the helper still kills it, but `residual_pids` can come
  out empty. P2-02's cgroup (`pids.current`, `cgroup.kill`) is the real fix. And the worst case
  is a count of scans, not a wall time (see decision 4). (Narrowed in round 5: without a pid
  wrap a forward chain is always found; see Consequences.)

### 6. Round-4 correction: the scanners fail closed, as a rule

*Added after the fourth review.* The round-3 scanner read `/proc/<pid>/status` as UTF-8 and
treated any error as "no process". The kernel escapes `comm` with only `ESCAPE_SPACE |
ESCAPE_SPECIAL`, so bytes `0x80..=0xFF` appear raw, and any process can put them there
(`prctl(PR_SET_NAME, "\xff")`, or exec a file so named — per thread, too, visible only in that
task's status). The read failed, and the process vanished from `residual_pids`, both views,
the helper's stop scans, `kill_if_uid` and the `RunAsOccupied` preflight: a false clean, the
third scan evasion in a row (after the nested namespace and the zombie leader).

So the fix is a rule, stated on `Scan`: **a scan may report false positives, never a false
empty.**

- Status is read as bytes and parsed byte-wise and strictly: exactly one `State:` line,
  exactly one numeric `Threads:` line, exactly one `Uid:` line with exactly four numbers;
  `Name:` is never interpreted (and cannot forge another line — `ESCAPE_SPECIAL` escapes
  newlines). Anything else is malformed.
- Only `ENOENT`/`ESRCH` mean "gone". Every other error, and any malformed status, makes the pid
  **unverifiable**: counted as present in `residual_pids`/`survivors` (reported separately as
  `unverified_pids`), as occupied in the preflight, and as non-empty for the helper's stop
  condition — but **never killed from root**, which signals only what it has verified as the
  run's (pidfd first, then re-verify through the same fail-closed view).
- The audit of every `/proc` read, with its choice, is listed in `docs/tasks.md` under P1-03's
  fourth review round.

The mutation check shows both halves: restoring the round-3 read reproduces the reviewer's
false clean (`residual_pids: []`), and a UTF-8 read that merely fails closed turns the same
process into an unverifiable pid — no false clean, but every concurrent spawn then refused as
`RunAsOccupied`, which is why the byte-wise parse matters as well as the rule.

**AppArmor signal mediation (reasoned in review, measured here).** AppArmor attaches
path-based profiles by the exec path *as seen in the process's mount namespace*, so a run
binary at a profiled path in the lower layer is confined by the host's profile: measured in
the pinned guest by exec'ing `/bin/sleep` bind-mounted over `/usr/bin/tcpdump` in a private
mount namespace — it ran as `tcpdump (enforce)`. A profile that refused signals from unconfined
senders would then make both the helper's and root's `SIGKILL` fail. On this image none does:
all 23 enforce-mode profiles either include `abstractions/base`, which grants
`signal (receive) peer=unconfined`, or allow all signals (`plasmashell`); the confined
`sleep` was killed by both the helper's credentials and root, with no denial logged.
`apparmor_restrict_unprivileged_unconfined` is `0` here. On a host with a stricter profile the
process **would survive** teardown — fail-closed, it is listed in `survivors` and the record is
not clean, but it keeps running. Proposed mitigation, not built: confine the run itself under a
dedicated harness profile (an `aa_change_onexec` before exec, with an inheriting exec rule),
so that path attachment never applies to run binaries — a P4 item alongside P4-01's seccomp.
Such a survivor has an operational cost as well. It is a live process under the run uid, so it
keeps holding its `RunAs` pair, and every later spawn on that pair is refused with
`RunAsOccupied` until someone intervenes by hand: fixing or unloading the profile and killing
it, or retiring the pair.

**Fifth review round (the stale leader).** The rule above held; one reading of `Threads:` did
not. A process can spawn a thread, end its main thread with a raw `SYS_exit`, and `execve`
from the thread. `de_thread` then swaps that thread onto the tgid and `release_task`s the old
leader. A concurrent `/proc/<pid>/status` read that took its reference before the swap prints
the released leader as `State: Z`/`X` with `Threads: 0`. The round-4 rule ("exited with
`Threads <= 1` is dead") reported that live process as gone: a false empty, which the reviewer
reproduced on about 0.1% of reads on a 6.6 host. A real zombie reads `Threads: 1`, because
`__exit_signal` drops the count and clears `sighand` under the same lock.

- The rule is now one pure function, `sweep::classify_leader`, shared by `process_view` and
  the unit tests. `Threads: 0` is a **stale leader, present** by the uids it shows; only an
  exited leader with `Threads: 1` is a zombie.
- `ENOENT` on `/proc/<pid>/task` keeps the leader's uid view instead of reporting the
  process gone, matching "a leader that shows the uid counts".
- The readdir-frontier wording is narrowed (Consequences): a forward fork chain is always
  found; a miss needs a pid allocated below the cursor, i.e. after a pid wrap.
- Test evidence, the race's hit counts under TCG and the mutation check are in `docs/tasks.md`
  under P1-03's fifth review round.

**Info: an unverifiable host pid affects every run.** An unverifiable pid's uids are unknown,
so it is counted against whichever uid is being scanned. Any one on the host (a status read
failing with anything but `ENOENT`/`ESRCH`, or a malformed status) therefore makes every
concurrent teardown uncertain, and makes every spawn's preflight refuse with `RunAsOccupied`,
until it goes away. That is the fail-safe direction, and a run cannot cause it. Its own
processes' statuses parse whatever their `comm` (the byte-wise parse), and it cannot make
another process's `/proc` entry unreadable to root.

## Consequences

- The supervisor must run as root; `spawn` refuses otherwise (`SandboxError::NotPrivileged`)
  after spec validation, so spec mistakes report identically on any host. The integration
  tests are `#[ignore]`d and run under a `sudo -E` cargo runner in the pinned guest.
- ADR-011's `user.overlay.*` concern stays what it measured: a tool can still *add*
  `user.overlay.*` xattrs (harmless — Decision 5 there ignores them by exact name), but cannot
  remove the kernel's `trusted.overlay.*` markers.
- **P2-01 must keep this property.** A user namespace for the tool is still wanted (it is the
  proper fix for shared-uid signalling and for orphan containment). The constraint it inherits
  from this ADR: the overlay is mounted from the initial user namespace by real root *before*
  the child enters its user namespace, so the mount remains a trusted-mode mount. Moving the
  mount inside the user namespace would silently flip the `userxattr` branch and reopen the
  removal attack; revisit this ADR rather than doing that.
- **P1-05's precondition:** a run is `containment_uncertain` if `residual_pids` is non-empty,
  `uid_sweep_completed` is false, or `confirmed_empty` is false — not merely when a namespace
  scan finds something. (`unverified_pids` are included in `residual_pids`/`survivors`, so they
  are covered.) A server started through a launcher that forks (`npx` → `node`) always has
  residual pids, so P1-08 must exec the server binary directly.
- **A clean record is "no descendant observed", and the qualifier must travel.** Until P2-02's cgroup accounting, a clean record means **no descendant was observed**, not that none existed. `/proc` is listed in pid order (`next_tgid`), so a forward
  fork-and-exit chain is always found: each child's pid lands at or ahead of the listing's
  cursor. A miss needs a pid allocated *below* the cursor, which is cyclic allocation after
  the pid allocator has wrapped. A host does that on its own over time, and a run can force
  it with ~`pid_max` forks (4,194,304 on the pinned image). After a wrap, a chain can be
  missed by every scan; the helper's `kill(-1)` still kills it, but `residual_pids` comes out
  empty.
  Requirement on **P1-05**: every gate result from this supervisor records the basis of its
  containment finding as `/proc` observation (e.g. `containment_basis = "proc_scan"`), and that
  qualifier stays attached to any verdict derived from the run and into publication (P5-04) —
  a caveat on the record, not a footnote. Requirement on **P2-02**: account for the run's
  processes with its cgroup (`cgroup.events` `populated 0` after `cgroup.kill`, and
  `pids.peak`/`pids.current`), which makes "no descendant existed" decidable, and upgrade the
  basis (e.g. `"cgroup"`) only then.
- Concurrent sandboxes need `RunAs` uids at least 2 apart (the tests step by 2).
- **AppArmor:** a run binary at a path attached by a host enforce profile is confined by it;
  if that profile refused signals from unconfined senders, the run would survive teardown
  (reported, not clean). None on the pinned image does (decision 6); mitigation proposed for P4.
  Operationally, such a survivor keeps holding its uid pair, so later spawns on that pair get
  `RunAsOccupied` until manual intervention.
- Known gaps the supervisor documents rather than closes (`crates/sandbox/src/supervisor.rs`
  module docs): run processes other than the direct child exist until the sweep reaches them
  and can create nested namespaces (P2-01, P4-01); shared network/IPC/UTS namespaces; no
  cgroups; no seccomp.
