//! P2-07: execute the arm shapes architecture.md §4.1 assigns beyond the single `Arm 1`
//! P1-08 already demonstrated end to end — `Arm 1'` (single call, independent repeat),
//! `Arm 2` (double call, same process), and `Arm 2R` (call, restart, call again) — and prove
//! each produces a genuinely independent changeset.
//!
//! # Why arguments are never resynthesised here
//!
//! `argsynth::synthesize` is deterministic (P2-06), so a caller wanting the identical
//! arguments architecture.md §4.2's noise floor requires across arms simply synthesises
//! once and passes the same [`serde_json::Value`] into every [`ArmProgram`] below — this
//! module never resynthesises, perturbs, or otherwise touches the arguments it's handed.
//!
//! # How `Arm 2R`'s "restart" actually works
//!
//! [`run_arm_2r`] calls [`sandbox::spawn`] *twice* against the exact same [`OverlaySpec`]
//! (same `lower`/`upper`/`work`/`mountpoint`). Each `spawn` call unshares its own private
//! mount namespace and mounts the overlay fresh — but `upperdir` is a real, host-filesystem
//! directory that outlives any one mount namespace, so the second `spawn`'s mount starts
//! from exactly whatever the first process's run left in `upper`, plus the same read-only
//! `lower`. This is "restart" in the architecture.md §4.2 sense — a genuinely new process,
//! continuing from the first one's accumulated on-disk effect — verified directly by this
//! module's own tests, not assumed to work from how overlayfs happens to be documented.
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use discovery::jsonrpc;
use sandbox::{OverlaySpec, SandboxSpec};
use serde_json::{json, Value};

/// Why running an arm failed.
#[derive(Debug)]
pub enum ArmError {
    /// A filesystem operation failed.
    Io(std::io::Error),
    /// Constructing or launching the sandbox failed.
    Sandbox(sandbox::SpawnError),
    /// A JSON message wasn't the expected shape.
    Json(serde_json::Error),
    /// The sandboxed program's JSON-RPC responses didn't match this module's expectations.
    Protocol(String),
    /// Storing or reading back evidence failed.
    Store(store::StoreError),
    /// Decoding the stored `evtree1` capture failed.
    Decode(String),
}

impl std::fmt::Display for ArmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Sandbox(e) => write!(f, "sandbox error: {e}"),
            Self::Json(e) => write!(f, "JSON error: {e}"),
            Self::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Self::Store(e) => write!(f, "evidence store error: {e}"),
            Self::Decode(msg) => write!(f, "evidence decode error: {msg}"),
        }
    }
}

impl std::error::Error for ArmError {}

impl From<std::io::Error> for ArmError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<sandbox::SpawnError> for ArmError {
    fn from(e: sandbox::SpawnError) -> Self {
        Self::Sandbox(e)
    }
}
impl From<serde_json::Error> for ArmError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
impl From<store::StoreError> for ArmError {
    fn from(e: store::StoreError) -> Self {
        Self::Store(e)
    }
}

/// Everything common to every arm below: the program to launch and the one MCP tool call to
/// make against it, already fully resolved — schema discovery (`discovery`) and argument
/// synthesis (`argsynth`) both happen upstream of this module, never inside it.
pub struct ArmProgram {
    /// The base layer (P1-02) every arm mounts as its overlay's read-only `lower`.
    pub base_layer: PathBuf,
    /// The program to exec inside the sandbox.
    pub program: PathBuf,
    /// Arguments to `program`.
    pub args: Vec<String>,
    /// The MCP tool name to call.
    pub tool_name: String,
    /// The `arguments` object to send with every `tools/call` — identical across every call
    /// this module makes, by construction (see this module's own doc comment).
    pub arguments: Value,
    /// Hard wall-clock deadline per sandboxed process.
    pub timeout: Duration,
}

/// One arm's outcome: the overlay's upper layer on the host (evidence, per P1-04) and the
/// changeset `observe::evtree` decoded from it.
#[derive(Debug)]
pub struct ArmRun {
    /// The overlay's upper directory, on the host filesystem, exactly as the run left it.
    pub upper: PathBuf,
    /// The decoded changeset.
    pub evidence: datamodel::RawEvidence,
}

/// A tiny, one-off newline-delimited JSON-RPC round trip over one sandboxed process's own
/// pipes — the same shape `xtask::first_verdict::RawClient` uses, kept as its own local copy
/// here rather than shared: encoding a message has no execution semantics (see
/// `discovery::jsonrpc`'s own doc comment on why `encode_request`/`encode_notification` are
/// already `pub` for exactly this kind of reuse), but the *session* built around it — how
/// many calls to make, over which pipes, torn down when — differs enough per call site that
/// a shared abstraction would be more indirection than the ~30 lines it replaces.
struct RawClient {
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl RawClient {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, ArmError> {
        let id = self.next_id;
        self.next_id += 1;
        let bytes = jsonrpc::encode_request(id, method, params);
        self.stdin.write_all(&bytes)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;

        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line)?;
            if n == 0 {
                return Err(ArmError::Protocol(format!(
                    "sandboxed program closed stdout before responding to {method}"
                )));
            }
            let envelope: Value = serde_json::from_str(&line)?;
            if envelope.get("id").is_none() {
                continue; // an unsolicited notification — keep reading
            }
            if envelope.get("id").and_then(Value::as_u64) != Some(id) {
                return Err(ArmError::Protocol(format!("response id mismatch for {method}: {line}")));
            }
            if let Some(error) = envelope.get("error") {
                return Err(ArmError::Protocol(format!("{method} returned an error: {error}")));
            }
            return envelope
                .get("result")
                .cloned()
                .ok_or_else(|| ArmError::Protocol(format!("{method} had no result: {line}")));
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), ArmError> {
        let bytes = jsonrpc::encode_notification(method, params);
        self.stdin.write_all(&bytes)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }
}

fn overlay_at(base_layer: &Path, scratch_root: &Path, label: &str) -> OverlaySpec {
    let root = scratch_root.join(label);
    OverlaySpec {
        lower: base_layer.to_path_buf(),
        upper: root.join("upper"),
        work: root.join("work"),
        mountpoint: root.join("merged"),
    }
}

/// Spawn `program.program` over `overlay`, run one MCP handshake, send `call_count`
/// `tools/call` requests (identical arguments every time), then wait for the process to
/// exit. One "session" — one spawned process, however many calls happen inside its
/// lifetime.
fn one_session(
    overlay: OverlaySpec,
    program: &ArmProgram,
    call_count: usize,
) -> Result<sandbox::SandboxOutcome, ArmError> {
    let spec = SandboxSpec {
        overlay,
        program: program.program.clone(),
        args: program.args.clone(),
        timeout: program.timeout,
        // P3-01's network isolation is opt-in and not yet wired into arm execution — every
        // arm here still needs `npx` able to resolve/verify its own package, which P3-01's
        // own doc comment found genuinely hangs under network isolation. A future arm
        // wanting P3-01's containment property needs a pre-resolved (not npx-wrapped)
        // program to run under it at all.
        network_isolated: false,
    };
    let (handle, stdin, stdout) = sandbox::spawn(&spec)?;
    let mut client = RawClient { stdin, reader: BufReader::new(stdout), next_id: 0 };

    client.call(
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "mcp-conformance-p2-07", "version": env!("CARGO_PKG_VERSION") },
        }),
    )?;
    client.notify("notifications/initialized", json!({}))?;

    for _ in 0..call_count {
        client.call(
            "tools/call",
            json!({ "name": program.tool_name, "arguments": program.arguments }),
        )?;
    }

    drop(client); // closes stdin/stdout so wait() below isn't blocked on an open pipe
    Ok(handle.wait()?)
}

fn harvest(outcome: sandbox::SandboxOutcome, blob_store: &store::BlobStore) -> Result<ArmRun, ArmError> {
    let observation = observe::harvest(
        &outcome.upper,
        outcome.exit_status,
        outcome.timed_out,
        outcome.orphans_impossible,
        blob_store,
    )
    .map_err(|e| ArmError::Io(std::io::Error::other(e)))?;
    let bytes = blob_store.get(&observation.upper_layer_digest)?;
    let evidence = observe::evtree::decode(&bytes).map_err(|e| ArmError::Decode(e.to_string()))?;
    Ok(ArmRun { upper: outcome.upper, evidence })
}

/// `Arm 1'`: a single call, in a freshly constructed sandbox, independent of every other
/// arm — the repeat half of architecture.md §4.2's noise-floor pair (`D1'`, alongside `Arm
/// 1`'s own `D1`, produced elsewhere).
pub fn run_arm_1_prime(
    program: &ArmProgram,
    scratch_root: &Path,
    blob_store: &store::BlobStore,
) -> Result<ArmRun, ArmError> {
    let overlay = overlay_at(&program.base_layer, scratch_root, "arm-1-prime");
    let outcome = one_session(overlay, program, 1)?;
    harvest(outcome, blob_store)
}

/// `Arm 2`: two calls in one freshly constructed sandbox, in the same process — `D2`.
pub fn run_arm_2(
    program: &ArmProgram,
    scratch_root: &Path,
    blob_store: &store::BlobStore,
) -> Result<ArmRun, ArmError> {
    let overlay = overlay_at(&program.base_layer, scratch_root, "arm-2");
    let outcome = one_session(overlay, program, 2)?;
    harvest(outcome, blob_store)
}

/// `Arm 2R`: one call, a genuine process restart (see this module's own doc comment), then
/// one more call — `D2R`, resolving the caching confound architecture.md §4.2 describes.
pub fn run_arm_2r(
    program: &ArmProgram,
    scratch_root: &Path,
    blob_store: &store::BlobStore,
) -> Result<ArmRun, ArmError> {
    let overlay = overlay_at(&program.base_layer, scratch_root, "arm-2r");
    let _first = one_session(overlay.clone(), program, 1)?;
    let second = one_session(overlay, program, 1)?;
    harvest(second, blob_store)
}

/// Test-only support shared across this crate's other test modules (`crate::noise`'s tests,
/// notably) — a separate module rather than nested inside `mod tests` below specifically so a
/// sibling module's own `#[cfg(test)]` code can reuse the same stub server and sandbox-slot
/// discipline instead of duplicating it. `pub`, not `pub(crate)`: this module is private
/// (`mod arms;`, not `pub mod arms;`), so the two are equally invisible outside the crate —
/// `pub` here is simply what that already-imposed boundary makes the simpler spelling.
#[cfg(test)]
pub mod tests_support {
    use super::{ArmProgram, ArmRun};
    use sandbox::{EntryKind, EntrySpec};
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::Duration;

    /// Rust's test harness runs `#[test]` functions concurrently by default, but this
    /// crate's own top-level doc comment states the constraint plainly: "must not run more
    /// than one sandbox per worker slot at a time... concurrent sandboxes share a kernel and
    /// a page cache, and the resulting timing coupling is exactly the noise P2-08's noise
    /// floor is trying to measure." Found directly, not assumed: running arm tests
    /// concurrently (the default) occasionally pushed one sandboxed session's wall-clock
    /// time past ten seconds under contention — the same test suite, serialized, completes
    /// in well under a second every time. Every test using this module takes this lock
    /// before spawning anything, so this crate's own tests obey the constraint the crate
    /// itself documents rather than accidentally violating it.
    static SANDBOX_SLOT: Mutex<()> = Mutex::new(());

    pub fn take_sandbox_slot() -> std::sync::MutexGuard<'static, ()> {
        SANDBOX_SLOT.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A minimal MCP-shaped stdio stub, purpose-built for this crate's own tests: it answers
    /// `initialize` and `tools/call` (matching whatever `id` the request actually used — the
    /// field order `discovery::jsonrpc::encode_request` always produces,
    /// `{"jsonrpc":...,"id":N,"method":...}`, makes a plain `sed` extraction reliable here),
    /// ignores `notifications/initialized`, and tracks an *in-process-only* call counter:
    /// only the first `tools/call` a given process instance ever receives appends a line to
    /// `effect.txt`. This is deliberately the exact "internal caching" shape architecture.md
    /// §4.2 is worried about — a tool whose second call in the same process has no
    /// additional effect, but whose effect reappears after a genuine restart — modelled
    /// directly rather than asserted about in the abstract.
    const STUB_SERVER_SCRIPT: &[u8] = b"#!/bin/sh
calls=0
while IFS= read -r line; do
  id=$(printf '%s' \"$line\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p')
  case \"$line\" in
    *'\"method\":\"initialize\"'*)
      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"serverInfo\":{\"name\":\"arm-stub\",\"version\":\"0.0.0\"}}}\\n' \"$id\"
      ;;
    *'\"method\":\"notifications/initialized\"'*)
      ;;
    *'\"method\":\"tools/call\"'*)
      calls=$((calls + 1))
      if [ \"$calls\" -eq 1 ]; then
        printf 'call\\n' >> effect.txt
      fi
      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\\n' \"$id\"
      ;;
  esac
done
";

    pub fn build_stub_base_layer(root: &Path) {
        let entries = vec![EntrySpec {
            path: PathBuf::from("stub_server.sh"),
            kind: EntryKind::File(STUB_SERVER_SCRIPT.to_vec()),
            mode: 0o755,
        }];
        sandbox::build(root, &entries).expect("build stub base layer");
    }

    pub fn stub_program(base_layer: &Path) -> ArmProgram {
        ArmProgram {
            base_layer: base_layer.to_path_buf(),
            program: PathBuf::from("/bin/sh"),
            args: vec!["stub_server.sh".to_string()],
            tool_name: "noop".to_string(),
            arguments: json!({}),
            timeout: Duration::from_secs(10),
        }
    }

    pub fn effect_lines(run: &ArmRun) -> usize {
        assert!(
            run.evidence.entries.iter().any(|e| e.path == b"effect.txt"),
            "the decoded changeset must list effect.txt as a real captured entry, not just a \
             file this test happens to read directly off disk"
        );
        let content = std::fs::read_to_string(run.upper.join("effect.txt")).unwrap_or_default();
        content.lines().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::tests_support::{build_stub_base_layer, effect_lines, stub_program, take_sandbox_slot};

    /// `Arm 1'`: two entirely separate invocations of this arm shape must each land in their
    /// own, non-overlapping upper directory and each independently show the single-call
    /// effect — "independent" in the exit criterion's literal sense: neither run's changeset
    /// can see or be affected by the other's.
    #[test]
    fn arm_1_prime_runs_are_physically_independent_and_each_shows_one_call() {
        let _slot = take_sandbox_slot();
        let lower = tempfile::tempdir().expect("lower tempdir");
        build_stub_base_layer(lower.path());
        let program = stub_program(lower.path());

        let scratch_a = tempfile::tempdir().expect("scratch a");
        let scratch_b = tempfile::tempdir().expect("scratch b");
        let store_dir = tempfile::tempdir().expect("store dir");
        let blob_store = store::BlobStore::open(store_dir.path()).expect("open blob store");

        let run_a = run_arm_1_prime(&program, scratch_a.path(), &blob_store).expect("arm 1' run a");
        let run_b = run_arm_1_prime(&program, scratch_b.path(), &blob_store).expect("arm 1' run b");

        assert_ne!(run_a.upper, run_b.upper, "two arm 1' runs must never share an upper directory");
        assert_eq!(effect_lines(&run_a), 1, "run a's own single call must have written once");
        assert_eq!(effect_lines(&run_b), 1, "run b's own single call must have written once");
    }

    /// `Arm 2`: two calls inside the *same* process only ever produce one write — the
    /// in-process-only caching this module's stub server deliberately models. This is `D2`
    /// exactly as architecture.md §4.2 needs it to be checkable against `D1`.
    #[test]
    fn arm_2_double_call_in_process_shows_only_the_first_calls_effect() {
        let _slot = take_sandbox_slot();
        let lower = tempfile::tempdir().expect("lower tempdir");
        build_stub_base_layer(lower.path());
        let program = stub_program(lower.path());

        let scratch = tempfile::tempdir().expect("scratch");
        let store_dir = tempfile::tempdir().expect("store dir");
        let blob_store = store::BlobStore::open(store_dir.path()).expect("open blob store");

        let run = run_arm_2(&program, scratch.path(), &blob_store).expect("arm 2 run");

        assert_eq!(
            effect_lines(&run),
            1,
            "two tools/call requests in one process must still show exactly one effect — the \
             second call's suppression by the stub's in-process counter is the whole point"
        );
    }

    /// `Arm 2R`: the restart between the two calls means the *second* process's own counter
    /// starts fresh, so its call writes again — the effect reappearing after a restart,
    /// exactly the signature architecture.md §4.2 says distinguishes caching from genuine
    /// idempotence. Same total call count as `Arm 2` above (two), different final state,
    /// because of the restart in between.
    #[test]
    fn arm_2r_restart_lets_the_second_calls_effect_reappear() {
        let _slot = take_sandbox_slot();
        let lower = tempfile::tempdir().expect("lower tempdir");
        build_stub_base_layer(lower.path());
        let program = stub_program(lower.path());

        let scratch = tempfile::tempdir().expect("scratch");
        let store_dir = tempfile::tempdir().expect("store dir");
        let blob_store = store::BlobStore::open(store_dir.path()).expect("open blob store");

        let run = run_arm_2r(&program, scratch.path(), &blob_store).expect("arm 2r run");

        assert_eq!(
            effect_lines(&run),
            2,
            "a genuine restart between the two calls must let both calls' effects land, unlike \
             Arm 2's single in-process effect"
        );
    }
}
