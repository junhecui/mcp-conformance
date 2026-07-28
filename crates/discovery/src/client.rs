//! `initialize` + `tools/list`, with byte-exact response capture (P0-01) and structural
//! prevention of any tool call.
//!
//! **Must not:** call any tool. Structural, not disciplinary: [`crate::transport::Transport`]
//! is private to this crate, so nothing outside it can reach a method taking an arbitrary
//! MCP method string. [`DiscoveryClient::discover`] is the only public entry point, and the
//! four method names it can send — `initialize`, `notifications/initialized`, `tools/list`,
//! and `server/discover` (P0-09's fallback, see below) — are literals in its body, not
//! parameters. There is no public function anywhere in this crate that accepts a method name.
//!
//! **P0-09: the `server/discover` fallback.** MCP spec revision `2026-07-28` removes the
//! `initialize`/`notifications/initialized` handshake entirely, replacing it with
//! `_meta["io.modelcontextprotocol/protocolVersion"]` on every request plus an optional
//! `server/discover` method. A server that has moved to that revision no longer answers
//! `initialize` at all — no loud failure, just whatever the server does for a method it
//! doesn't implement (typically a JSON-RPC `-32601 Method not found`, but for a spawned
//! stdio child, a pipe that never responds is equally possible).
//! [`DiscoveryClient::discover`] treats `initialize` getting a `-32601` error, or (over
//! stdio only) no response at all, as "this server may speak the new lifecycle" and retries
//! once with `server/discover` before giving up. A transport-level failure (connection
//! refused, DNS, TLS, timeout) is deliberately *not* treated as ambiguous — it means the
//! transport never reached the server, so a retry cannot produce a different outcome, only
//! a second full timeout against the same unreachable target. See [`DiscoveryPath`] for how
//! the chosen path is recorded on the result.

use std::ffi::OsStr;

use serde_json::{Value, json};

use crate::transport::{ChildProcessTransport, HttpTransport, Transport};

/// The MCP protocol revision this crate negotiates.
///
/// Verify against <https://modelcontextprotocol.io> before bumping — design.md explicitly
/// calls this out as something to check, not assume, because it has changed across
/// revisions before. Current stable as of 2026-07-26: `2025-11-25`. Revision `2026-07-28`
/// removes the `initialize` handshake entirely; this client still *requests* the
/// `2025-11-25` revision it was built against (both on `initialize` and, per P0-09, on the
/// `server/discover` fallback), and records whatever the server actually negotiates back in
/// [`Discovery::negotiated_spec_revision`] regardless of which path produced it.
const CLIENT_PROTOCOL_VERSION: &str = "2025-11-25";

const CLIENT_NAME: &str = "mcp-conformance-harness";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything discovery captured about one server.
///
/// `initialize_raw` and `tools_list_raw` are the only fields P0-02's metadata pin may ever
/// hash — "the pin is over bytes, not semantics" (architecture.md §3.1). Nothing in this
/// crate parses them any further than routing the response envelope; that's the pinner's
/// and census's job. Pinning is unaffected by [`discovery_path`](Self::discovery_path): the
/// pin (P0-02) hashes only `tools_list_raw`, never `initialize_raw` and never any discovery-
/// mechanics field, so which handshake produced a result has no bearing on its pin.
#[derive(Debug, Clone)]
pub struct Discovery {
    /// The exact bytes of the handshake response, before any parsing — the `initialize`
    /// response on the ordinary path, or the `server/discover` response when
    /// [`discovery_path`](Self::discovery_path) is [`DiscoveryPath::ServerDiscover`]. The
    /// field name predates P0-09's fallback and is kept for API stability; consult
    /// `discovery_path` to know which handshake actually produced these bytes.
    pub initialize_raw: Vec<u8>,
    /// The exact bytes of the `tools/list` response, before any parsing.
    pub tools_list_raw: Vec<u8>,
    /// The `protocolVersion` the server actually negotiated — read from whichever handshake
    /// response produced this result (`initialize` or `server/discover`), for
    /// `TOOL_SNAPSHOT.spec_revision` (architecture.md §6).
    pub negotiated_spec_revision: String,
    /// Which handshake actually produced this result — provenance, so a spec-revision
    /// transition shows up as a recorded fact on every affected result rather than as a
    /// silent behavioural difference. See [`DiscoveryPath`].
    pub discovery_path: DiscoveryPath,
}

/// Which handshake [`DiscoveryClient::discover`] used to reach a [`Discovery`] result.
///
/// Mirrors the Stage 2 census's `execution_provenance` pattern (bare-host vs.
/// containerized, `docs/tasks.md` P0-06): record *how* a result was produced on the result
/// itself, so census/audit data is never silently pooled across a discovery-mechanics
/// change the way ADR-002 already forbids pooling across oracles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryPath {
    /// The `initialize` + `notifications/initialized` lifecycle (spec `2025-11-25` and
    /// earlier).
    Initialize,
    /// The `server/discover` fallback (P0-09), attempted only after `initialize` got no
    /// response or a `-32601 Method not found` error — the shape a server that has moved to
    /// spec `2026-07-28`'s handshake-free lifecycle is expected to produce.
    ServerDiscover,
}

impl DiscoveryPath {
    /// Stable, lowercase-snake-case string form, for provenance fields in published results
    /// (JSON, DB columns) rather than `Debug` formatting.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::ServerDiscover => "server_discover",
        }
    }
}

impl std::fmt::Display for DiscoveryPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why discovery failed.
#[derive(Debug)]
pub enum DiscoveryError {
    /// A transport-level failure: connection refused, DNS, TLS, timeout.
    Transport(String),
    /// An underlying I/O error (process spawn, stdio pipe read/write).
    Io(std::io::Error),
    /// The response was not well-formed JSON-RPC, or its shape was unexpected. Includes a
    /// mismatched response id — treated as a protocol violation, not tolerated, since
    /// discovery runs against a server the trust model assumes may be actively hostile.
    Protocol(String),
    /// The server returned a JSON-RPC error object.
    ServerError {
        /// The JSON-RPC error code.
        code: i64,
        /// The JSON-RPC error message.
        message: String,
    },
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(msg) => write!(f, "transport error: {msg}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Self::ServerError { code, message } => {
                write!(f, "server returned error {code}: {message}")
            }
        }
    }
}

impl std::error::Error for DiscoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Transport(_) | Self::Protocol(_) | Self::ServerError { .. } => None,
        }
    }
}

/// Speaks `initialize` + `tools/list` over one transport.
///
/// Construct via [`DiscoveryClient::stdio`] or [`DiscoveryClient::http`]; the only other
/// public method is [`discover`](Self::discover). There is deliberately no way to send an
/// arbitrary MCP request through this type.
pub struct DiscoveryClient {
    transport: Box<dyn Transport>,
}

impl DiscoveryClient {
    /// Spawn `program` and speak MCP over its stdin/stdout.
    pub fn stdio(program: impl AsRef<OsStr>, args: &[&str]) -> Result<Self, DiscoveryError> {
        let transport = ChildProcessTransport::spawn(program, args)?;
        Ok(Self { transport: Box::new(transport) })
    }

    /// Same as [`Self::stdio`], with a hard wall-clock deadline on the child process's
    /// lifetime — see [`crate::transport::ChildProcessTransport::spawn_with_timeout`]. For a
    /// stdio target this harness does not control the code of (e.g. a containerized Class A
    /// package under Stage 2 census), unbounded blocking on `recv_line` is not acceptable at
    /// sweep scale.
    pub fn stdio_with_timeout(
        program: impl AsRef<OsStr>,
        args: &[&str],
        timeout: std::time::Duration,
    ) -> Result<Self, DiscoveryError> {
        let transport = ChildProcessTransport::spawn_with_timeout(program, args, Some(timeout))?;
        Ok(Self { transport: Box::new(transport) })
    }

    /// Speak MCP Streamable HTTP against `endpoint` (the single-JSON-response case; see
    /// [`crate::transport::HttpTransport`] for the SSE-streaming exclusion). 30s timeout.
    #[must_use]
    pub fn http(endpoint: impl Into<String>) -> Self {
        Self { transport: Box::new(HttpTransport::new(endpoint.into())) }
    }

    /// Same as [`Self::http`], with a caller-chosen timeout instead of the 30s default —
    /// for a large sequential sweep (a census over hundreds of servers) where a handful of
    /// unresponsive hosts at the default timeout would dominate total run time.
    #[must_use]
    pub fn http_with_timeout(endpoint: impl Into<String>, timeout: std::time::Duration) -> Self {
        Self { transport: Box::new(HttpTransport::with_timeout(endpoint.into(), timeout)) }
    }

    /// Run the full discovery sequence and capture its evidence.
    ///
    /// Ordinarily sends exactly three things, in order: `initialize`, the
    /// `notifications/initialized` notification required by the `2025-11-25`-and-earlier MCP
    /// lifecycle before any other request is valid, then `tools/list`. Never calls a tool.
    ///
    /// P0-09 fallback: if `initialize` gets no response over stdio (an I/O failure — a
    /// closed pipe or a watchdog-killed hang) or a `-32601 Method not found` error, retries
    /// once with `server/discover` — the spec `2026-07-28` replacement for the handshake —
    /// before `tools/list`, skipping `notifications/initialized` (there is no `initialize`
    /// result to acknowledge on that path). Any other `initialize` failure (a malformed
    /// response, a JSON-RPC error that isn't "unrecognized method", or a transport-level
    /// failure such as connection refused, DNS, TLS, or a timeout) is treated as a real
    /// discovery failure, not a signal to fall back — see [`is_initialize_unavailable`].
    pub fn discover(&mut self) -> Result<Discovery, DiscoveryError> {
        let handshake_params = json!({
            "protocolVersion": CLIENT_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": CLIENT_NAME, "version": CLIENT_VERSION },
        });

        let (handshake_raw, negotiated_spec_revision, discovery_path) =
            match self.transport.call("initialize", handshake_params.clone()) {
                Ok(init_response) => {
                    let negotiated_spec_revision =
                        extract_negotiated_version(&init_response.bytes)?;
                    self.transport.set_negotiated_protocol_version(&negotiated_spec_revision);
                    self.transport.notify("notifications/initialized", json!({}))?;
                    (init_response.bytes, negotiated_spec_revision, DiscoveryPath::Initialize)
                }
                Err(err) if is_initialize_unavailable(&err) => {
                    let discover_response =
                        self.transport.call("server/discover", handshake_params)?;
                    let negotiated_spec_revision =
                        extract_negotiated_version(&discover_response.bytes)?;
                    self.transport.set_negotiated_protocol_version(&negotiated_spec_revision);
                    (
                        discover_response.bytes,
                        negotiated_spec_revision,
                        DiscoveryPath::ServerDiscover,
                    )
                }
                Err(err) => return Err(err),
            };

        let tools_response = self.transport.call("tools/list", json!({}))?;

        Ok(Discovery {
            initialize_raw: handshake_raw,
            tools_list_raw: tools_response.bytes,
            negotiated_spec_revision,
            discovery_path,
        })
    }
}

/// Decide whether an `initialize` failure looks like "this server doesn't implement
/// `initialize` at all" (worth retrying with `server/discover`, per P0-09) rather than a
/// real discovery failure (worth surfacing as-is).
///
/// Deliberately narrow: a malformed response ([`DiscoveryError::Protocol`]) or a JSON-RPC
/// error the server raised for some other reason (any [`DiscoveryError::ServerError`] code
/// besides the standard `-32601 Method not found`) means the server *did* engage with
/// `initialize` and rejected it on its own terms — that is a real finding about the server,
/// not evidence it has moved to a handshake-free spec revision, and papering over it with a
/// silent retry would hide it.
///
/// [`DiscoveryError::Transport`] is deliberately *excluded*, unlike [`DiscoveryError::Io`].
/// For `HttpTransport`, every connection-level failure — DNS failure, connection refused,
/// TLS error, and critically, hitting the configured request timeout — maps to `Transport`
/// (see `transport.rs`). A connection-level failure means the transport never reached the
/// server at all, which says nothing about which JSON-RPC method the server implements,
/// unlike a `-32601` (which requires the server to have actually engaged with the request
/// and rejected it). Falling back on it would buy a second full-timeout round trip to the
/// same unreachable target with no chance of a different outcome — a silent 2x cost on
/// exactly the failure population (unresponsive hosts) that dominates sweep-scale census
/// runs, which tune their timeouts specifically to bound that cost (see
/// `xtask/src/census_stage1.rs`).
fn is_initialize_unavailable(err: &DiscoveryError) -> bool {
    match err {
        // No response at all over stdio: closed pipe, or a watchdog-killed hang on a
        // spawned child process. Indistinguishable at this layer from "the server doesn't
        // speak this method and doesn't bother responding," so worth one retry via the
        // fallback path.
        DiscoveryError::Io(_) => true,
        // JSON-RPC's standard code for "the server does not recognize this method name."
        DiscoveryError::ServerError { code, .. } => *code == -32601,
        DiscoveryError::Protocol(_) | DiscoveryError::Transport(_) => false,
    }
}

fn extract_negotiated_version(bytes: &[u8]) -> Result<String, DiscoveryError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| {
        DiscoveryError::Protocol(format!("handshake result is not valid JSON: {e}"))
    })?;
    value
        .get("result")
        .and_then(|r| r.get("protocolVersion"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            DiscoveryError::Protocol("handshake result missing protocolVersion".into())
        })
}
