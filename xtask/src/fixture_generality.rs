//! P3-06: measure, across a real corpus of tools, the ratio whose `tools/call` round trip
//! completes under P3-01's network isolation plus P3-03's generic mock backend versus the
//! ones whose round trip itself fails — answering open question 2 empirically
//! (architecture.md §4.4: "log how many tools need bespoke fixtures versus how many work
//! against a generic mock. That ratio is a publishable result in its own right.").
//!
//! # Corpus size, disclosed honestly — the same shortfall P2-10 already found and reported
//!
//! Same constraint as `ruleset_v2`: the one real, running MCP server available without
//! adding a broader vetted corpus of third-party servers is
//! `@modelcontextprotocol/server-everything`, exposing 13 tools. This run measures all 13 —
//! real sandboxed executions under the real veth bridge and generic mock, real argument
//! synthesis against each tool's real `inputSchema` — and [`RESULT_PATH`] reports the real
//! number honestly. Reaching a sample size actually informative about fixture generality in
//! general needs a broader corpus of vetted servers that make genuine external API calls;
//! this reference server is a *protocol* demonstration server (`echo`, `add`,
//! `longRunningOperation`, and so on), not one plausibly calling out to any real API at all.
//!
//! # A real, non-degenerate finding anyway — and a real measurement bug it exposed
//!
//! The naive expectation ("100% mock-sufficient because nothing attempted egress") was not
//! quite what the first real run showed: 2 of the 13 tools, `toggle-simulated-logging` and
//! `toggle-subscriber-updates`, initially measured as "needs bespoke fixture" because the
//! sandboxed *process* never exited (one timed out, one crashed with `EPIPE` writing a
//! notification to an already-closed pipe). Investigated rather than reported as-is:
//! reproduced the identical non-exiting behaviour by hand, completely outside the sandbox,
//! with no network isolation involved at all — both tools answer their own `tools/call`
//! immediately and successfully, then start a 5-second background timer sending
//! `notifications/message`/resource-update notifications indefinitely, by design. This
//! confirmed the earlier failures were an artifact of requiring the whole *process* to exit
//! cleanly, not evidence the generic mock was insufficient — see [`run_one_tool`]'s own doc
//! comment for the fix (classify on whether the `tools/call` round trip itself completed).
//! With that fixed, all 13 tools measure mock-sufficient — the originally expected, if
//! unglamorous, degenerate result, reached honestly rather than by an uninvestigated
//! coincidence.
//!
//! # Why this runs the resolved entry point directly, not `npx`, inside the sandbox
//!
//! P3-01's own doc comment found `npx` itself hangs under network isolation — its registry
//! freshness check retries with backoff rather than failing fast, even against an
//! already-cached package — and explicitly deferred solving it to "whoever wires strict mode
//! into the measurement pipeline next." This is that pipeline, so it is solved here:
//! [`resolve_entry_point`] runs once, on the host, with the same unrestricted network access
//! `list_tools` below already uses, to find the package's real on-disk entry script by
//! searching `npm`'s own cache directory (never hardcoding the internal hash path `npm`
//! itself assigns it) — and the sandboxed runs invoke that script directly via `node`, so only
//! the *tool's own* business-logic network behaviour is exposed to the network-isolated
//! sandbox, not `npx`'s own bootstrap resolution step.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sandbox::{OverlaySpec, SandboxSpec};
use serde_json::{json, Value};

const RESULT_PATH: &str = "results/conformance/p3_06_fixture_generality.json";
const PER_TOOL_TIMEOUT: Duration = Duration::from_secs(30);
const PACKAGE: &str = "@modelcontextprotocol/server-everything";

/// Why this run failed outright (a measurement failure for one *tool* is not this — that's
/// recorded as `skipped` in the result and the run continues, the same discipline
/// `ruleset_v2`'s own driver already established).
#[derive(Debug)]
pub enum FixtureGeneralityError {
    /// A filesystem operation failed.
    Io(std::io::Error),
    /// A JSON message wasn't the expected shape.
    Json(serde_json::Error),
    /// Constructing or launching the sandbox failed.
    Sandbox(sandbox::SpawnError),
    /// Setting up or tearing down the veth bridge failed.
    Bridge(sandbox::NetnsError),
    /// Starting the mock backend failed.
    MockBackend(world::mock_backend::MockBackendError),
}

impl std::fmt::Display for FixtureGeneralityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Json(e) => write!(f, "JSON error: {e}"),
            Self::Sandbox(e) => write!(f, "sandbox error: {e}"),
            Self::Bridge(e) => write!(f, "bridge error: {e}"),
            Self::MockBackend(e) => write!(f, "mock backend error: {e}"),
        }
    }
}

impl std::error::Error for FixtureGeneralityError {}

impl From<std::io::Error> for FixtureGeneralityError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<serde_json::Error> for FixtureGeneralityError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
impl From<sandbox::SpawnError> for FixtureGeneralityError {
    fn from(e: sandbox::SpawnError) -> Self {
        Self::Sandbox(e)
    }
}
impl From<sandbox::NetnsError> for FixtureGeneralityError {
    fn from(e: sandbox::NetnsError) -> Self {
        Self::Bridge(e)
    }
}
impl From<world::mock_backend::MockBackendError> for FixtureGeneralityError {
    fn from(e: world::mock_backend::MockBackendError) -> Self {
        Self::MockBackend(e)
    }
}

const SERVER_PROGRAM: &str = "npx";
fn server_args() -> Vec<String> {
    vec!["-y".into(), PACKAGE.into(), "stdio".into()]
}

/// List the reference server's tools directly over its own stdio — same reasoning and same
/// shape as `ruleset_v2::list_tools`: `discovery::DiscoveryClient` correctly rejects this
/// server's unsolicited `notifications/tools/list_changed` interleaving as a protocol
/// violation, so a small local client that skips them is used instead.
fn list_tools() -> Result<Vec<Value>, FixtureGeneralityError> {
    let mut child = std::process::Command::new(SERVER_PROGRAM)
        .args(server_args())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("piped stdout"));

    let send = |stdin: &mut std::process::ChildStdin, message: &Value| -> Result<(), FixtureGeneralityError> {
        stdin.write_all(serde_json::to_string(message)?.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(())
    };
    let call = |stdin: &mut std::process::ChildStdin,
                reader: &mut BufReader<std::process::ChildStdout>,
                id: u64,
                method: &str,
                params: Value|
     -> Result<Value, FixtureGeneralityError> {
        send(stdin, &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                return Err(FixtureGeneralityError::Io(std::io::Error::other(format!(
                    "server closed stdout before responding to {method}"
                ))));
            }
            let envelope: Value = serde_json::from_str(&line)?;
            if envelope.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            return Ok(envelope.get("result").cloned().unwrap_or(Value::Null));
        }
    };

    call(
        &mut stdin,
        &mut reader,
        0,
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "mcp-conformance-p3-06", "version": env!("CARGO_PKG_VERSION") },
        }),
    )?;
    send(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} }))?;

    let tools_result = call(&mut stdin, &mut reader, 1, "tools/list", json!({}))?;
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();

    Ok(tools_result.get("tools").and_then(Value::as_array).cloned().unwrap_or_default())
}

/// Resolve `@modelcontextprotocol/server-everything`'s real on-disk entry script — see this
/// module's own doc comment for why this exists instead of invoking `npx` inside the
/// sandbox. Assumes [`list_tools`] already ran `npx -y <PACKAGE> stdio` successfully, which
/// guarantees the package is cached somewhere under `npm`'s own `_npx` cache directory
/// (`npm config get cache`, never hardcoded — the exact hash `npm` assigns that directory is
/// an internal implementation detail this function deliberately does not try to predict, the
/// same reasoning that ruled out a `require.resolve`-based approach: confirmed directly that
/// `npx -p <pkg> node -e ...` does *not* put the temp-installed package on `NODE_PATH`, so
/// `require.resolve` from an arbitrary script's own working directory can't see it either).
fn resolve_entry_point() -> Result<PathBuf, FixtureGeneralityError> {
    let cache_output = std::process::Command::new("npm").args(["config", "get", "cache"]).output()?;
    if !cache_output.status.success() {
        return Err(FixtureGeneralityError::Io(std::io::Error::other(format!(
            "npm config get cache failed: {}",
            String::from_utf8_lossy(&cache_output.stderr)
        ))));
    }
    let npx_cache = PathBuf::from(String::from_utf8_lossy(&cache_output.stdout).trim().to_string()).join("_npx");
    let package_relative = Path::new("node_modules").join(PACKAGE).join("package.json");

    let package_json_path = std::fs::read_dir(&npx_cache)?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(&package_relative))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            FixtureGeneralityError::Io(std::io::Error::other(format!(
                "{PACKAGE} not found under any npx cache directory in {}; list_tools() should \
                 have cached it",
                npx_cache.display()
            )))
        })?;

    let package_json: Value = serde_json::from_slice(&std::fs::read(&package_json_path)?)?;
    let bin_relative = package_json
        .get("bin")
        .and_then(|b| b.get("mcp-server-everything"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            FixtureGeneralityError::Io(std::io::Error::other(
                "package.json has no bin.mcp-server-everything entry",
            ))
        })?;
    Ok(package_json_path.parent().expect("package.json has a parent directory").join(bin_relative))
}

/// A tiny, one-off newline-delimited JSON-RPC round trip over the sandbox's own pipes — same
/// shape as `first_verdict::RawClient`/`ruleset_v2`'s own local client, not shared across
/// `xtask` modules for the same reason those two don't share one either: each is a
/// deliberately disposable one-off, not a component worth a shared abstraction over.
struct RawClient {
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl RawClient {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, FixtureGeneralityError> {
        let id = self.next_id;
        self.next_id += 1;
        self.stdin.write_all(
            serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?
                .as_bytes(),
        )?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line)?;
            if n == 0 {
                return Err(FixtureGeneralityError::Io(std::io::Error::other(format!(
                    "server closed stdout before responding to {method}"
                ))));
            }
            let envelope: Value = serde_json::from_str(&line)?;
            if envelope.get("id").is_none() {
                continue; // unsolicited notification
            }
            if envelope.get("id").and_then(Value::as_u64) != Some(id) {
                return Err(FixtureGeneralityError::Io(std::io::Error::other(format!(
                    "response id mismatch for {method}: {line}"
                ))));
            }
            return Ok(envelope.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), FixtureGeneralityError> {
        self.stdin.write_all(
            serde_json::to_string(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))?
                .as_bytes(),
        )?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }
}

/// One tool's measurement outcome: did its `tools/call` round trip complete under the
/// network-isolated, generic-mocked sandbox, or did the sandboxed process hang (timeout) or
/// exit abnormally instead — the mechanical, non-heuristic signal this metric uses for
/// "worked against the generic mock" versus "plausibly needs a bespoke fixture." A tool
/// whose own `tools/call` response reports a business-level error is still counted as
/// mock-sufficient: it completed a coherent protocol round trip, which is the property this
/// metric measures — classifying *why* a tool's own logic failed would need semantic
/// interpretation of its response content, which is out of scope for a mechanical measurement
/// (the same separation `destructive`'s own doc comment draws between mechanical evidence and
/// model-based interpretation).
struct ToolMeasurement {
    name: String,
    mock_sufficient: bool,
    detail: String,
}

fn measure_one_tool(
    entry_point: &Path,
    base_layer: &Path,
    scratch_root: &Path,
    tool: &Value,
) -> Option<ToolMeasurement> {
    let name = tool.get("name").and_then(Value::as_str)?.to_string();
    let schema = tool.get("inputSchema").unwrap_or(&Value::Null);
    let synthesis = match argsynth::synthesize(schema, &argsynth::FixtureBindings::new()) {
        Ok(s) => s,
        Err(e) => {
            return Some(ToolMeasurement {
                name,
                mock_sufficient: false,
                detail: format!("argument synthesis failed: {e}"),
            })
        }
    };

    let result = run_one_tool(entry_point, base_layer, scratch_root, &name, synthesis.arguments);
    Some(match result {
        Ok((call_result, post_response_anomaly)) => ToolMeasurement {
            name,
            mock_sufficient: true,
            detail: match post_response_anomaly {
                None => format!("completed: {call_result}"),
                Some(anomaly) => format!(
                    "completed: {call_result} (note: {anomaly} — a known background-timer \
                     tool class, not a mock-insufficiency signal; see this module's own doc \
                     comment)"
                ),
            },
        },
        Err(e) => ToolMeasurement { name, mock_sufficient: false, detail: e.to_string() },
    })
}

/// Runs one tool's `tools/call` under the network-isolated, generic-mocked sandbox and
/// returns its result, plus an optional note about the sandboxed *process* not exiting
/// cleanly afterward.
///
/// # Why a non-exiting process does not, by itself, mean "needs a bespoke fixture"
///
/// Confirmed directly, entirely outside the sandbox and with no network isolation at all
/// (so nothing about interception or the mock could be the cause): two of this reference
/// server's tools, `toggle-simulated-logging` and `toggle-subscriber-updates`, respond to
/// their own `tools/call` immediately and successfully, then start a background timer that
/// keeps the Node process alive indefinitely, sending `notifications/message`/resource-update
/// notifications every 5 seconds — by design, not a bug. A harness that required the whole
/// sandboxed *process* to exit cleanly (this function's own first version did exactly that)
/// would misclassify both as "needs bespoke fixture," when the real signal — did the
/// `tools/call` round trip itself complete — already says the mock was entirely sufficient.
/// This function therefore bases [`ToolMeasurement::mock_sufficient`] purely on whether the
/// `tools/call` request received a response, and reports a non-exiting/timed-out process
/// afterward only as an informational note, never as the reason for the classification.
fn run_one_tool(
    entry_point: &Path,
    base_layer: &Path,
    scratch_root: &Path,
    tool_name: &str,
    arguments: Value,
) -> Result<(Value, Option<String>), FixtureGeneralityError> {
    let spec = SandboxSpec {
        overlay: OverlaySpec {
            lower: base_layer.to_path_buf(),
            upper: scratch_root.join("upper"),
            work: scratch_root.join("work"),
            mountpoint: scratch_root.join("merged"),
        },
        program: PathBuf::from("node"),
        args: vec![entry_point.to_string_lossy().into_owned(), "stdio".to_string()],
        timeout: PER_TOOL_TIMEOUT,
        network_isolated: true,
    };
    let (handle, stdin, stdout) = sandbox::spawn(&spec)?;
    let mut client = RawClient { stdin, reader: BufReader::new(stdout), next_id: 0 };

    let backend = world::mock_backend::GenericMockBackend::start()?;
    let bridge = sandbox::NetworkBridge::set_up(handle.init_pid(), backend.port())?;

    let session = (|| -> Result<Value, FixtureGeneralityError> {
        client.call(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "mcp-conformance-p3-06", "version": env!("CARGO_PKG_VERSION") },
            }),
        )?;
        client.notify("notifications/initialized", json!({}))?;
        client.call("tools/call", json!({ "name": tool_name, "arguments": arguments }))
    })();

    drop(client);
    // `handle.wait()` blocks up to `PER_TOOL_TIMEOUT` for a background-timer tool (see this
    // function's own doc comment) — an accepted cost for a one-off measurement script, not a
    // correctness issue: the round trip above already completed before this ever blocks.
    let outcome = handle.wait()?;
    backend.stop();
    bridge.teardown()?;

    let call_result = session?;
    let post_response_anomaly = if outcome.timed_out {
        Some("sandbox's hard timeout fired waiting for the process to exit".to_string())
    } else if !outcome.exit_status.is_some_and(|s| s.success()) {
        Some(format!("process exited abnormally after responding: {:?}", outcome.exit_status))
    } else {
        None
    };
    Ok((call_result, post_response_anomaly))
}

/// Run the full P3-06 measurement end to end and write [`RESULT_PATH`].
pub fn run() -> Result<(), FixtureGeneralityError> {
    let tools = list_tools()?;
    println!("discovered {} tools from {SERVER_PROGRAM} {:?}", tools.len(), server_args());

    let entry_point = resolve_entry_point()?;
    println!("resolved {PACKAGE}'s entry point: {}", entry_point.display());

    let scratch = tempfile::tempdir()?;
    let base_layer = scratch.path().join("lower");
    sandbox::build(&base_layer, &[]).map_err(|e| FixtureGeneralityError::Io(std::io::Error::other(e)))?;

    let mut measurements = Vec::new();
    for tool in &tools {
        let tool_scratch = scratch.path().join(format!(
            "tool-{}",
            tool.get("name").and_then(Value::as_str).unwrap_or("unnamed")
        ));
        if let Some(measurement) = measure_one_tool(&entry_point, &base_layer, &tool_scratch, tool) {
            println!(
                "`{}`: {}",
                measurement.name,
                if measurement.mock_sufficient { "mock-sufficient" } else { "needs bespoke fixture" }
            );
            measurements.push(measurement);
        }
    }

    write_result(&tools, &measurements)?;
    Ok(())
}

fn write_result(tools: &[Value], measurements: &[ToolMeasurement]) -> Result<(), FixtureGeneralityError> {
    let mock_sufficient_count = measurements.iter().filter(|m| m.mock_sufficient).count();
    let total = measurements.len();
    #[allow(clippy::cast_precision_loss)]
    let ratio = if total == 0 { None } else { Some(mock_sufficient_count as f64 / total as f64) };

    let record = json!({
        "task": "P3-06",
        "corpus": {
            "server": PACKAGE,
            "tools_discovered": tools.len(),
            "tools_measured": total,
            "note": "corpus size limited to this one real reference server's own tools (13); \
                     it is a protocol demonstration server (echo, add, longRunningOperation, \
                     and so on), not one plausibly making real external API calls at all, so \
                     a result near 100% mock-sufficient here is expected and reflects this \
                     corpus's shape (nothing attempted egress), not evidence the generic mock \
                     generalises to tools that genuinely need a backend. Answering that needs \
                     a broader corpus of vetted servers that actually call external APIs.",
        },
        "mock_sufficient_count": mock_sufficient_count,
        "needs_bespoke_fixture_count": total - mock_sufficient_count,
        "fixture_generality_ratio": ratio,
        "per_tool": measurements.iter().map(|m| json!({
            "tool": m.name,
            "mock_sufficient": m.mock_sufficient,
            "detail": m.detail,
        })).collect::<Vec<_>>(),
    });
    if let Some(parent) = Path::new(RESULT_PATH).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(RESULT_PATH, serde_json::to_string_pretty(&record)?)?;
    println!("wrote {RESULT_PATH}");
    Ok(())
}
