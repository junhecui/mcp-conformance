//! P3-03: combine `sandbox::spawn` + `sandbox::NetworkBridge` (containment and interception,
//! P3-01/P3-02) with `world::mock_backend::GenericMockBackend` (P3-03's mock-serving half)
//! against a real sandboxed process — proving a tool that believes it is calling a real
//! external API is transparently served a valid response instead, through production code on
//! both sides. Mirrors `network`'s own combination of `sandbox` and `observe` for P3-02.
//!
//! **Must not:** decide what being served (or not) means for a verdict — that is P3-05's
//! `openWorldHint` job; this module only wires the containment, interception, and
//! mock-serving halves together and returns what the sandboxed process itself observed.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use sandbox::{NetworkBridge, OverlaySpec, SandboxOutcome, SandboxSpec};
use world::mock_backend::GenericMockBackend;

/// Why a network-isolated, mocked session failed.
#[derive(Debug)]
pub enum MockSessionError {
    /// Constructing or launching the sandbox failed.
    Sandbox(sandbox::SpawnError),
    /// Waiting on the sandboxed process failed.
    Wait(std::io::Error),
    /// Setting up or tearing down the veth bridge failed.
    Bridge(sandbox::NetnsError),
    /// Starting the mock backend failed.
    MockBackend(world::mock_backend::MockBackendError),
    /// Releasing the sandboxed process, or reading its stdout, failed.
    Io(std::io::Error),
}

impl std::fmt::Display for MockSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sandbox(e) => write!(f, "failed to spawn the sandbox: {e}"),
            Self::Wait(e) => write!(f, "failed to wait on the sandboxed process: {e}"),
            Self::Bridge(e) => write!(f, "failed to bridge the sandboxed network namespace: {e}"),
            Self::MockBackend(e) => write!(f, "failed to start the mock backend: {e}"),
            Self::Io(e) => write!(f, "failed to interact with the sandboxed process: {e}"),
        }
    }
}

impl std::error::Error for MockSessionError {}

impl From<sandbox::SpawnError> for MockSessionError {
    fn from(e: sandbox::SpawnError) -> Self {
        Self::Sandbox(e)
    }
}

impl From<sandbox::NetnsError> for MockSessionError {
    fn from(e: sandbox::NetnsError) -> Self {
        Self::Bridge(e)
    }
}

impl From<world::mock_backend::MockBackendError> for MockSessionError {
    fn from(e: world::mock_backend::MockBackendError) -> Self {
        Self::MockBackend(e)
    }
}

/// Run `program`/`args` inside a network-isolated sandbox (P3-01), bridged back to a
/// [`GenericMockBackend`] (P3-03) instead of `observe::connection_log`'s recording-only
/// listener, and return both the ordinary [`SandboxOutcome`] and everything the sandboxed
/// process wrote to its own stdout — the direct, first-hand evidence of what it saw when it
/// tried to reach what it believed was a real external API.
///
/// `stdin_release` is written to the sandboxed process's stdin (then stdin is closed) only
/// *after* the bridge is live, the same stdin-gating discipline `network`'s own
/// `run_network_isolated_and_bridged` uses, so no connection attempt can race the bridge's
/// own setup.
///
/// # Errors
/// Any step failing: spawning the sandbox, starting the mock backend, setting up or tearing
/// down the bridge, releasing the sandboxed process, reading its stdout, or waiting on it.
pub fn run_network_isolated_and_mocked(
    overlay: OverlaySpec,
    program: PathBuf,
    args: Vec<String>,
    timeout: Duration,
    stdin_release: &[u8],
) -> Result<(SandboxOutcome, String), MockSessionError> {
    let spec = SandboxSpec { overlay, program, args, timeout, network_isolated: true };
    let (handle, mut stdin, mut stdout) = sandbox::spawn(&spec)?;

    let backend = GenericMockBackend::start()?;
    let bridge = NetworkBridge::set_up(handle.init_pid(), backend.port())?;

    stdin.write_all(stdin_release).map_err(MockSessionError::Io)?;
    drop(stdin);

    let mut output = String::new();
    stdout.read_to_string(&mut output).map_err(MockSessionError::Io)?;
    let outcome = handle.wait().map_err(MockSessionError::Wait)?;
    backend.stop();
    bridge.teardown()?;

    Ok((outcome, output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandbox::build;

    /// P3-03's literal exit criterion, proven through production code on both sides: a
    /// network-isolated sandboxed process makes a real HTTP request to what it believes is
    /// an arbitrary external API (`93.184.216.34`, the same address `network`'s own P3-02
    /// test uses, reused rather than inventing a second "arbitrary external host") and is
    /// transparently served the generic mock's response — the tool never sees a refused or
    /// hanging connection, and the JSON it receives is exactly the generic fixture's own
    /// seeded content.
    #[test]
    fn a_sandboxed_http_request_to_an_external_api_is_transparently_served_by_the_generic_mock()
    {
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
import sys, urllib.request
sys.stdin.readline()
with urllib.request.urlopen('http://93.184.216.34/items', timeout=5) as response:
    assert response.status == 200, response.status
    print(response.read().decode('utf-8'), end='')
";

        let (outcome, output) = run_network_isolated_and_mocked(
            overlay,
            PathBuf::from("/usr/local/bin/python3"),
            vec!["-c".to_string(), script.to_string()],
            Duration::from_secs(10),
            b"go\n",
        )
        .expect("run network-isolated and mocked session");

        assert!(!outcome.timed_out, "sandboxed output: {output}");
        assert!(outcome.network_isolated);
        assert_eq!(
            output,
            "[{\"id\":1,\"name\":\"alpha\",\"value\":\"seed-value-1\"},\
{\"id\":2,\"name\":\"beta\",\"value\":\"seed-value-2\"},\
{\"id\":3,\"name\":\"gamma\",\"value\":\"seed-value-3\"}]",
            "the sandboxed process must receive exactly the generic mock's own response body"
        );
    }
}
