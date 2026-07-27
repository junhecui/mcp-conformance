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

/// Used only when a successful `server/discover` response doesn't itself carry an explicit
/// `protocolVersion` field. Per the RC announcement (O-01), protocol version may travel
/// entirely via `_meta["io.modelcontextprotocol/protocolVersion"]` on ordinary requests
/// under the new scheme rather than in the handshake result — but `server/discover` did not
/// exist before this revision, so a server answering it at all is itself evidence of which
/// revision negotiated. Not a guess invented here: it is the one fact this module can
/// actually observe (the method existing) standing in for a field the spec may not put in
/// this particular response.
const SERVER_DISCOVER_PROTOCOL_VERSION_FALLBACK: &str = "2026-07-28";

/// Which handshake actually produced a successful discovery (P0-09).
///
/// A second, orthogonal provenance axis alongside Stage 2 census's
/// bare-host-vs-containerized `execution_provenance` flag — this one records *how* the
/// server was discovered, not *where* it ran. Needed because spec revision `2026-07-28`
/// removes the `initialize`/`notifications/initialized` handshake entirely (see O-01),
/// replacing it with an optional `server/discover` request; a corpus discovered under a mix
/// of both paths must never silently pool them as if they were the same measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakePath {
    /// The `initialize` + `notifications/initialized` handshake — every spec revision up to
    /// and including `2025-11-25`.
    Initialize,
    /// The `server/discover` fallback — spec revision `2026-07-28` and later, once a server
    /// stops recognizing `initialize` at all.
    ServerDiscover,
}

impl std::fmt::Display for HandshakePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Initialize => write!(f, "initialize"),
            Self::ServerDiscover => write!(f, "server_discover"),
        }
    }
}

/// Everything discovery captured about one server.
///
/// `handshake_raw` and `tools_list_raw` are the only fields P0-02's metadata pin may ever
/// hash — "the pin is over bytes, not semantics" (architecture.md §3.1). Nothing in this
/// crate parses them any further than routing the response envelope; that's the pinner's
/// and census's job.
#[derive(Debug, Clone)]
pub struct Discovery {
    /// The exact bytes of whichever handshake response succeeded — `initialize`'s result
    /// when `handshake_path` is [`HandshakePath::Initialize`], `server/discover`'s result
    /// when it's [`HandshakePath::ServerDiscover`] — before any parsing.
    pub handshake_raw: Vec<u8>,
    /// The exact bytes of the `tools/list` response, before any parsing.
    pub tools_list_raw: Vec<u8>,
    /// The `protocolVersion` the server actually negotiated — for
    /// `TOOL_SNAPSHOT.spec_revision` (architecture.md §6). Populated from the handshake
    /// response when it carries one explicitly; see [`SERVER_DISCOVER_PROTOCOL_VERSION_FALLBACK`]
    /// for the one case it isn't.
    pub negotiated_spec_revision: String,
    /// Which handshake actually succeeded — provenance, per P0-09's exit criterion.
    pub handshake_path: HandshakePath,
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
    /// Tries the `initialize`/`notifications/initialized`/`tools/list` sequence first (every
    /// spec revision through `2025-11-25`). If `initialize` comes back as an unrecognized
    /// method — the one unambiguous signal that this server has adopted revision
    /// `2026-07-28`, which removes `initialize` outright (P0-09; see O-01) — falls back to
    /// `server/discover` followed by `tools/list` instead, with no `notifications/initialized`
    /// (that notification belongs to the handshake this fallback exists because the server no
    /// longer speaks). Any other failure (transport/IO, a different JSON-RPC error, a
    /// malformed response) is reported as a real discovery failure, never silently retried
    /// under the second method — retrying on an ambiguous signal would risk masking a genuine
    /// reachability problem as a spec mismatch at census scale. Never calls a tool, under
    /// either path.
    pub fn discover(&mut self) -> Result<Discovery, DiscoveryError> {
        let init_params = json!({
            "protocolVersion": CLIENT_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": CLIENT_NAME, "version": CLIENT_VERSION },
        });

        match self.transport.call("initialize", init_params) {
            Ok(init_response) => {
                let negotiated_spec_revision = extract_negotiated_version(&init_response.bytes)?;
                self.transport.set_negotiated_protocol_version(&negotiated_spec_revision);

                self.transport.notify("notifications/initialized", json!({}))?;

                let tools_response = self.transport.call("tools/list", json!({}))?;

                Ok(Discovery {
                    handshake_raw: init_response.bytes,
                    tools_list_raw: tools_response.bytes,
                    negotiated_spec_revision,
                    handshake_path: HandshakePath::Initialize,
                })
            }
            Err(DiscoveryError::ServerError { code: -32601, .. }) => {
                let discover_params = json!({
                    "clientInfo": { "name": CLIENT_NAME, "version": CLIENT_VERSION },
                });
                let discover_response = self.transport.call("server/discover", discover_params)?;
                let negotiated_spec_revision = extract_negotiated_version(&discover_response.bytes)
                    .unwrap_or_else(|_| SERVER_DISCOVER_PROTOCOL_VERSION_FALLBACK.to_string());
                self.transport.set_negotiated_protocol_version(&negotiated_spec_revision);

                let tools_response = self.transport.call("tools/list", json!({}))?;

                Ok(Discovery {
                    handshake_raw: discover_response.bytes,
                    tools_list_raw: tools_response.bytes,
                    negotiated_spec_revision,
                    handshake_path: HandshakePath::ServerDiscover,
                })
            }
            Err(other) => Err(other),
        }
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
