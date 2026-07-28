//! P4-02: combine `sandbox`'s seccomp-bpf filter (P4-01, applied to every `spawn()` call
//! unconditionally) with `observe::seccomp_audit` (P4-02's harvesting half) and surface the
//! result to `integrity::decide` — proving the full pipeline architecture.md §5.1's `G4`
//! branch depends on, end to end, against a real sandboxed process.
//!
//! **Must not:** decide what a denial *means* beyond what `integrity::decide` itself already
//! encodes (architecture.md §5.1: an escape-class denial is accepted evidence, flagged, never
//! rejecting evidence on its own) — this module only wires the containment, harvesting, and
//! gate-decision pieces together.

use std::path::PathBuf;
use std::time::Duration;

use integrity::GateOutcome;
use observe::seccomp_audit::{SeccompAudit, SeccompDenialEntry};
use sandbox::{OverlaySpec, SandboxOutcome, SandboxSpec};

/// Why a seccomp-instrumented run failed.
#[derive(Debug)]
pub enum SeccompSessionError {
    /// Constructing or launching the sandbox failed.
    Sandbox(sandbox::SpawnError),
    /// Waiting on the sandboxed process failed.
    Wait(std::io::Error),
    /// Starting the seccomp audit harvester failed.
    Audit(observe::seccomp_audit::SeccompAuditError),
    /// Releasing the sandboxed process, or reading its stdout, failed.
    Io(std::io::Error),
}

impl std::fmt::Display for SeccompSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sandbox(e) => write!(f, "failed to spawn the sandbox: {e}"),
            Self::Wait(e) => write!(f, "failed to wait on the sandboxed process: {e}"),
            Self::Audit(e) => write!(f, "failed to start the seccomp audit: {e}"),
            Self::Io(e) => write!(f, "failed to interact with the sandboxed process: {e}"),
        }
    }
}

impl std::error::Error for SeccompSessionError {}

impl From<sandbox::SpawnError> for SeccompSessionError {
    fn from(e: sandbox::SpawnError) -> Self {
        Self::Sandbox(e)
    }
}

impl From<observe::seccomp_audit::SeccompAuditError> for SeccompSessionError {
    fn from(e: observe::seccomp_audit::SeccompAuditError) -> Self {
        Self::Audit(e)
    }
}

/// Run `program`/`args` inside a real sandbox (P4-01's seccomp filter applies
/// unconditionally, no opt-in needed), harvest every escape-class denial the run produced
/// (P4-02), and decide the integrity gate's outcome from it (architecture.md §5.1's `G4`).
///
/// `stdin_release` is written to the sandboxed process's stdin (then stdin is closed) —
/// pass an empty slice for a program that reads nothing from stdin before acting.
///
/// # Errors
/// Any step failing: spawning the sandbox, starting the seccomp audit, releasing the
/// sandboxed process, reading its stdout, or waiting on it.
pub fn run_and_assess_containment(
    overlay: OverlaySpec,
    program: PathBuf,
    args: Vec<String>,
    timeout: Duration,
    stdin_release: &[u8],
) -> Result<(SandboxOutcome, Vec<SeccompDenialEntry>, GateOutcome), SeccompSessionError> {
    use std::io::{Read, Write};

    let spec = SandboxSpec { overlay, program, args, timeout, network_isolated: false };

    // Start the audit *before* spawning — see `SeccompAudit::start`'s own doc comment for
    // why: it must be positioned at "only new records" before the sandboxed process (whose
    // denials this run cares about) ever runs.
    let audit = SeccompAudit::start()?;

    let (handle, mut stdin, mut stdout) = sandbox::spawn(&spec)?;
    let target_pid = handle.target_pid();

    stdin.write_all(stdin_release).map_err(SeccompSessionError::Io)?;
    drop(stdin);

    let mut output = String::new();
    stdout.read_to_string(&mut output).map_err(SeccompSessionError::Io)?;
    let outcome = handle.wait().map_err(SeccompSessionError::Wait)?;

    let denials = audit.stop(target_pid.as_raw());

    let signals = integrity::RunSignals {
        timed_out: outcome.timed_out,
        // Neither of this module's own concern — containment teardown and resource caps are
        // P2-01/P2-02's own signals, orthogonal to seccomp denial. Always `false` here, the
        // same disclosed-gap posture `xtask::first_verdict` already used for
        // `resource_cap_hit` before P2-04 wired in a real `Cgroup`.
        containment_uncertain: !outcome.orphans_impossible,
        resource_cap_hit: false,
        escape_class_syscall_denied: !denials.is_empty(),
    };
    let gate_outcome = integrity::decide(signals);

    Ok((outcome, denials, gate_outcome))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandbox::build;

    fn overlay(scratch: &std::path::Path, lower: &std::path::Path) -> OverlaySpec {
        OverlaySpec {
            lower: lower.to_path_buf(),
            upper: scratch.join("upper"),
            work: scratch.join("work"),
            mountpoint: scratch.join("merged"),
        }
    }

    fn run(script: &str) -> (SandboxOutcome, Vec<SeccompDenialEntry>, GateOutcome) {
        let _slot = crate::arms::tests_support::take_sandbox_slot();
        let lower_dir = tempfile::tempdir().expect("tempdir");
        build(lower_dir.path(), &[]).expect("build base layer");
        let scratch = tempfile::tempdir().expect("tempdir");

        run_and_assess_containment(
            overlay(scratch.path(), lower_dir.path()),
            PathBuf::from("/usr/local/bin/python3"),
            vec!["-c".to_string(), script.to_string()],
            Duration::from_secs(10),
            b"",
        )
        .expect("run and assess containment")
    }

    /// P4-02's own exit criterion, proven end to end against a real hostile-shaped sandboxed
    /// process: an attempted `ptrace` (an escape-class syscall P4-01's filter denies) is
    /// harvested into evidence by `observe::seccomp_audit` and reaches the integrity gate as
    /// `Accept { adversarial_flag: true }` — accepted evidence, flagged, never rejected
    /// (architecture.md §5.1: an attempted escape is among the most interesting findings
    /// this harness can produce).
    #[test]
    fn a_real_escape_attempt_is_harvested_and_flags_the_gate_outcome() {
        const SYS_PTRACE: i64 = 101; // x86_64 SYS_ptrace
        let script = "\
import ctypes
libc = ctypes.CDLL(None, use_errno=True)
libc.syscall(101, 0, 0, 0, 0, 0, 0)  # SYS_ptrace
";
        let (outcome, denials, gate_outcome) = run(script);

        assert!(!outcome.timed_out);
        assert!(
            !denials.is_empty(),
            "a real ptrace attempt must be harvested into at least one denial entry"
        );
        assert!(
            denials.iter().any(|d| d.syscall_nr == SYS_PTRACE),
            "the harvested denial must name ptrace ({SYS_PTRACE}), got: {denials:?}"
        );
        assert_eq!(gate_outcome, GateOutcome::Accept { adversarial_flag: true });
    }

    /// A clean run that never attempts an escape-class syscall must reach the gate
    /// unflagged — proving this pipeline doesn't manufacture a denial where none occurred.
    #[test]
    fn a_clean_run_is_accepted_and_unflagged() {
        let (outcome, denials, gate_outcome) = run("print('hello from a clean run')");

        assert!(!outcome.timed_out);
        assert!(denials.is_empty(), "a clean run must harvest no denials, got: {denials:?}");
        assert_eq!(gate_outcome, GateOutcome::Accept { adversarial_flag: false });
    }

    /// P4-03's own exit criterion, proven for the flagged case specifically (`first_verdict`'s
    /// own real run only ever exercises the *unflagged* path, since `echo` never attempts an
    /// escape): a real hostile-shaped process's `adversarial_flag = true` is written to a
    /// real `INTEGRITY` row and read back exactly as `true`, plus the exact syscall number
    /// harvested — proving the published value traces through the database, not a
    /// stand-alone in-memory copy that only happens to agree with it.
    #[test]
    fn a_flagged_run_persists_and_reads_back_true_through_the_integrity_table() {
        const SYS_PTRACE: i64 = 101; // x86_64 SYS_ptrace
        let script = "\
import ctypes
libc = ctypes.CDLL(None, use_errno=True)
libc.syscall(101, 0, 0, 0, 0, 0, 0)  # SYS_ptrace
";
        let (outcome, denials, gate_outcome) = run(script);
        let adversarial_flag =
            matches!(gate_outcome, GateOutcome::Accept { adversarial_flag: true });
        assert!(adversarial_flag, "the in-memory gate outcome must already be flagged");

        let conn = store::db::open_and_migrate(":memory:").expect("open_and_migrate");
        store::db::insert_server(
            &conn,
            &store::db::ServerRecord {
                server_id: "srv-hostile",
                source_uri: "test://hostile",
                containability_class: datamodel::ContainabilityClass::A,
                spec_revision: "2025-11-25",
            },
        )
        .expect("insert_server");
        store::db::insert_tool_snapshot(
            &conn,
            &store::db::ToolSnapshotRecord {
                snapshot_id: "snap-hostile",
                server_id: "srv-hostile",
                tool_name: "hostile-tool",
                metadata_pin: "test-pin",
                annotations_raw: "null",
                readonly_explicit: false,
                destructive_explicit: false,
                idempotent_explicit: false,
                openworld_explicit: false,
                observed_at: "unix:0",
            },
        )
        .expect("insert_tool_snapshot");
        store::db::insert_run(
            &conn,
            &store::db::RunRecord {
                run_id: "run-hostile",
                snapshot_id: "snap-hostile",
                arm: "1",
                fixture_id: None,
                arguments: "{}",
                harness_version: env!("CARGO_PKG_VERSION"),
                started_at: "unix:0",
            },
        )
        .expect("insert_run");
        store::db::insert_integrity(
            &conn,
            &store::db::IntegrityRecord {
                run_id: "run-hostile",
                clean_teardown: true,
                caps_respected: true,
                timed_out: outcome.timed_out,
                denied_syscalls: &serde_json::to_string(
                    &denials.iter().map(|d| d.syscall_nr).collect::<Vec<_>>(),
                )
                .expect("serialise denied syscalls"),
                adversarial_flag,
            },
        )
        .expect("insert_integrity");

        let row = store::db::get_integrity(&conn, "run-hostile")
            .expect("get_integrity")
            .expect("row was just inserted");
        assert!(row.adversarial_flag, "the published record must read back flagged");
        let stored_syscalls: Vec<i64> =
            serde_json::from_str(&row.denied_syscalls).expect("parse stored denied_syscalls");
        assert!(
            stored_syscalls.contains(&SYS_PTRACE),
            "the stored denied_syscalls must include ptrace ({SYS_PTRACE}): {stored_syscalls:?}"
        );
    }
}
