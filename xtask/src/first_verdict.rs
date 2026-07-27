//! P1-08: end-to-end demonstration — one real `readOnlyHint` verdict, on one real tool,
//! from one real MCP server, driven through every Phase 1 component that actually exists
//! (`sandbox` → `observe` → `integrity` → `normalise` → `verdict`), not a synthetic assembly
//! of already-unit-tested pieces. This is the Phase 1 exit criterion (architecture.md §10).
//!
//! **Target:** the official MCP reference "everything" server
//! (`@modelcontextprotocol/server-everything`, published by the `modelcontextprotocol` org
//! on npm), invoked via `npx` — the same resolution path Stage 2 census already uses for any
//! npm-resolved Class A package. Its `echo` tool declares `readOnlyHint: true` and genuinely
//! is read-only (it only formats and returns its input as text) — confirmed by hand,
//! inspecting its real `initialize`/`tools/list`/`tools/call` responses directly before
//! wiring this run, not assumed from the package name (the same discipline P0-06's hand
//! -verification and B-01's live findings already followed).
//!
//! **Not reused here, and why:** `discovery::DiscoveryClient` cannot drive this run — by
//! design (P0-01), it has no way to send a `tools/call`, and this script's entire point is
//! to invoke one. Rather than weaken that crate's structural guarantee for a one-off demo,
//! this module speaks the newline-delimited JSON-RPC directly over the pipes
//! `sandbox::spawn` returns, reusing `discovery::jsonrpc::encode_request`/
//! `encode_notification` (already `pub`, exactly for this kind of reuse — `probe`'s
//! `ProbeClient` set the precedent).
//!
//! **Containment note, stated plainly:** `sandbox::supervisor` mounts an overlay at one
//! directory and does not `pivot_root`/`chroot` (Phase 1's documented scope — mount
//! namespace + overlay + timeout only, per architecture.md §10). `npx`/`node` themselves run
//! against the real host filesystem, exactly as `class_a_stage2`'s Docker-based Stage 2
//! census already accepts for its own, differently-shaped containment. Only writes the
//! server makes *relative to its sandboxed working directory* are contained and captured;
//! `echo` makes none at all, which is exactly what this run is designed to demonstrate.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::Duration;

use discovery::jsonrpc;
use sandbox::{OverlaySpec, SandboxSpec};
use serde_json::{json, Value};

const TARGET_TOOL: &str = "echo";
const RUN_TIMEOUT: Duration = Duration::from_secs(60);
const RULESET_PATH: &str = "rulesets/v1.json";
const RESULT_PATH: &str = "results/conformance/p1_08_first_verdict.json";

/// Why this run failed.
#[derive(Debug)]
pub enum FirstVerdictError {
    /// A filesystem operation failed.
    Io(std::io::Error),
    /// Constructing or launching the sandbox failed.
    Sandbox(sandbox::SpawnError),
    /// A JSON message wasn't the expected shape.
    Json(serde_json::Error),
    /// The server's JSON-RPC responses didn't match this script's expectations.
    Protocol(String),
    /// Loading `rulesets/v1.json` failed.
    Ruleset(orchestrator::LoadRulesetError),
    /// Storing or reading back evidence failed.
    Store(store::StoreError),
    /// Decoding the stored `evtree1` capture failed.
    Decode(String),
}

impl std::fmt::Display for FirstVerdictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Sandbox(e) => write!(f, "sandbox error: {e}"),
            Self::Json(e) => write!(f, "JSON error: {e}"),
            Self::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Self::Ruleset(e) => write!(f, "ruleset load error: {e}"),
            Self::Store(e) => write!(f, "evidence store error: {e}"),
            Self::Decode(msg) => write!(f, "evidence decode error: {msg}"),
        }
    }
}

impl std::error::Error for FirstVerdictError {}

impl From<std::io::Error> for FirstVerdictError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<sandbox::SpawnError> for FirstVerdictError {
    fn from(e: sandbox::SpawnError) -> Self {
        Self::Sandbox(e)
    }
}
impl From<serde_json::Error> for FirstVerdictError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
impl From<orchestrator::LoadRulesetError> for FirstVerdictError {
    fn from(e: orchestrator::LoadRulesetError) -> Self {
        Self::Ruleset(e)
    }
}
impl From<store::StoreError> for FirstVerdictError {
    fn from(e: store::StoreError) -> Self {
        Self::Store(e)
    }
}

/// A tiny, one-off newline-delimited JSON-RPC round trip over the sandbox's own pipes —
/// deliberately not a reusable client (see this module's own doc comment for why
/// `discovery::DiscoveryClient` isn't extended for this instead). Owns the pipes outright;
/// dropping it closes both, which is how this script signals EOF to the sandboxed server
/// when the conversation is over.
struct RawClient {
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl RawClient {
    /// Send a request and return its result, skipping over any unsolicited server-to-client
    /// notifications interleaved before the matching response — a real thing this server
    /// does (`notifications/tools/list_changed` arrived before this script's own `tools/list`
    /// reply during manual testing), not a shape this script's protocol handling can assume
    /// away. A message the JSON-RPC spec allows a server to send unprompted (no `id` field
    /// at all) is skipped unconditionally; a response whose `id` doesn't match this call's is
    /// still treated as a real protocol error, not silently skipped too.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, FirstVerdictError> {
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
                return Err(FirstVerdictError::Protocol(format!(
                    "server closed stdout before responding to {method}"
                )));
            }
            let envelope: Value = serde_json::from_str(&line)?;
            if envelope.get("id").is_none() {
                // An unsolicited notification (e.g. `notifications/tools/list_changed`) —
                // not a response to anything this script sent; keep reading.
                continue;
            }
            if envelope.get("id").and_then(Value::as_u64) != Some(id) {
                return Err(FirstVerdictError::Protocol(format!(
                    "response id mismatch for {method}: {line}"
                )));
            }
            if let Some(error) = envelope.get("error") {
                return Err(FirstVerdictError::Protocol(format!(
                    "{method} returned an error: {error}"
                )));
            }
            return envelope.get("result").cloned().ok_or_else(|| {
                FirstVerdictError::Protocol(format!("{method} had no result: {line}"))
            });
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), FirstVerdictError> {
        let bytes = jsonrpc::encode_notification(method, params);
        self.stdin.write_all(&bytes)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }
}

/// Run the full P1-08 demonstration end to end and write [`RESULT_PATH`].
pub fn run() -> Result<(), FirstVerdictError> {
    let scratch = tempfile::tempdir()?;
    let lower = scratch.path().join("lower");
    sandbox::build(&lower, &[]).map_err(|e| FirstVerdictError::Io(std::io::Error::other(e)))?;

    let spec = SandboxSpec {
        overlay: OverlaySpec {
            lower,
            upper: scratch.path().join("upper"),
            work: scratch.path().join("work"),
            mountpoint: scratch.path().join("merged"),
        },
        program: "npx".into(),
        args: vec![
            "-y".to_string(),
            "@modelcontextprotocol/server-everything".to_string(),
            "stdio".to_string(),
        ],
        timeout: RUN_TIMEOUT,
    };

    println!("spawning sandboxed server: npx -y @modelcontextprotocol/server-everything stdio");
    let (handle, stdin, stdout) = sandbox::spawn(&spec)?;
    let mut client = RawClient { stdin, reader: BufReader::new(stdout), next_id: 0 };

    let init_result = client.call(
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "mcp-conformance-p1-08", "version": env!("CARGO_PKG_VERSION") },
        }),
    )?;
    let negotiated_version = init_result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| FirstVerdictError::Protocol("initialize missing protocolVersion".into()))?
        .to_string();
    println!("negotiated spec revision: {negotiated_version}");

    client.notify("notifications/initialized", json!({}))?;

    let tools_result = client.call("tools/list", json!({}))?;
    let tools = tools_result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| FirstVerdictError::Protocol("tools/list missing tools array".into()))?;
    let target = tools
        .iter()
        .find(|t| t.get("name").and_then(Value::as_str) == Some(TARGET_TOOL))
        .ok_or_else(|| FirstVerdictError::Protocol(format!("tool `{TARGET_TOOL}` not found")))?;
    let declared_read_only = target
        .get("annotations")
        .and_then(|a| a.get("readOnlyHint"))
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            FirstVerdictError::Protocol(format!("`{TARGET_TOOL}` does not declare readOnlyHint"))
        })?;
    println!("`{TARGET_TOOL}` declares readOnlyHint: {declared_read_only}");

    let call_result = client.call(
        "tools/call",
        json!({ "name": TARGET_TOOL, "arguments": { "message": "hello from mcp-conformance P1-08" } }),
    )?;
    println!("tool call result: {call_result}");

    drop(client); // closes stdin/stdout so wait() below isn't blocked on an open pipe
    let outcome = handle.wait()?;
    println!("sandbox outcome: exit_status={:?} timed_out={}", outcome.exit_status, outcome.timed_out);

    let gate_signals = integrity::RunSignals {
        timed_out: outcome.timed_out,
        containment_uncertain: !outcome.orphans_impossible,
        // This demo run predates P2-04's run planner, the first real caller that constructs
        // every run inside a `sandbox::cgroup::Cgroup` — no cap is enforced here yet, so
        // there is nothing a real cap-hit signal could be derived from. A disclosed gap, not
        // a hidden one; see `integrity::RunSignals`'s own doc comment.
        resource_cap_hit: false,
        // No seccomp instrumentation exists yet (P4-01) to ever set this `true`.
        escape_class_syscall_denied: false,
    };
    let gate_outcome = integrity::decide(gate_signals);
    println!("integrity gate: {gate_outcome:?}");
    let adversarial_flag =
        matches!(&gate_outcome, integrity::GateOutcome::Accept { adversarial_flag: true });

    let assessment = match gate_outcome {
        integrity::GateOutcome::Unverifiable(reason) => {
            verdict::Assessment::unverifiable(datamodel::Oracle::KernelChangeset, reason)
        }
        integrity::GateOutcome::Accept { adversarial_flag } => {
            if adversarial_flag {
                println!("adversarial_flag: true (escape-class syscall denied during this run)");
            }
            let store_dir = scratch.path().join("evidence-store");
            let blob_store = store::BlobStore::open(&store_dir)?;
            let observation = observe::harvest(
                &outcome.upper,
                outcome.exit_status,
                outcome.timed_out,
                outcome.orphans_impossible,
                &blob_store,
            )
            .map_err(|e| FirstVerdictError::Io(std::io::Error::other(e)))?;
            let capture_bytes = blob_store.get(&observation.upper_layer_digest)?;
            let raw_evidence = observe::evtree::decode(&capture_bytes)
                .map_err(|e| FirstVerdictError::Decode(e.to_string()))?;
            println!(
                "upper layer digest: {} ({} entries)",
                observation.upper_layer_digest,
                raw_evidence.entries.len()
            );

            let ruleset = orchestrator::load_ruleset(Path::new(RULESET_PATH))?;
            let changeset = normalise::normalise(&raw_evidence, &ruleset);
            println!(
                "canonical changeset: {} entries, user_state_is_empty={}",
                changeset.entries.len(),
                changeset.user_state_is_empty()
            );

            verdict::read_only_hint(declared_read_only, &changeset)
        }
    };

    println!(
        "VERDICT: outcome={:?} reason={:?} oracle={}",
        assessment.outcome(),
        assessment.reason(),
        assessment.oracle()
    );

    write_result(
        &negotiated_version,
        declared_read_only,
        &call_result,
        &assessment,
        adversarial_flag,
    )?;

    Ok(())
}

fn write_result(
    negotiated_version: &str,
    declared_read_only: bool,
    call_result: &Value,
    assessment: &verdict::Assessment,
    adversarial_flag: bool,
) -> Result<(), FirstVerdictError> {
    let record = json!({
        "task": "P1-08",
        "server": "@modelcontextprotocol/server-everything",
        "resolution": "npx -y @modelcontextprotocol/server-everything stdio",
        "negotiated_spec_revision": negotiated_version,
        "tool": TARGET_TOOL,
        "declared_read_only_hint": declared_read_only,
        "tool_call_result": call_result,
        "verdict": {
            "outcome": assessment.outcome().as_db_str(),
            "reason": assessment.reason().map(|r| r.as_db_str()),
            "oracle": assessment.oracle().as_db_str(),
        },
        // P2-03: the escape-class-denial flag from the integrity gate's `G4` branch follows
        // the record all the way into publication, per architecture.md §5.1 — never dropped
        // once the gate accepts the run, since an attempted escape is itself a finding.
        "adversarial_flag": adversarial_flag,
    });
    if let Some(parent) = Path::new(RESULT_PATH).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(RESULT_PATH, serde_json::to_string_pretty(&record)?)?;
    println!("wrote {RESULT_PATH}");
    Ok(())
}
