//! `initialize` + `tools/list`, with byte-exact response capture (P0-01) and structural
//! prevention of any tool call.
//!
//! **Must not:** call any tool. Structural, not disciplinary: [`crate::transport::Transport`]
//! is private to this crate, so nothing outside it can reach a method taking an arbitrary
//! MCP method string. [`DiscoveryClient::discover`] is the only public entry point, and the
//! three method names it sends — `initialize`, `notifications/initialized`, `tools/list` —
//! are literals in its body, not parameters. There is no public function anywhere in this
//! crate that accepts a method name.

use std::ffi::OsStr;

use serde_json::{Value, json};

use crate::transport::{ChildProcessTransport, HttpTransport, Transport};

/// The MCP protocol revision this crate negotiates.
///
/// Verify against <https://modelcontextprotocol.io> before bumping — design.md explicitly
/// calls this out as something to check, not assume, because it has changed across
/// revisions before. Current stable as of 2026-07-26: `2025-11-25`. A `2026-07-28` revision
/// is in release-candidate status and removes the `initialize` handshake entirely, which
/// would be a breaking change to this module's whole shape, not a version bump — O-01 owns
/// tracking that transition.
const CLIENT_PROTOCOL_VERSION: &str = "2025-11-25";

const CLIENT_NAME: &str = "mcp-conformance-harness";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything discovery captured about one server.
///
/// `initialize_raw` and `tools_list_raw` are the only fields P0-02's metadata pin may ever
/// hash — "the pin is over bytes, not semantics" (architecture.md §3.1). Nothing in this
/// crate parses them any further than routing the response envelope; that's the pinner's
/// and census's job.
#[derive(Debug, Clone)]
pub struct Discovery {
    /// The exact bytes of the `initialize` response, before any parsing.
    pub initialize_raw: Vec<u8>,
    /// The exact bytes of the `tools/list` response, before any parsing.
    pub tools_list_raw: Vec<u8>,
    /// The `protocolVersion` the server actually returned from `initialize` — for
    /// `TOOL_SNAPSHOT.spec_revision` (architecture.md §6).
    pub negotiated_spec_revision: String,
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
    /// Sends exactly three things, in order: `initialize`, the `notifications/initialized`
    /// notification required by the MCP lifecycle before any other request is valid, then
    /// `tools/list`. Nothing else. Never calls a tool.
    pub fn discover(&mut self) -> Result<Discovery, DiscoveryError> {
        let init_params = json!({
            "protocolVersion": CLIENT_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": CLIENT_NAME, "version": CLIENT_VERSION },
        });
        let init_response = self.transport.call("initialize", init_params)?;
        let negotiated_spec_revision = extract_negotiated_version(&init_response.bytes)?;
        self.transport.set_negotiated_protocol_version(&negotiated_spec_revision);

        self.transport.notify("notifications/initialized", json!({}))?;

        let tools_response = self.transport.call("tools/list", json!({}))?;

        Ok(Discovery {
            initialize_raw: init_response.bytes,
            tools_list_raw: tools_response.bytes,
            negotiated_spec_revision,
        })
    }
}

fn extract_negotiated_version(bytes: &[u8]) -> Result<String, DiscoveryError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| {
        DiscoveryError::Protocol(format!("initialize result is not valid JSON: {e}"))
    })?;
    value
        .get("result")
        .and_then(|r| r.get("protocolVersion"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| DiscoveryError::Protocol("initialize result missing protocolVersion".into()))
}
