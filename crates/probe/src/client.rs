//! A JSON-RPC client that, unlike [`discovery::DiscoveryClient`], can send an arbitrary
//! MCP method — `resources/list`, `resources/read`, `tools/call`. That capability is the
//! entire reason this crate exists separately from `discovery` rather than extending it:
//! P0-01's contract for `discovery` is "must not call any tool," enforced structurally by
//! never exposing a method-taking call. Adding one here would either weaken that guarantee
//! or require a second, parallel type inside the same crate — worse for readers than a
//! second crate whose own doc comment states plainly what it does.
//!
//! Streamable HTTP only (Class B servers are remote HTTP endpoints by definition —
//! architecture.md §2). No stdio transport: Track B never launches anything locally.
//!
//! This duplicates a small amount of `discovery::transport::HttpTransport` (request
//! framing over `ureq`, response decoding, the SSE-stream rejection). That module is
//! `pub(crate)` to `discovery` on purpose — see its doc comment — so it cannot be reused
//! here without exposing the same call surface P0-01 deliberately withholds. What *is*
//! shared is [`discovery::jsonrpc::encode_request`]/`encode_notification`, which carry no
//! execution semantics and were made `pub` for exactly this reuse.

use std::time::Duration;

use discovery::jsonrpc::{encode_notification, encode_request};
use serde::Deserialize;
use serde_json::Value;

/// Identifies this component distinctly from `discovery`'s own `User-Agent` — a server
/// operator inspecting logs should be able to tell "was discovered" apart from "had a tool
/// invoked against it," and the latter deserves the more conspicuous label.
const USER_AGENT: &str = concat!(
    "mcp-conformance-harness-probe/",
    env!("CARGO_PKG_VERSION"),
    " (annotation conformance research — Track B protocol-probe oracle; THIS REQUEST MAY \
     INVOKE A TOOL; contact: see repository)"
);

/// Kept in sync by hand with `discovery`'s own `CLIENT_PROTOCOL_VERSION` — that constant
/// is private to `discovery::client`, and promoting it to a shared constant is more churn
/// than this crate's one call site justifies today. O-01 (spec-revision tracking) is the
/// natural place to notice drift if these two ever disagree.
const CLIENT_PROTOCOL_VERSION: &str = "2025-11-25";

/// Why a probe-client operation failed.
#[derive(Debug)]
pub enum ProbeError {
    /// A transport-level failure: connection, TLS, timeout, non-2xx status.
    Transport(String),
    /// The response was not well-formed JSON-RPC, or its shape was unexpected.
    Protocol(String),
    /// The server returned a JSON-RPC error object — for example, "method not found" when
    /// probing for a capability the server doesn't have.
    ServerError {
        /// The JSON-RPC error code.
        code: i64,
        /// The JSON-RPC error message.
        message: String,
    },
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(msg) => write!(f, "transport error: {msg}"),
            Self::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Self::ServerError { code, message } => {
                write!(f, "server returned error {code}: {message}")
            }
        }
    }
}

impl std::error::Error for ProbeError {}

/// A JSON-RPC-over-Streamable-HTTP client with no restriction on which method it can send.
///
/// Construct with [`Self::new`], then [`Self::initialize`] before anything else — MCP's
/// lifecycle requires it, the same way `discovery::DiscoveryClient` does.
pub struct ProbeClient {
    endpoint: String,
    agent: ureq::Agent,
    next_id: u64,
    negotiated_version: Option<String>,
}

impl ProbeClient {
    /// A client against `endpoint`, timing out any single request after `timeout`.
    #[must_use]
    pub fn new(endpoint: impl Into<String>, timeout: Duration) -> Self {
        let agent: ureq::Agent =
            ureq::Agent::config_builder().timeout_global(Some(timeout)).build().into();
        Self { endpoint: endpoint.into(), agent, next_id: 0, negotiated_version: None }
    }

    /// `initialize` + `notifications/initialized`, mirroring the MCP lifecycle
    /// `discovery::DiscoveryClient::discover` already performs. A probe target has always
    /// already passed discovery once; this repeats the handshake because a probe run is a
    /// fresh connection, not a continuation of a prior one.
    pub fn initialize(&mut self) -> Result<String, ProbeError> {
        let params = serde_json::json!({
            "protocolVersion": CLIENT_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "mcp-conformance-harness-probe",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        let result = self.call("initialize", params)?;
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProbeError::Protocol("initialize result missing protocolVersion".into())
            })?
            .to_string();
        self.negotiated_version = Some(version.clone());
        self.notify("notifications/initialized", serde_json::json!({}))?;
        Ok(version)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), ProbeError> {
        let body = encode_notification(method, params);
        self.post(&body)?;
        Ok(())
    }

    /// Send an arbitrary JSON-RPC request and return its `result` on success.
    ///
    /// This is the deliberate capability `discovery` withholds — see the crate and module
    /// doc comments for why that capability belongs here and not there.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, ProbeError> {
        let id = self.next_id;
        self.next_id += 1;
        let body = encode_request(id, method, params);

        let mut response = self.post(&body)?;

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if content_type.contains("text/event-stream") {
            return Err(ProbeError::Protocol(
                "server responded with an SSE stream; this client handles only the \
                 single-JSON-response case of Streamable HTTP, same exclusion as discovery"
                    .into(),
            ));
        }

        let bytes = response
            .body_mut()
            .read_to_vec()
            .map_err(|e| ProbeError::Transport(e.to_string()))?;

        #[derive(Deserialize)]
        struct Envelope {
            #[serde(default)]
            id: Value,
            #[serde(default)]
            result: Option<Value>,
            #[serde(default)]
            error: Option<ErrorObject>,
        }
        #[derive(Deserialize)]
        struct ErrorObject {
            code: i64,
            message: String,
        }

        let envelope: Envelope = serde_json::from_slice(&bytes)
            .map_err(|e| ProbeError::Protocol(format!("response is not valid JSON-RPC: {e}")))?;

        let got_id = envelope.id.as_u64().ok_or_else(|| {
            ProbeError::Protocol("response id is missing or not an integer".into())
        })?;
        if got_id != id {
            return Err(ProbeError::Protocol(format!(
                "response id {got_id} does not match request id {id}"
            )));
        }

        if let Some(err) = envelope.error {
            return Err(ProbeError::ServerError { code: err.code, message: err.message });
        }
        envelope
            .result
            .ok_or_else(|| ProbeError::Protocol("response has neither result nor error".into()))
    }

    fn post(&self, body: &[u8]) -> Result<ureq::http::Response<ureq::Body>, ProbeError> {
        let mut builder = self
            .agent
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("User-Agent", USER_AGENT);
        if let Some(v) = &self.negotiated_version {
            builder = builder.header("MCP-Protocol-Version", v);
        }
        builder.send(body).map_err(|e| ProbeError::Transport(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn read_one_http_request(stream: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).expect("read header byte");
            buf.push(byte[0]);
            if buf.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let headers = String::from_utf8_lossy(&buf).into_owned();
        let content_length: usize = headers
            .lines()
            .find_map(|line| {
                let lower = line.to_ascii_lowercase();
                lower.strip_prefix("content-length:").map(|v| v.trim().parse().expect("numeric"))
            })
            .unwrap_or(0);
        let mut body = vec![0u8; content_length];
        stream.read_exact(&mut body).expect("read body");
        headers
    }

    fn write_json_response(stream: &mut TcpStream, body: &[u8]) {
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(header.as_bytes()).expect("write header");
        stream.write_all(body).expect("write body");
        stream.flush().expect("flush");
    }

    fn spawn_server(
        listener: TcpListener,
        responses: Vec<serde_json::Value>,
    ) -> thread::JoinHandle<Vec<String>> {
        thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept");
                let request = read_one_http_request(&mut stream);
                assert!(
                    request.to_ascii_lowercase().contains("user-agent: mcp-conformance-harness-probe"),
                    "probe requests must identify themselves distinctly: {request}"
                );
                requests.push(request);
                write_json_response(&mut stream, &serde_json::to_vec(&response).unwrap());
            }
            requests
        })
    }

    #[test]
    fn call_round_trips_a_result() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        let server = spawn_server(
            listener,
            vec![serde_json::json!({"jsonrpc":"2.0","id":0,"result":{"echo":true}})],
        );

        let mut client = ProbeClient::new(endpoint, Duration::from_secs(5));
        let result = client.call("ping", serde_json::json!({})).expect("call");
        assert_eq!(result["echo"], true);
        server.join().expect("server thread");
    }

    #[test]
    fn call_surfaces_a_json_rpc_error() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        let server = spawn_server(
            listener,
            vec![serde_json::json!({
                "jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"Method not found"}
            })],
        );

        let mut client = ProbeClient::new(endpoint, Duration::from_secs(5));
        let err = client.call("resources/list", serde_json::json!({})).expect_err("must fail");
        match err {
            ProbeError::ServerError { code, message } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "Method not found");
            }
            other => panic!("expected ServerError, got {other:?}"),
        }
        server.join().expect("server thread");
    }

    #[test]
    fn call_rejects_a_mismatched_response_id() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        let server = spawn_server(
            listener,
            vec![serde_json::json!({"jsonrpc":"2.0","id":99,"result":{}})],
        );

        let mut client = ProbeClient::new(endpoint, Duration::from_secs(5));
        let err = client.call("ping", serde_json::json!({})).expect_err("must fail");
        assert!(matches!(err, ProbeError::Protocol(_)));
        server.join().expect("server thread");
    }

    #[test]
    fn initialize_negotiates_the_protocol_version_and_sends_the_lifecycle_notification() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        // Two responses scripted: `initialize`, then the `notifications/initialized` POST
        // (its body is never read by the client, but the HTTP exchange still needs
        // completing). Exactly two — proving the notification really was sent requires a
        // listener that accepts exactly twice, no more, no fewer: if `initialize` failed to
        // send the notification, `server.join()` below would hang waiting for a second
        // connection nobody made.
        let server = spawn_server(
            listener,
            vec![
                serde_json::json!({
                    "jsonrpc":"2.0","id":0,
                    "result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"fake","version":"0"}}
                }),
                serde_json::json!({}),
            ],
        );

        let mut client = ProbeClient::new(endpoint, Duration::from_secs(5));
        let version = client.initialize().expect("initialize");
        assert_eq!(version, "2025-11-25");
        let requests = server.join().expect("server thread");
        assert_eq!(requests.len(), 2);
        assert!(requests[1].to_ascii_lowercase().contains("mcp-protocol-version: 2025-11-25"));
    }
}
