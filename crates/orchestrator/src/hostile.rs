//! P4-05: a deliberately hostile sandboxed process attempting escape (P4-01/02),
//! exfiltration (P3-01/02), resource exhaustion (P2-02), and a hang (P1-03) — all in one
//! run — proving every one of them is contained *and* appears in evidence. This is Phase
//! 4's own exit criterion, combining every containment and evidence-harvesting mechanism
//! this project has built so far in a single real sandboxed process, not four separate
//! demonstrations run in isolation.
//!
//! # Observation evasion is explicitly out of scope (design.md §8)
//!
//! This module's hostile script makes no attempt to detect or evade the fact that it is
//! being observed or sandboxed (timing side channels, checking for a tracer, probing for
//! seccomp, and so on). Containing and recording an overt attack is what this harness
//! claims to do; detecting a sufficiently sophisticated attempt to evade *observation
//! itself* is a different, harder problem this project has not solved and does not claim
//! to. [`HostileRunReport`]'s own doc comment repeats this so it travels with the evidence,
//! not just this module's source.
//!
//! # Why `target_pid`, not `init_pid`, gets added to the cgroup
//!
//! `sandbox::Cgroup::add_process`'s own doc comment recommends adding the sandbox's
//! outermost process (fork-1, `init_pid`) *before its own second fork*, so every descendant
//! automatically inherits membership. That advice is for a caller driving `fork`/`exec`
//! directly; by the time `sandbox::spawn` itself returns a handle, fork-1's second fork (the
//! real target, fork-2) has already happened — adding `init_pid` at that point would be too
//! late to cover fork-2, since cgroup membership is never applied retroactively to an
//! already-existing process. What actually needs cgroup membership is the real target
//! itself (`SandboxHandle::target_pid`, P4-02's own addition, for an unrelated reason) —
//! added to the cgroup *before* the stdin-gated hostile script is released, so every child
//! it (not `init_pid`) ever forks inherits membership correctly.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use observe::connection_log::{ConnectionLog, ConnectionLogEntry};
use observe::seccomp_audit::{SeccompAudit, SeccompDenialEntry};
use sandbox::{Cgroup, NetworkBridge, OverlaySpec, ResourceLimits, ResourceUsage, SandboxOutcome, SandboxSpec};

/// Why the hostile-server run failed to even produce a report (containment failing to hold
/// is not this — that would show up *inside* a successfully-produced report instead).
#[derive(Debug)]
pub enum HostileRunError {
    /// Constructing or launching the sandbox failed.
    Sandbox(sandbox::SpawnError),
    /// Waiting on the sandboxed process failed.
    Wait(std::io::Error),
    /// Starting the seccomp audit harvester failed.
    SeccompAudit(observe::seccomp_audit::SeccompAuditError),
    /// Starting the connection log failed.
    ConnectionLog(observe::connection_log::ConnectionLogError),
    /// Setting up or tearing down the veth bridge failed.
    Bridge(sandbox::NetnsError),
    /// Creating, attaching to, or removing the cgroup failed.
    Cgroup(sandbox::CgroupError),
    /// Releasing the sandboxed process, or reading its stdout, failed.
    Io(std::io::Error),
}

impl std::fmt::Display for HostileRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sandbox(e) => write!(f, "failed to spawn the sandbox: {e}"),
            Self::Wait(e) => write!(f, "failed to wait on the sandboxed process: {e}"),
            Self::SeccompAudit(e) => write!(f, "failed to start the seccomp audit: {e}"),
            Self::ConnectionLog(e) => write!(f, "failed to start the connection log: {e}"),
            Self::Bridge(e) => write!(f, "failed to bridge the sandboxed network namespace: {e}"),
            Self::Cgroup(e) => write!(f, "failed to manage the cgroup: {e}"),
            Self::Io(e) => write!(f, "failed to interact with the sandboxed process: {e}"),
        }
    }
}

impl std::error::Error for HostileRunError {}

impl From<sandbox::SpawnError> for HostileRunError {
    fn from(e: sandbox::SpawnError) -> Self {
        Self::Sandbox(e)
    }
}
impl From<observe::seccomp_audit::SeccompAuditError> for HostileRunError {
    fn from(e: observe::seccomp_audit::SeccompAuditError) -> Self {
        Self::SeccompAudit(e)
    }
}
impl From<observe::connection_log::ConnectionLogError> for HostileRunError {
    fn from(e: observe::connection_log::ConnectionLogError) -> Self {
        Self::ConnectionLog(e)
    }
}
impl From<sandbox::NetnsError> for HostileRunError {
    fn from(e: sandbox::NetnsError) -> Self {
        Self::Bridge(e)
    }
}
impl From<sandbox::CgroupError> for HostileRunError {
    fn from(e: sandbox::CgroupError) -> Self {
        Self::Cgroup(e)
    }
}

/// Every evidence surface this project has built, over one hostile run — architecture.md's
/// own list ("Harvest upper layer, conntrack log, seccomp audit log, cgroup counters, exit
/// status") made literal.
///
/// **Observation evasion is out of scope** (design.md §8) — see this module's own doc
/// comment. Nothing here claims to detect a hostile server that specifically tries to evade
/// being observed; it claims only that an overt attempt, once made, is contained and
/// recorded.
pub struct HostileRunReport {
    /// The sandbox's own outcome: exit status, whether the hard timeout fired.
    pub outcome: SandboxOutcome,
    /// Every syscall `observe::seccomp_audit` harvested as denied.
    pub seccomp_denials: Vec<SeccompDenialEntry>,
    /// Every connection attempt `observe::connection_log` observed, with its real
    /// destination.
    pub connection_attempts: Vec<ConnectionLogEntry>,
    /// What the cgroup actually recorded for this run.
    pub cgroup_usage: ResourceUsage,
    /// How many entries the overlay's upper layer contains — the filesystem evidence
    /// surface, proving a hostile process's ordinary writes are captured too, not just its
    /// more dramatic attempts.
    pub upper_layer_entry_count: usize,
    /// Everything the sandboxed process itself wrote to its own stdout before being killed
    /// or exiting — not one of architecture.md's own named evidence surfaces, but real,
    /// direct evidence of what the process did and in what order, kept here because
    /// discarding it would be strictly less honest than keeping it.
    pub stdout: String,
}

/// Run a hostile script inside the full containment stack (network isolation + veth bridge,
/// seccomp filter + audit, and a cgroup) and return every evidence surface it produced.
///
/// # Errors
/// Any containment or harvesting step failing outright (a contained *attempt* by the
/// sandboxed process itself is never an `Err` here — it shows up in the returned report).
pub fn run_hostile_script(
    overlay: OverlaySpec,
    script: &str,
    timeout: Duration,
    limits: ResourceLimits,
) -> Result<HostileRunReport, HostileRunError> {
    // Started before spawn, per `SeccompAudit::start`'s own doc comment: only records from
    // this point on are guaranteed to still be in the kernel's buffer once harvested.
    let seccomp_audit = SeccompAudit::start()?;

    let spec = SandboxSpec {
        overlay: overlay.clone(),
        program: PathBuf::from("/usr/local/bin/python3"),
        args: vec!["-c".to_string(), script.to_string()],
        timeout,
        network_isolated: true,
    };
    let (handle, mut stdin, mut stdout) = sandbox::spawn(&spec)?;
    let target_pid = handle.target_pid();

    let connection_log = ConnectionLog::start()?;
    let bridge = NetworkBridge::set_up(handle.init_pid(), connection_log.port())?;

    let cgroup_name = format!("hostile-{}", target_pid.as_raw());
    let cgroup = Cgroup::create(
        &cgroup_name,
        std::path::Path::new(Cgroup::DEFAULT_V2_ROOT),
        std::path::Path::new(Cgroup::DEFAULT_V2_ROOT),
        &limits,
    )?;
    // See this module's own doc comment for why this is `target_pid`, not `init_pid`.
    cgroup.add_process(target_pid)?;

    // Only now — bridge, connection log, and cgroup are all armed — release the
    // stdin-gated script, the same discipline every other multi-mechanism test in this
    // codebase already uses to avoid racing its own setup.
    stdin.write_all(b"go\n").map_err(HostileRunError::Io)?;
    drop(stdin);

    let mut output = String::new();
    stdout.read_to_string(&mut output).map_err(HostileRunError::Io)?;
    let outcome = handle.wait().map_err(HostileRunError::Wait)?;

    let seccomp_denials = seccomp_audit.stop(target_pid.as_raw());
    let connection_attempts = connection_log.stop();
    bridge.teardown()?;
    let cgroup_usage = cgroup.usage();
    cgroup.remove()?;

    let upper_layer_bytes = observe::evtree::capture(&outcome.upper).map_err(HostileRunError::Io)?;
    let upper_layer_entry_count = observe::evtree::decode(&upper_layer_bytes)
        .map(|raw| raw.entries.len())
        .unwrap_or(0);

    Ok(HostileRunReport {
        outcome,
        seccomp_denials,
        connection_attempts,
        cgroup_usage,
        upper_layer_entry_count,
        stdout: output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandbox::build;

    /// Phase 4's own exit criterion, literally: a single hostile process attempts escape,
    /// exfiltration, resource exhaustion, a filesystem write, and finally hangs — every one
    /// of them contained, and every one of them appearing in the returned evidence report.
    #[test]
    fn a_hostile_process_is_contained_and_every_attempt_appears_in_evidence() {
        let _slot = crate::arms::tests_support::take_sandbox_slot();
        let lower_dir = tempfile::tempdir().expect("tempdir");
        build(lower_dir.path(), &[]).expect("build base layer");
        let scratch = tempfile::tempdir().expect("tempdir");
        let overlay = OverlaySpec {
            lower: lower_dir.path().to_path_buf(),
            upper: scratch.path().join("upper"),
            work: scratch.path().join("work"),
            mountpoint: scratch.path().join("merged"),
        };

        const SYS_PTRACE: i64 = 101; // x86_64 SYS_ptrace
        const FORK_ATTEMPTS: u32 = 50;
        let script = format!(
            "\
import sys, os, socket, ctypes, time
sys.stdin.readline()

# 1. Escape: an escape-class syscall the seccomp filter (P4-01) must deny.
libc = ctypes.CDLL(None, use_errno=True)
libc.syscall({SYS_PTRACE}, 0, 0, 0, 0, 0, 0)
print('escape attempted', flush=True)

# 2. Exfiltration: an outbound connection the network bridge (P3-02) must capture.
try:
    socket.create_connection(('93.184.216.34', 443), timeout=5)
except OSError:
    pass
print('exfiltration attempted', flush=True)

# 3. An ordinary write, to prove the upper-layer evidence surface still captures whatever a
# hostile process does alongside its more dramatic attempts, not only those.
with open('hostile_output.txt', 'w') as f:
    f.write('written by a hostile tool')
print('wrote a file', flush=True)

# 4. Resource exhaustion: a bounded fork bomb the cgroup (P2-02) must cap.
children = 0
for _ in range({FORK_ATTEMPTS}):
    try:
        pid = os.fork()
    except OSError:
        break
    if pid == 0:
        os._exit(0)
    children += 1
for _ in range(children):
    try:
        os.wait()
    except OSError:
        pass
print(f'fork bomb attempted: {{children}} children created', flush=True)

# 5. A hang: the sandbox's own hard timeout (P1-03) must kill this.
print('hanging now', flush=True)
while True:
    time.sleep(1)
"
        );

        let limits = ResourceLimits { memory_max_bytes: 256 * 1024 * 1024, pids_max: 8, cpu_fraction: 1.0 };
        let report = run_hostile_script(overlay, &script, Duration::from_secs(5), limits)
            .expect("run hostile script");

        // --- Contained ---
        assert!(report.outcome.timed_out, "the hang must be killed by the hard timeout");
        assert!(
            !report.outcome.exit_status.is_some_and(|s| s.success()),
            "a timed-out, SIGKILLed process must never report success"
        );

        // --- Escape: contained and in evidence ---
        assert!(
            report.seccomp_denials.iter().any(|d| d.syscall_nr == SYS_PTRACE),
            "the ptrace attempt must be harvested into evidence, got: {:?}",
            report.seccomp_denials
        );

        // --- Exfiltration: contained (redirected, never actually left the sandbox's own
        // bridge) and in evidence, with the real destination the script tried to reach.
        assert!(
            report
                .connection_attempts
                .iter()
                .any(|c| c.destination.port() == 443),
            "the exfiltration attempt must be harvested into evidence, got: {:?}",
            report.connection_attempts
        );

        // --- Resource exhaustion: contained (fewer children than attempted) ---
        // The script's own self-report of how many forks actually succeeded is the direct
        // signal the cap did something, matching `pids_max_contains_a_fork_bomb`'s own
        // reasoning in `sandbox::cgroup`'s tests, over the real stdout this run produced.
        let fork_line = report
            .stdout
            .lines()
            .find(|line| line.starts_with("fork bomb attempted:"))
            .unwrap_or_else(|| panic!("no fork-bomb report line in stdout: {:?}", report.stdout));
        let children: u32 = fork_line
            .rsplit_once(':')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("could not parse child count from: {fork_line:?}"));
        assert!(
            children < FORK_ATTEMPTS,
            "the fork bomb must be capped well short of the {FORK_ATTEMPTS} attempted, got: \
             {fork_line:?}"
        );

        // --- Filesystem write: in evidence, alongside everything else ---
        assert!(
            report.upper_layer_entry_count > 0,
            "the ordinary file write must still appear in the upper-layer evidence surface"
        );

        // --- Every earlier step actually ran, in order, before the hang took over ---
        assert!(report.stdout.contains("escape attempted"), "stdout: {:?}", report.stdout);
        assert!(report.stdout.contains("exfiltration attempted"), "stdout: {:?}", report.stdout);
        assert!(report.stdout.contains("wrote a file"), "stdout: {:?}", report.stdout);
        assert!(report.stdout.contains("hanging now"), "stdout: {:?}", report.stdout);
    }
}
