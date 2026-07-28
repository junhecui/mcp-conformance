//! P3-02: combine `sandbox::spawn` + `sandbox::NetworkBridge` (containment and interception,
//! P3-01/P3-02) with `observe::connection_log::ConnectionLog` (P3-02's evidence-harvesting
//! half) into the one place in this codebase permitted to depend on both — proving the full
//! pipeline delivers every connection a network-isolated sandboxed process attempts, logged
//! with its real destination, end to end through production code on both sides. Mirrors how
//! `arms`/`noise`/`idempotency` each combine `sandbox` and `observe` for their own P2 exit
//! criteria.
//!
//! **Must not:** decide what a logged destination *means* — in-sandbox versus external
//! classification is P3-04's job, and `openWorldHint` is P3-05's; this module only wires the
//! containment, interception, and evidence-harvesting halves together and returns what was
//! observed, uninterpreted.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use observe::connection_log::{ConnectionLog, ConnectionLogEntry};
use sandbox::{NetworkBridge, OverlaySpec, SandboxOutcome, SandboxSpec};

/// Why a network-isolated, bridged session failed.
#[derive(Debug)]
pub enum NetworkSessionError {
    /// Constructing or launching the sandbox failed.
    Sandbox(sandbox::SpawnError),
    /// Waiting on the sandboxed process failed.
    Wait(std::io::Error),
    /// Setting up or tearing down the veth bridge failed.
    Bridge(sandbox::NetnsError),
    /// Starting the connection log failed.
    ConnectionLog(observe::connection_log::ConnectionLogError),
    /// Writing the stdin release signal failed.
    Io(std::io::Error),
}

impl std::fmt::Display for NetworkSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sandbox(e) => write!(f, "failed to spawn the sandbox: {e}"),
            Self::Wait(e) => write!(f, "failed to wait on the sandboxed process: {e}"),
            Self::Bridge(e) => write!(f, "failed to bridge the sandboxed network namespace: {e}"),
            Self::ConnectionLog(e) => write!(f, "failed to start the connection log: {e}"),
            Self::Io(e) => write!(f, "failed to release the sandboxed process: {e}"),
        }
    }
}

impl std::error::Error for NetworkSessionError {}

impl From<sandbox::SpawnError> for NetworkSessionError {
    fn from(e: sandbox::SpawnError) -> Self {
        Self::Sandbox(e)
    }
}

impl From<sandbox::NetnsError> for NetworkSessionError {
    fn from(e: sandbox::NetnsError) -> Self {
        Self::Bridge(e)
    }
}

impl From<observe::connection_log::ConnectionLogError> for NetworkSessionError {
    fn from(e: observe::connection_log::ConnectionLogError) -> Self {
        Self::ConnectionLog(e)
    }
}

/// Run `program`/`args` inside a network-isolated sandbox (P3-01), bridged back to the host
/// (P3-02's `NetworkBridge`), and return both the ordinary [`SandboxOutcome`] and every
/// connection the sandboxed side attempted — in the order accepted, each with its real
/// destination recovered via `SO_ORIGINAL_DST` (`observe::connection_log`).
///
/// `stdin_release` is written to the sandboxed process's stdin (then stdin is closed) only
/// *after* the bridge is fully live — the sandboxed program is expected to block reading its
/// own stdin until this arrives, the same stdin-gating discipline `sandbox::netns`'s own
/// tests use, so no connection attempt can ever race the bridge's own setup.
///
/// # Errors
/// Any step failing: spawning the sandbox, starting the connection log, setting up or
/// tearing down the bridge, releasing the sandboxed process, or waiting on it.
pub fn run_network_isolated_and_bridged(
    overlay: OverlaySpec,
    program: PathBuf,
    args: Vec<String>,
    timeout: Duration,
    stdin_release: &[u8],
) -> Result<(SandboxOutcome, Vec<ConnectionLogEntry>), NetworkSessionError> {
    let spec = SandboxSpec { overlay, program, args, timeout, network_isolated: true };
    let (handle, mut stdin, _stdout) = sandbox::spawn(&spec)?;

    let log = ConnectionLog::start()?;
    let bridge = NetworkBridge::set_up(handle.init_pid(), log.port())?;

    stdin.write_all(stdin_release).map_err(NetworkSessionError::Io)?;
    drop(stdin);

    let outcome = handle.wait().map_err(NetworkSessionError::Wait)?;
    let entries = log.stop();
    bridge.teardown()?;

    Ok((outcome, entries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandbox::build;
    use std::net::Ipv4Addr;

    /// P3-02's literal exit criterion, proven through production code on both sides of the
    /// bridge (not the bare-listener proof `sandbox::netns`'s own test already gives the
    /// plumbing half in isolation): a network-isolated sandboxed process attempts connections
    /// to *several distinct* external destinations, and every one of them is logged with its
    /// real, correct destination — including the port, which only `SO_ORIGINAL_DST` (not a
    /// connection's local peer address, which is always the proxy) can recover.
    #[test]
    fn every_connection_the_sandboxed_process_attempts_is_logged_with_its_real_destination() {
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

        let script = "\
import socket, sys
sys.stdin.readline()
for host, port in [('93.184.216.34', 80), ('192.0.2.1', 443), ('192.0.2.1', 8080)]:
    try:
        socket.create_connection((host, port), timeout=5)
    except OSError:
        pass
print('done')
";

        let (outcome, entries) = run_network_isolated_and_bridged(
            overlay,
            PathBuf::from("/usr/local/bin/python3"),
            vec!["-c".to_string(), script.to_string()],
            Duration::from_secs(10),
            b"go\n",
        )
        .expect("run network-isolated and bridged session");

        assert!(!outcome.timed_out);
        assert!(outcome.network_isolated);

        let destinations: Vec<_> = entries.iter().map(|e| e.destination).collect();
        assert_eq!(
            destinations,
            vec![
                std::net::SocketAddrV4::new(Ipv4Addr::new(93, 184, 216, 34), 80),
                std::net::SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 443),
                std::net::SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 8080),
            ],
            "every attempted connection must be logged, in order, with its real destination \
             including port: {destinations:?}"
        );
    }
}
