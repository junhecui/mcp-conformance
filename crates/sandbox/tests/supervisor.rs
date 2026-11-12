//! P1-03 integration tests against real `unshare`/`mount`/`pivot_root` syscalls.
//!
//! Every test here needs root (the privileged overlay mount, ADR-016) and a kernel matching
//! the F-08 pin, so they are `#[ignore]`d: a plain `cargo test --workspace` on a non-root or
//! non-pinned host lists them as ignored rather than reporting a pass for something it never
//! ran. Run them inside the pinned guest, building as the normal user and executing the test
//! binary under `sudo`:
//!
//! ```text
//! # x86_64 guest (the Windows/WSL2 F-08 VM)
//! scripts/vm.sh ssh "bash -lc 'cd ~/mcp-conformance && \
//!   cargo test -p sandbox --config \"target.x86_64-unknown-linux-gnu.runner=\\\"sudo -E\\\"\" \
//!   -- --include-ignored'"
//! # aarch64 guest (the macOS/Lima VM): the runner key is target-specific
//!   cargo test -p sandbox --config 'target.aarch64-unknown-linux-gnu.runner="sudo -E"' \
//!   -- --include-ignored
//! ```
//!
//! Run with `--ignored` but without root, they fail loudly at [`require_root`] rather than
//! skipping.
//!
//! The lower layer is built with P1-02's `world::base_layer` builder: copies of the host's
//! `/bin/sh` (dash), `/bin/sleep` and `/usr/bin/unshare` plus the shared libraries `ldd`
//! names, placed at the same paths, so the sandbox has a working shell and nothing else of the
//! host's. Each fixture runs under its own uid, because a run's uid is exclusive to it.
#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use sandbox::{RunAs, SandboxError, SandboxSpec, spawn};
use world::base_layer::{BaseLayerSpec, SpecNode};

fn require_root() {
    assert!(
        rustix::process::geteuid().is_root(),
        "these tests need root; see this file's module docs for the sudo runner command"
    );
}

/// Add a host binary and every shared object `ldd` lists for it, at their host paths.
fn add_host_binary(spec: &mut BaseLayerSpec, host_path: &str) {
    add_host_binary_at(spec, host_path, host_path);
}

/// As [`add_host_binary`], but the binary itself goes to `lower_path` in the lower layer.
fn add_host_binary_at(spec: &mut BaseLayerSpec, host_path: &str, lower_path: &str) {
    let mut paths = vec![host_path.to_owned()];
    let ldd = Command::new("ldd").arg(host_path).output().expect("ldd");
    assert!(ldd.status.success(), "ldd {host_path} failed");
    for line in String::from_utf8(ldd.stdout).expect("ldd output is UTF-8").lines() {
        if let Some(path) = line.split_whitespace().find(|t| t.starts_with('/')) {
            paths.push(path.to_owned());
        }
    }
    for (i, path) in paths.iter().enumerate() {
        let content = fs::read(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
        let dest = if i == 0 { lower_path } else { path.as_str() };
        spec.add(dest.trim_start_matches('/').as_bytes(), SpecNode::file_mode(0o755, content));
    }
}

/// The `zombie_leader` example binary, which `cargo test` builds next to the test executables
/// (`<target>/debug/examples/`, beside `<target>/debug/deps/`).
fn zombie_leader_binary() -> String {
    example_binary("zombie_leader")
}

/// An example binary `cargo test` builds next to the test executables.
fn example_binary(name: &str) -> String {
    let exe = std::env::current_exe().unwrap();
    let path = exe.parent().unwrap().parent().unwrap().join("examples").join(name);
    assert!(path.exists(), "{} missing: run `cargo test -p sandbox` (it builds examples)", path.display());
    path.to_str().unwrap().to_owned()
}

/// Distinct uids per fixture, above `RunAs::DEFAULT` so a stray run of the default id is not
/// mistaken for a test's. Stepped by 2: a run also owns `uid + 1` as its helper uid.
static NEXT_ID: AtomicU32 = AtomicU32::new(2_000_000_100);

struct Fixture {
    // Held for their Drop.
    _lower_dir: tempfile::TempDir,
    scratch_parent: tempfile::TempDir,
    lower: PathBuf,
    scratch: PathBuf,
    run_as: RunAs,
}

impl Drop for Fixture {
    /// Hygiene only, after every assertion has run: if a mutation (or a regression) left a
    /// process under this fixture's uid, do not leak it into the next test or the guest.
    fn drop(&mut self) {
        for uid in [self.run_as.uid, self.run_as.helper_uid()] {
            let _ = Command::new("pkill").args(["-KILL", "-U", &uid.to_string()]).status();
        }
    }
}

/// A lower layer with `/bin/sh`, `/bin/sleep`, `/usr/bin/unshare`, a world-writable `/work`,
/// and a marker file. No `/dev`: the reachability probe asserts there is none.
fn fixture() -> Fixture {
    fixture_with(|_| {})
}

/// [`fixture`], plus an empty *regular file* at `/dev/null`: dash opens `/dev/null` for a
/// background job's stdin and refuses to start the job without it.
fn fixture_with_dev_null() -> Fixture {
    fixture_with(|spec| {
        spec.add("dev/null", SpecNode::file_mode(0o666, ""));
    })
}

fn fixture_with(extra: impl FnOnce(&mut BaseLayerSpec)) -> Fixture {
    let lower_dir = tempfile::tempdir().expect("tempdir");
    let lower = lower_dir.path().join("lower");
    let mut spec = BaseLayerSpec::new();
    add_host_binary(&mut spec, "/bin/sh");
    add_host_binary(&mut spec, "/bin/sleep");
    add_host_binary(&mut spec, "/usr/bin/unshare");
    spec.add("work", SpecNode::dir_mode(0o777));
    spec.add("etc/marker", SpecNode::file_mode(0o644, "from the lower layer\n"));
    extra(&mut spec);
    fs::create_dir(&lower).expect("mkdir lower");
    spec.materialize(&lower).expect("materialize lower");
    let scratch_parent = tempfile::tempdir().expect("tempdir");
    let scratch = scratch_parent.path().join("run");
    let id = NEXT_ID.fetch_add(2, Ordering::SeqCst);
    let run_as = RunAs { uid: id, gid: id };
    Fixture { _lower_dir: lower_dir, scratch_parent, lower, scratch, run_as }
}

/// Live (non-zombie) host pids with `uid` as any of their uids.
fn live_pids_of(uid: u32) -> Vec<i32> {
    let mut out = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else { continue };
        let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) else { continue };
        // A zombie *leader* fronting live threads is still running (round-3 review).
        let zombie = status.lines().any(|l| l.starts_with("State:") && l.contains("Z ("))
            && status.lines().any(|l| l == "Threads:\t1");
        let ours = status
            .lines()
            .find(|l| l.starts_with("Uid:"))
            .is_some_and(|l| l.split_whitespace().skip(1).any(|f| f == uid.to_string()));
        if ours && !zombie {
            out.push(pid);
        }
    }
    out
}

impl Fixture {
    fn spec(&self, program: &str, args: &[&str], timeout: Duration) -> SandboxSpec {
        SandboxSpec {
            lower: self.lower.clone(),
            scratch: self.scratch.clone(),
            program: PathBuf::from(program),
            args: args.iter().map(OsString::from).collect(),
            env: vec![("MCP_SANDBOX_TEST".into(), "1".into())],
            cwd: PathBuf::from("/"),
            run_as: self.run_as,
            timeout,
        }
    }

    fn sh(&self, script: &str) -> SandboxSpec {
        self.spec("/bin/sh", &["-c", script], Duration::from_secs(30))
    }

    /// The canonical scratch path, as the supervisor resolves it.
    fn canonical_scratch(&self) -> PathBuf {
        fs::canonicalize(self.scratch_parent.path()).unwrap().join("run")
    }
}

fn entries(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect()
}

fn read_all(mut r: impl Read) -> String {
    let mut s = String::new();
    r.read_to_string(&mut s).expect("read");
    s
}

#[test]
#[ignore = "needs root and the F-08 pinned kernel; see module docs"]
fn a_write_lands_in_the_upper_layer_not_the_lower_layer_or_the_host() {
    require_root();
    let fx = fixture();
    assert!(!Path::new("/work/out.txt").exists(), "precondition: host has no /work/out.txt");

    let (sandbox, stdio) =
        // No `cat` in the lower layer; dash's `read` builtin stands in for it.
        spawn(&fx.sh(
            "echo written-in-sandbox > /work/out.txt && \
             while read -r l; do echo \"$l\"; done < /etc/marker",
        ))
        .expect("spawn");
    drop(stdio.stdin);
    let stdout = read_all(stdio.stdout);
    let record = sandbox.wait().expect("wait");

    assert!(record.status.success(), "{record:?}");
    assert!(!record.timed_out && !record.killed_on_request);
    assert_eq!(stdout, "from the lower layer\n", "the lower layer is visible through the overlay");

    let out = record.upper.join("work/out.txt");
    assert_eq!(fs::read_to_string(&out).unwrap(), "written-in-sandbox\n");
    assert_eq!(fs::metadata(&out).unwrap().uid(), fx.run_as.uid, "written as the dropped id");
    assert!(!fx.lower.join("work/out.txt").exists(), "the lower layer must never be written");
    assert!(!Path::new("/work/out.txt").exists(), "the write must not reach the host root");
    assert_eq!(record.upper, fx.canonical_scratch().join("upper"));
}

#[test]
#[ignore = "needs root and the F-08 pinned kernel; see module docs"]
fn a_program_that_never_exits_is_killed_at_the_timeout() {
    require_root();
    let fx = fixture();
    let timeout = Duration::from_secs(1);
    let started = Instant::now();
    let (sandbox, stdio) = spawn(&fx.spec("/bin/sleep", &["100000"], timeout)).expect("spawn");
    drop(stdio);
    let record = sandbox.wait().expect("wait");
    let elapsed = started.elapsed();

    assert!(record.timed_out, "{record:?}");
    assert!(!record.killed_on_request);
    assert_eq!(record.status.signal(), Some(9), "killed by SIGKILL: {record:?}");
    assert!(record.wall_time >= timeout, "{record:?}");
    // Generous slack: the guest may be running under TCG emulation.
    assert!(elapsed < timeout + Duration::from_secs(10), "took {elapsed:?}");
    assert!(record.teardown.confirmed_empty && record.teardown.scratch_removed, "{record:?}");
}

#[test]
#[ignore = "needs root and the F-08 pinned kernel; see module docs"]
fn the_overlay_exists_only_in_the_child_namespace_and_teardown_leaves_only_the_upper_layer() {
    require_root();
    let fx = fixture();
    let host_before = fs::read_to_string("/proc/self/mountinfo").unwrap();

    let (sandbox, _stdio) = spawn(&fx.spec("/bin/sleep", &["100000"], Duration::from_secs(60)))
        .expect("spawn");
    let scratch = fx.canonical_scratch();
    let scratch_str = scratch.to_str().unwrap();

    // During the run: the host's mount table never sees the overlay...
    let host_during = fs::read_to_string("/proc/self/mountinfo").unwrap();
    assert!(!host_during.contains(scratch_str), "overlay leaked into the host namespace");
    // ...and the child's mount table is exactly one mount: the overlay as `/`, with the
    // pinned options and the privileged (`nouserxattr`) branch. The old root is gone.
    let child = fs::read_to_string(format!("/proc/{}/mountinfo", sandbox.pid())).unwrap();
    let lines: Vec<&str> = child.lines().collect();
    assert_eq!(lines.len(), 1, "expected only the overlay root, got:\n{child}");
    let fields: Vec<&str> = lines[0].split(' ').collect();
    assert_eq!(fields[4], "/", "{child}");
    let sep = fields.iter().position(|f| *f == "-").unwrap();
    assert_eq!(fields[sep + 1], "overlay", "{child}");
    let mount_opts = fields[5];
    assert!(mount_opts.contains("nosuid") && mount_opts.contains("nodev"), "{child}");
    let super_opts = fields[sep + 3];
    assert!(super_opts.contains(&format!("upperdir={scratch_str}/upper")), "{child}");
    assert!(super_opts.split(',').any(|o| o == "nouserxattr"), "{child}");
    for forbidden in ["userxattr", "redirect_dir=on", "metacopy=on", "index=on", "redirect_dir=follow"] {
        assert!(!super_opts.split(',').any(|o| o == forbidden), "{forbidden} in {child}");
    }

    let record = sandbox.kill_and_wait().expect("kill_and_wait");
    assert!(record.killed_on_request && !record.timed_out, "{record:?}");

    let host_after = fs::read_to_string("/proc/self/mountinfo").unwrap();
    assert_eq!(host_before, host_after, "host mount table changed across a run");
    assert!(record.teardown.confirmed_empty, "{record:?}");
    assert!(record.teardown.scratch_removed, "{record:?}");
    assert!(record.teardown.residual_pids.is_empty(), "{record:?}");
    assert_eq!(entries(&scratch), BTreeSet::from(["upper".to_owned()]));
}

#[test]
#[ignore = "needs root and the F-08 pinned kernel; see module docs"]
fn the_upper_and_work_directories_are_unreachable_from_inside() {
    require_root();
    let fx = fixture();
    let scratch = fx.canonical_scratch();
    // Positive control first: the probe must be able to see what *is* there, or a broken
    // `[ -e ]` would make every host path look unreachable.
    let mut spec = fx.sh(
        r#"[ -e /etc/marker ] && [ -d /work ] || echo "CONTROL FAILED"
           for p in "$UPPER" "$WORK" "$MERGED" "$SCRATCH" "$LOWER" "$TMPROOT" \
              /proc /proc/self /proc/1/root /sys /dev /tmp; do
             [ -e "$p" ] && echo "REACHABLE $p"
           done
           cd /.. && [ "$(echo *)" = "$(cd / && echo *)" ] || echo "ESCAPED via /.."
           echo payload > "$UPPER/escape" && echo "WROTE $UPPER/escape"
           echo DONE"#,
    );
    let lower_canonical = fs::canonicalize(&fx.lower).unwrap();
    spec.env.extend([
        ("UPPER".into(), scratch.join("upper").into()),
        ("WORK".into(), scratch.join("work").into()),
        ("MERGED".into(), scratch.join("merged").into()),
        ("SCRATCH".into(), scratch.clone().into()),
        ("LOWER".into(), lower_canonical.into()),
        ("TMPROOT".into(), std::env::temp_dir().into()),
    ]);

    let (sandbox, stdio) = spawn(&spec).expect("spawn");
    drop(stdio.stdin);
    let stdout = read_all(stdio.stdout);
    let record = sandbox.wait().expect("wait");

    assert_eq!(stdout, "DONE\n", "something on the host was reachable from inside");
    assert!(record.status.success(), "{record:?}");
    assert!(!record.upper.join("escape").exists());
    assert_eq!(entries(&record.upper), BTreeSet::new(), "the probe itself wrote nothing");
}

#[test]
#[ignore = "needs root and the F-08 pinned kernel; see module docs"]
fn stdio_round_trips_between_the_host_and_the_sandbox() {
    require_root();
    let fx = fixture();
    let (sandbox, mut stdio) =
        spawn(&fx.sh(r#"while read -r line; do echo "sandbox saw: $line"; done"#)).expect("spawn");

    let mut reader = BufReader::new(stdio.stdout);
    for msg in ["first line from the host", r#"{"jsonrpc":"2.0","id":1}"#] {
        writeln!(stdio.stdin, "{msg}").expect("write");
        stdio.stdin.flush().unwrap();
        let mut reply = String::new();
        reader.read_line(&mut reply).expect("read");
        assert_eq!(reply, format!("sandbox saw: {msg}\n"));
    }
    drop(stdio.stdin);
    let record = sandbox.wait().expect("wait");
    assert!(record.status.success() && !record.timed_out, "{record:?}");
}

#[test]
#[ignore = "needs root and the F-08 pinned kernel; see module docs"]
fn the_child_holds_only_stdio_no_privilege_and_only_the_given_environment() {
    require_root();
    let fx = fixture();
    // A descriptor the harness leaked without CLOEXEC — the classic inheritance hazard.
    let leaked = fs::File::open("/etc/hostname").unwrap();
    rustix::io::fcntl_setfd(&leaked, rustix::io::FdFlags::empty()).unwrap();

    let (sandbox, _stdio) = spawn(&fx.spec("/bin/sleep", &["100000"], Duration::from_secs(60)))
        .expect("spawn");
    let proc = PathBuf::from(format!("/proc/{}", sandbox.pid()));

    // `spawn` returns once `exec` has succeeded, which is before the dynamic loader is done:
    // `ld.so` briefly holds each shared library open as fd 3 (seen as a flake under TCG). So
    // wait for the set to settle. An *inherited* fd never closes — `sleep` closes nothing —
    // so a leak still fails this (a leaked fd 6 did, when `close_range` was mutated).
    let expected = BTreeSet::from(["0".to_owned(), "1".to_owned(), "2".to_owned()]);
    let settle = Instant::now() + Duration::from_secs(10);
    let mut fds = entries(&proc.join("fd"));
    while fds != expected && Instant::now() < settle {
        std::thread::sleep(Duration::from_millis(50));
        fds = entries(&proc.join("fd"));
    }
    let fd_targets: Vec<String> = fds
        .iter()
        .map(|fd| format!("{fd} -> {:?}", fs::read_link(proc.join("fd").join(fd)).ok()))
        .collect();
    let status = fs::read_to_string(proc.join("status")).unwrap();
    let environ = fs::read(proc.join("environ")).unwrap();
    let stat = fs::read_to_string(proc.join("stat")).unwrap();
    let record = sandbox.kill_and_wait().expect("kill_and_wait");
    drop(leaked);

    assert_eq!(fds, expected, "{fd_targets:?}");
    let field = |name: &str| -> Vec<String> {
        let line = status.lines().find(|l| l.starts_with(&format!("{name}:"))).unwrap();
        line.split_whitespace().skip(1).map(str::to_owned).collect()
    };
    assert_eq!(field("Uid"), vec![fx.run_as.uid.to_string(); 4], "{status}");
    assert_eq!(field("Gid"), vec![fx.run_as.gid.to_string(); 4], "{status}");
    assert!(field("Groups").is_empty(), "{status}");
    // Every capability set, the bounding set included.
    for caps in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
        assert_eq!(field(caps), vec!["0000000000000000".to_owned()], "{caps}: {status}");
    }
    assert_eq!(field("NoNewPrivs"), vec!["1".to_owned()], "{status}");
    assert_eq!(environ, b"MCP_SANDBOX_TEST=1\0", "environment must be exactly the spec's");
    // Its own session (setsid): the session id, field 6 of /proc/<pid>/stat, is its own pid.
    let after_comm: Vec<&str> = stat.rsplit_once(')').unwrap().1.split_whitespace().collect();
    assert_eq!(after_comm[3], stat.split_whitespace().next().unwrap(), "session: {stat}");
    assert!(record.teardown.confirmed_empty, "{record:?}");
}

#[test]
#[ignore = "needs root and the F-08 pinned kernel; see module docs"]
fn grandchildren_alive_at_the_timeout_are_recorded_and_swept() {
    // No PID namespace until P2-01, so killing the direct child does not end its children.
    // A pipeline makes the shell fork two children and wait; killing the shell orphans
    // both. The end-of-run sweep must kill them and the record must say they were there.
    require_root();
    let fx = fixture();
    let (sandbox, stdio) = spawn(&fx.spec(
        "/bin/sh",
        &["-c", "/bin/sleep 100000 | /bin/sleep 100000"],
        // Long enough for dash to have forked both sides even under TCG emulation.
        Duration::from_secs(3),
    ))
    .expect("spawn");
    drop(stdio);
    let record = sandbox.wait().expect("wait");

    assert!(record.timed_out, "{record:?}");
    let t = &record.teardown;
    assert_eq!(t.residual_pids.len(), 2, "{record:?}");
    assert!(t.residual_outside_namespace.is_empty(), "{record:?}");
    assert!(t.uid_sweep_completed && t.survivors.is_empty(), "{record:?}");
    assert!(t.confirmed_empty && t.scratch_removed, "{record:?}");
    assert!(live_pids_of(fx.run_as.uid).is_empty());
}

#[test]
#[ignore = "needs root and the F-08 pinned kernel; see module docs"]
fn a_program_missing_from_the_lower_layer_is_a_launch_error_that_leaves_nothing() {
    require_root();
    let fx = fixture();
    let err = spawn(&fx.spec("/usr/bin/python3", &[], Duration::from_secs(5))).unwrap_err();
    assert!(matches!(err, SandboxError::Launch { step: None, .. }), "{err}");
    assert!(!fx.scratch.exists(), "a failed launch must remove its scratch directory");
}

#[test]
#[ignore = "needs root and the F-08 pinned kernel; see module docs"]
fn a_process_escaping_into_a_nested_user_and_mount_namespace_is_killed_and_reported() {
    // Review finding: `unshare -Um` gives a run process a mount namespace of its own, so a
    // namespace scan cannot see it. It keeps its host uid, which is what the sweep keys on.
    // The direct child exits once the escapee has written, leaving it appending to the upper
    // layer.
    require_root();
    let fx = fixture_with_dev_null();
    let (sandbox, stdio) = spawn(&fx.spec(
        "/bin/sh",
        &[
            "-c",
            "/usr/bin/unshare -Um --propagation unchanged /bin/sh -c \
             'while :; do echo x >> /work/escape; /bin/sleep 0.2; done' & \
             while [ ! -s /work/escape ]; do /bin/sleep 0.1; done; exit 0",
        ],
        Duration::from_secs(60),
    ))
    .expect("spawn");
    // Dropped, not read: a surviving escapee would hold stderr open and a read to EOF would
    // hang the test instead of failing it (seen when the uid sweep was mutated away).
    drop(stdio);
    let record = sandbox.wait().expect("wait");
    let t = &record.teardown;

    assert!(!record.timed_out, "{record:?}");
    // It escaped, it was seen, and it was seen *only* by uid.
    assert!(!t.residual_outside_namespace.is_empty(), "escapee not detected: {record:?}");
    // It is gone, and the record does not claim otherwise.
    assert!(t.uid_sweep_completed, "{record:?}");
    assert!(t.survivors.is_empty(), "escapee survived the sweep: {record:?}");
    assert!(t.confirmed_empty && t.scratch_removed, "{record:?}");
    assert!(live_pids_of(fx.run_as.uid).is_empty(), "a uid {} process is alive", fx.run_as.uid);
    // And it no longer writes the upper layer.
    let escape = record.upper.join("work/escape");
    let before = fs::metadata(&escape).expect("the escapee did write").len();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(fs::metadata(&escape).unwrap().len(), before, "the upper layer is still being written");
}

#[test]
#[ignore = "needs root and the F-08 pinned kernel; see module docs"]
fn a_descendant_holding_stdout_cannot_block_the_host_past_the_direct_childs_exit() {
    // Review finding: the direct child exits at once, a background grandchild keeps stdout
    // open. The host reads to EOF *without* dropping stdio first; that read must end when the
    // direct child does, not when the grandchild chooses to (here: never), and not only at
    // the 60 s deadline.
    require_root();
    let fx = fixture_with_dev_null();
    let started = Instant::now();
    let (sandbox, stdio) = spawn(&fx.spec(
        "/bin/sh",
        &["-c", "/bin/sleep 100000 & echo started; exit 0"],
        Duration::from_secs(60),
    ))
    .expect("spawn");
    // The read runs on its own thread so a regression fails this test instead of hanging it
    // (the fixture's Drop then kills the holder, which unblocks the thread).
    let (tx, rx) = std::sync::mpsc::channel();
    let stdout = stdio.stdout;
    std::thread::spawn(move || {
        let _ = tx.send(read_all(stdout));
    });
    // Generous for TCG, far below the 60 s deadline.
    let out = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the host's stdout read was still blocked 20 s after the direct child exited");
    let read_took = started.elapsed();
    let record = sandbox.wait().expect("wait");

    assert_eq!(out, "started\n", "read took {read_took:?}");
    assert!(!record.timed_out && record.status.success(), "{record:?}");
    assert_eq!(record.teardown.residual_pids.len(), 1, "the grandchild is recorded: {record:?}");
    assert!(record.teardown.survivors.is_empty() && record.teardown.confirmed_empty, "{record:?}");
    drop(stdio.stdin);
}

#[test]
#[ignore = "needs root and the F-08 pinned kernel; see module docs"]
fn a_run_s_uid_is_exclusive_to_one_live_sandbox() {
    require_root();
    let first = fixture();
    let (sandbox, _stdio) =
        spawn(&first.spec("/bin/sleep", &["100000"], Duration::from_secs(60))).expect("spawn");

    let second = fixture();
    let mut spec = second.spec("/bin/sleep", &["1"], Duration::from_secs(60));
    spec.run_as = first.run_as;
    let err = spawn(&spec).unwrap_err();
    assert!(matches!(err, SandboxError::RunAsInUse(uid) if uid == first.run_as.uid), "{err}");
    assert!(!second.scratch.exists(), "a refused spawn creates nothing");
    // A run whose uid is a live run's *helper* uid is refused too: the helper's kill(-1) would
    // reach it.
    let mut overlapping = spec.clone();
    overlapping.run_as.uid = first.run_as.helper_uid();
    let err = spawn(&overlapping).unwrap_err();
    assert!(
        matches!(err, SandboxError::RunAsInUse(uid) if uid == first.run_as.helper_uid()),
        "{err}"
    );
    assert!(!second.scratch.exists(), "a refused spawn creates nothing");

    let record = sandbox.kill_and_wait().expect("kill_and_wait");
    assert!(record.teardown.confirmed_empty, "{record:?}");
    // Released after teardown: the same uid is usable again.
    let (again, _stdio) = spawn(&spec).expect("the lock is released when the run ends");
    let again_pid = again.pid();
    let again = again.wait().expect("wait");
    assert!(
        again.status.success() && again.teardown.confirmed_empty,
        "uid {} pid {again_pid}: {again:?}",
        first.run_as.uid
    );
}

#[test]
#[ignore = "needs root and the F-08 pinned kernel; see module docs"]
fn an_escapee_fighting_the_sweep_is_killed_anyway_and_teardown_is_bounded() {
    // Round-2 review: a run process that escapes into its own user+mount namespace and then
    // loops `kill -STOP -1; kill -KILL -1` signals everything it may. When the helper ran as
    // the run's uid that included each helper as it started, so the escapee could outlive
    // teardown. Now the helper's real and saved uid are the separate helper uid, so the
    // escapee gets EPERM against it, and root kills whatever the helper leaves.
    require_root();
    let fx = fixture_with_dev_null();
    let started = Instant::now();
    let (sandbox, stdio) = spawn(&fx.spec(
        "/bin/sh",
        &[
            "-c",
            "/usr/bin/unshare -Um --propagation unchanged /bin/sh -c \
             'while :; do kill -s STOP -1; kill -s KILL -1; echo x >> /work/escape; done' \
             2>/dev/null & \
             while [ ! -s /work/escape ]; do /bin/sleep 0.1; done; exit 0",
        ],
        Duration::from_secs(60),
    ))
    .expect("spawn");
    // Dropped, not read: see the escape test.
    drop(stdio);
    let record = sandbox.wait().expect("wait");
    let took = started.elapsed();
    let t = &record.teardown;

    // Bounded even against a run fighting back (TCG-generous; the kill_run worst case is
    // two helper attempts of up to 5 s plus ~51 scans; generous here with few processes).
    assert!(took < Duration::from_secs(45), "teardown took {took:?}: {record:?}");
    assert!(!t.residual_pids.is_empty(), "the escapee was not seen: {record:?}");
    // Reported clean only because it really is gone.
    assert!(t.uid_sweep_completed, "{record:?}");
    assert!(t.survivors.is_empty() && t.confirmed_empty && t.scratch_removed, "{record:?}");
    assert!(live_pids_of(fx.run_as.uid).is_empty(), "a uid {} process is alive", fx.run_as.uid);
    assert!(live_pids_of(fx.run_as.helper_uid()).is_empty(), "a helper is still alive");
    let escape = record.upper.join("work/escape");
    let before = fs::metadata(&escape).expect("the escapee did write").len();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(fs::metadata(&escape).unwrap().len(), before, "the upper layer is still being written");
}

#[test]
#[ignore = "needs root and the F-08 pinned kernel; see module docs"]
fn the_teardown_helper_runs_with_credentials_the_run_cannot_signal() {
    // Deterministic cover for the round-2 fix. The adversarial escapee test above passes even
    // with a helper the run *can* signal, because a single escapee rarely wins the race to
    // the helper's first kill pass. So observe the helper directly: while teardown runs, a
    // thread watches /proc for it. It must exist and read `Uid: h u h u` — real and saved uid
    // the run's helper uid, effective and filesystem uid the run uid — which a run process
    // (`u u u u`) is not permitted to signal.
    require_root();
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    let mut seen: Vec<String> = Vec::new();
    let mut expected = String::new();
    for _ in 0..3 {
        let fx = fixture();
        let (u, h) = (fx.run_as.uid, fx.run_as.helper_uid());
        expected = format!("{h} {u} {h} {u} | 0000000000000000 0000000000000000");
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut lines = Vec::new();
                while !stop.load(Ordering::SeqCst) {
                    for entry in fs::read_dir("/proc").unwrap().flatten() {
                        let name = entry.file_name();
                        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
                            continue;
                        }
                        let Ok(status) = fs::read_to_string(entry.path().join("status")) else {
                            continue;
                        };
                        let Some(uids) = status.lines().find(|l| l.starts_with("Uid:")) else {
                            continue;
                        };
                        let uids = uids.split_whitespace().skip(1).collect::<Vec<_>>().join(" ");
                        if uids.split(' ').any(|f| f == h.to_string()) {
                            let cap = |name: &str| {
                                status
                                    .lines()
                                    .find(|l| l.starts_with(name))
                                    .map_or_else(String::new, |l| l.split_whitespace().nth(1).unwrap_or("").to_owned())
                            };
                            lines.push(format!("{uids} | {} {}", cap("CapEff:"), cap("CapPrm:")));
                        }
                    }
                }
                lines
            })
        };
        let (sandbox, _stdio) =
            spawn(&fx.spec("/bin/sleep", &["100000"], Duration::from_secs(60))).expect("spawn");
        let record = sandbox.kill_and_wait().expect("kill_and_wait");
        stop.store(true, Ordering::SeqCst);
        seen.extend(watcher.join().unwrap());
        assert!(record.teardown.confirmed_empty && record.teardown.uid_sweep_completed, "{record:?}");
        if !seen.is_empty() {
            break;
        }
    }
    assert!(!seen.is_empty(), "no process with the helper uid was ever observed");
    for uids in &seen {
        assert_eq!(uids, &expected, "helper credentials (real effective saved fs | CapEff CapPrm)");
    }
}

#[test]
#[ignore = "needs root and the F-08 pinned kernel; see module docs"]
fn a_zombie_leader_with_a_live_thread_is_seen_killed_and_never_reported_clean() {
    // Round-3 review (HIGH): a run descendant starts a thread, then ends its main thread with a
    // raw SYS_exit. Its leader reads `State: Z` with `Threads: 2` and its ns link does not
    // resolve, while the thread keeps writing the upper layer. Every scan used to read that as
    // dead, giving `residual_pids: []` and a clean record. It must be seen, killed, and gone.
    require_root();
    let zl = zombie_leader_binary();
    let fx = fixture_with(|spec| {
        spec.add("dev/null", SpecNode::file_mode(0o666, ""));
        add_host_binary_at(spec, &zl, "/bin/zombie_leader");
    });
    let (sandbox, stdio) = spawn(&fx.sh(
        // Wait until the thread has written and the main thread has had time to exit (300 ms
        // after start), so the direct child leaves behind a zombie leader with a live thread.
        "/bin/zombie_leader /work/zl & \
         while [ ! -s /work/zl ]; do /bin/sleep 0.1; done; /bin/sleep 1; exit 0",
    ))
    .expect("spawn");
    drop(stdio);
    let record = sandbox.wait().expect("wait");
    let t = &record.teardown;

    assert!(!t.residual_pids.is_empty(), "the zombie leader with a live thread was missed: {record:?}");
    assert!(t.uid_sweep_completed && t.survivors.is_empty() && t.confirmed_empty, "{record:?}");
    assert!(live_pids_of(fx.run_as.uid).is_empty(), "a uid {} process is alive", fx.run_as.uid);
    let file = record.upper.join("work/zl");
    let before = fs::metadata(&file).expect("the thread did write").len();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(fs::metadata(&file).unwrap().len(), before, "the upper layer is still being written");
}

#[test]
#[ignore = "needs root and the F-08 pinned kernel; see module docs"]
fn a_descendant_with_a_non_utf8_name_is_seen_killed_and_never_reported_clean() {
    // Round-4 review (HIGH): `prctl(PR_SET_NAME, "\xff")` puts a raw 0xFF byte in the Name: line
    // of /proc/<pid>/status; the round-3 scanner read the status as UTF-8, failed, and treated
    // the process as absent — `residual_pids: []` and a clean record while it ran. It must be
    // seen (and verified, not merely unverifiable), killed, and gone.
    require_root();
    let on = example_binary("odd_name");
    let fx = fixture_with(|spec| {
        spec.add("dev/null", SpecNode::file_mode(0o666, ""));
        add_host_binary_at(spec, &on, "/bin/odd_name");
    });
    let (sandbox, stdio) = spawn(&fx.sh(
        "/bin/odd_name /work/on & \
         while [ ! -s /work/on ]; do /bin/sleep 0.1; done; /bin/sleep 0.5; exit 0",
    ))
    .expect("spawn");
    drop(stdio);
    let record = sandbox.wait().expect("wait");
    let t = &record.teardown;

    assert!(!t.residual_pids.is_empty(), "the 0xFF-named descendant was missed: {record:?}");
    assert!(t.unverified_pids.is_empty(), "it must be verified, not merely counted: {record:?}");
    assert!(t.uid_sweep_completed && t.survivors.is_empty() && t.confirmed_empty, "{record:?}");
    assert!(live_pids_of(fx.run_as.uid).is_empty(), "a uid {} process is alive", fx.run_as.uid);
    let file = record.upper.join("work/on");
    let before = fs::metadata(&file).expect("it did write").len();
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(fs::metadata(&file).unwrap().len(), before, "the upper layer is still being written");
}
