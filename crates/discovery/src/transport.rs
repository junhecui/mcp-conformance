//! Transport implementations. `pub(crate)` throughout — this is the structural half of
//! P0-01's "must not call any tool": [`Transport::call`] takes an arbitrary method string,
//! and nothing outside this crate can name the trait to reach it. [`crate::DiscoveryClient`]
//! is the only public door in, and it only ever calls `call`/`notify` with the three MCP
//! method names literal in `client.rs`.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use serde_json::Value;

use crate::jsonrpc::{self, ResponseEnvelope};
use crate::DiscoveryError;

/// Identifies this harness to any HTTP server it discovers against — visible polling, not
/// anonymous, the same disclosure posture `intake::registry` uses against the registry
/// itself.
const USER_AGENT: &str = concat!(
    "mcp-conformance-harness/",
    env!("CARGO_PKG_VERSION"),
    " (annotation conformance research; read-only discovery; contact: see repository)"
);

/// One raw JSON-RPC response, exactly as received, before any parsing.
///
/// P0-01: "the pin depends on this." These bytes ride untouched all the way out to
/// [`crate::Discovery`], alongside — never instead of — whatever gets parsed out of them
/// to route the response.
#[derive(Debug)]
pub(crate) struct RawResponse {
    pub bytes: Vec<u8>,
}

pub(crate) trait Transport {
    fn call(&mut self, method: &str, params: Value) -> Result<RawResponse, DiscoveryError>;
    fn notify(&mut self, method: &str, params: Value) -> Result<(), DiscoveryError>;

    /// Streamable HTTP requires the negotiated `MCP-Protocol-Version` header on every
    /// request after `initialize`. A no-op for stdio, which has no headers.
    fn set_negotiated_protocol_version(&mut self, _version: &str) {}
}

/// Parse just enough of a response to route it, without discarding the bytes it came from.
/// Both transports funnel through this so id-mismatch and JSON-RPC-error handling live in
/// one place. A wrong or reused id is treated as a protocol violation, not tolerated —
/// discovery runs against a server the trust model assumes is actively hostile.
fn decode_and_validate(bytes: &[u8], expected_id: u64) -> Result<RawResponse, DiscoveryError> {
    let envelope: ResponseEnvelope = serde_json::from_slice(bytes)
        .map_err(|e| DiscoveryError::Protocol(format!("response is not valid JSON-RPC: {e}")))?;

    let got_id = envelope
        .id
        .as_u64()
        .ok_or_else(|| DiscoveryError::Protocol("response id is missing or not an integer".into()))?;
    if got_id != expected_id {
        return Err(DiscoveryError::Protocol(format!(
            "response id {got_id} does not match request id {expected_id}"
        )));
    }

    if let Some(error) = envelope.error {
        return Err(DiscoveryError::ServerError { code: error.code, message: error.message });
    }
    if envelope.result.is_none() {
        return Err(DiscoveryError::Protocol("response has neither result nor error".into()));
    }

    Ok(RawResponse { bytes: bytes.to_vec() })
}

// ---- stdio ----

/// Newline-delimited JSON-RPC over a pair of byte streams (MCP's stdio framing: one
/// message per line, no embedded newlines).
///
/// Generic over `Read`/`Write` so tests can drive it over an in-process `UnixStream` pair
/// instead of a real subprocess; [`ChildProcessTransport`] below is what production code
/// actually constructs.
pub(crate) struct StdioTransport<R, W> {
    reader: BufReader<R>,
    writer: W,
    next_id: u64,
}

impl<R: Read, W: Write> StdioTransport<R, W> {
    pub(crate) fn new(reader: R, writer: W) -> Self {
        Self { reader: BufReader::new(reader), writer, next_id: 0 }
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), DiscoveryError> {
        debug_assert!(!bytes.contains(&b'\n'), "a JSON-RPC line must not embed a newline");
        self.writer.write_all(bytes).map_err(DiscoveryError::Io)?;
        self.writer.write_all(b"\n").map_err(DiscoveryError::Io)?;
        self.writer.flush().map_err(DiscoveryError::Io)
    }

    fn recv_line(&mut self) -> Result<Vec<u8>, DiscoveryError> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).map_err(DiscoveryError::Io)?;
        if n == 0 {
            return Err(DiscoveryError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "stdio transport closed before a response was received",
            )));
        }
        Ok(line.trim_end_matches(['\n', '\r']).as_bytes().to_vec())
    }
}

impl<R: Read, W: Write> Transport for StdioTransport<R, W> {
    fn call(&mut self, method: &str, params: Value) -> Result<RawResponse, DiscoveryError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&jsonrpc::encode_request(id, method, params))?;
        let bytes = self.recv_line()?;
        decode_and_validate(&bytes, id)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), DiscoveryError> {
        self.send(&jsonrpc::encode_notification(method, params))
    }
}

/// A [`StdioTransport`] over a spawned child process's stdio, owning the [`Child`] so it
/// gets reaped on drop instead of left as a zombie or orphan. Matters at census scale: a
/// discovery target that never exits on its own must not accumulate across a run of
/// thousands of servers.
pub(crate) struct ChildProcessTransport {
    child: Child,
    inner: StdioTransport<ChildStdout, ChildStdin>,
}

impl ChildProcessTransport {
    pub(crate) fn spawn(
        program: impl AsRef<std::ffi::OsStr>,
        args: &[&str],
    ) -> Result<Self, DiscoveryError> {
        Self::spawn_with_timeout(program, args, None)
    }

    /// Same as [`Self::spawn`], plus an optional hard wall-clock deadline on the child's
    /// lifetime.
    ///
    /// Exists for Stage 2 census (`docker run` wrapping a locally-launchable Class A
    /// package): a hostile or merely broken tool under `initialize`/`tools/list` can hang
    /// indefinitely, and `StdioTransport::recv_line` blocks with no timeout of its own. A
    /// watcher thread sleeps `timeout` and then sends the child a kill signal if it is still
    /// running; the killed process's stdout closing is what unblocks a pending
    /// `recv_line` (it surfaces as the ordinary "peer closed" `DiscoveryError::Io`, the same
    /// path an early-exiting server already takes).
    ///
    /// Best-effort by construction, not a containment mechanism: this is a watchdog for a
    /// hung *discovery* call, not the sandbox. If the child already exited before the
    /// deadline, the watcher's kill targets a pid the OS may since have reused — an accepted
    /// risk for a short (tens-of-seconds) timeout window in a census tool, not something
    /// Phase 1+'s actual containment may ever rely on.
    pub(crate) fn spawn_with_timeout(
        program: impl AsRef<std::ffi::OsStr>,
        args: &[&str],
        timeout: Option<Duration>,
    ) -> Result<Self, DiscoveryError> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(DiscoveryError::Io)?;

        if let Some(timeout) = timeout {
            let pid = child.id();
            std::thread::spawn(move || {
                std::thread::sleep(timeout);
                // Best-effort: ignore the exit status entirely. If the child already
                // exited, this either fails harmlessly or (rarely, pid reuse) kills an
                // unrelated process — see the doc comment above.
                let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            });
        }

        let stdout = child.stdout.take().expect("spawned with Stdio::piped()");
        let stdin = child.stdin.take().expect("spawned with Stdio::piped()");
        Ok(Self { child, inner: StdioTransport::new(stdout, stdin) })
    }
}

impl Transport for ChildProcessTransport {
    fn call(&mut self, method: &str, params: Value) -> Result<RawResponse, DiscoveryError> {
        self.inner.call(method, params)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), DiscoveryError> {
        self.inner.notify(method, params)
    }
}

impl Drop for ChildProcessTransport {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---- HTTP (Streamable HTTP, single-JSON-response case) ----

/// Streamable HTTP transport, limited to the non-streaming case: POST a JSON-RPC message,
/// get a `Content-Type: application/json` body back. A server that upgrades to
/// `text/event-stream` gets a clear [`DiscoveryError::Protocol`] rather than silent
/// mishandling — SSE support is out of scope for P0-01's thinnest path.
pub(crate) struct HttpTransport {
    endpoint: String,
    agent: ureq::Agent,
    next_id: u64,
    negotiated_version: Option<String>,
}

impl HttpTransport {
    pub(crate) fn new(endpoint: String) -> Self {
        Self::with_timeout(endpoint, Duration::from_secs(30))
    }

    pub(crate) fn with_timeout(endpoint: String, timeout: Duration) -> Self {
        let agent: ureq::Agent =
            ureq::Agent::config_builder().timeout_global(Some(timeout)).build().into();
        Self { endpoint, agent, next_id: 0, negotiated_version: None }
    }

    fn post(&self, body: &[u8]) -> Result<ureq::http::Response<ureq::Body>, DiscoveryError> {
        let mut builder = self
            .agent
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            // The Streamable HTTP spec (2025-03-26) requires both content types here even
            // though this transport only handles the single-JSON-response case: "the client
            // MUST include an Accept header, listing both application/json and
            // text/event-stream." Advertising only application/json gets a spec-compliant
            // server to correctly reject the request with 406 — found running Stage 1
            // against live servers: 14 of 78 failures were exactly this, not a real
            // reachability problem. The scope exclusion is unaffected: if a server responds
            // with an SSE stream anyway, that is still rejected below, just no longer
            // provoked by an under-declared Accept header in the first place.
            .header("Accept", "application/json, text/event-stream")
            .header("User-Agent", USER_AGENT);
        if let Some(v) = &self.negotiated_version {
            builder = builder.header("MCP-Protocol-Version", v);
        }
        builder.send(body).map_err(|e| DiscoveryError::Transport(e.to_string()))
    }
}

impl Transport for HttpTransport {
    fn call(&mut self, method: &str, params: Value) -> Result<RawResponse, DiscoveryError> {
        let id = self.next_id;
        self.next_id += 1;
        let body = jsonrpc::encode_request(id, method, params);

        let mut response = self.post(&body)?;

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if content_type.contains("text/event-stream") {
            return Err(DiscoveryError::Protocol(
                "server responded with an SSE stream; this transport handles only the \
                 single-JSON-response case of Streamable HTTP"
                    .into(),
            ));
        }

        let bytes = response
            .body_mut()
            .read_to_vec()
            .map_err(|e| DiscoveryError::Transport(e.to_string()))?;
        decode_and_validate(&bytes, id)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), DiscoveryError> {
        let body = jsonrpc::encode_notification(method, params);
        self.post(&body)?;
        Ok(())
    }

    fn set_negotiated_protocol_version(&mut self, version: &str) {
        self.negotiated_version = Some(version.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_and_validate_accepts_a_matching_result() {
        let bytes = br#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#;
        let raw = decode_and_validate(bytes, 3).expect("must decode");
        assert_eq!(raw.bytes, bytes, "raw bytes must be returned untouched");
    }

    #[test]
    fn decode_and_validate_rejects_id_mismatch() {
        let bytes = br#"{"jsonrpc":"2.0","id":4,"result":{}}"#;
        let err = decode_and_validate(bytes, 3).expect_err("must reject");
        assert!(matches!(err, DiscoveryError::Protocol(_)));
    }

    #[test]
    fn decode_and_validate_surfaces_a_json_rpc_error() {
        let bytes = br#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"nope"}}"#;
        let err = decode_and_validate(bytes, 3).expect_err("must reject");
        match err {
            DiscoveryError::ServerError { code, message } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "nope");
            }
            other => panic!("expected ServerError, got {other:?}"),
        }
    }

    #[test]
    fn decode_and_validate_rejects_malformed_json() {
        let err = decode_and_validate(b"not json", 0).expect_err("must reject");
        assert!(matches!(err, DiscoveryError::Protocol(_)));
    }

    #[test]
    fn decode_and_validate_rejects_a_response_with_neither_result_nor_error() {
        let bytes = br#"{"jsonrpc":"2.0","id":0}"#;
        let err = decode_and_validate(bytes, 0).expect_err("must reject");
        assert!(matches!(err, DiscoveryError::Protocol(_)));
    }

    /// Exercises the real framing (`send`/`recv_line`) over a genuine bidirectional OS
    /// pipe — a faithful stand-in for a stdio pipe, since both are just byte streams under
    /// `Read`/`Write`, without needing a full subprocess for a framing-level test.
    #[test]
    fn stdio_transport_round_trips_over_a_real_pipe() {
        use std::os::unix::net::UnixStream;
        use std::thread;

        let (client_side, server_side) = UnixStream::pair().expect("socket pair");
        let server = thread::spawn(move || {
            let mut reader = BufReader::new(server_side.try_clone().expect("clone"));
            let mut writer = server_side;
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");
            let request: Value = serde_json::from_str(&line).expect("valid JSON");
            let id = request["id"].clone();
            let response = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"echo": true}});
            writeln!(writer, "{response}").expect("write response");
        });

        let mut transport =
            StdioTransport::new(client_side.try_clone().expect("clone"), client_side);
        let raw = transport.call("ping", Value::Null).expect("call");
        let value: Value = serde_json::from_slice(&raw.bytes).expect("valid JSON");
        assert_eq!(value["result"]["echo"], true);

        server.join().expect("server thread must not panic");
    }

    #[test]
    fn stdio_transport_reports_an_error_when_the_peer_closes_without_responding() {
        use std::os::unix::net::UnixStream;

        let (client_side, server_side) = UnixStream::pair().expect("socket pair");
        drop(server_side); // simulate the server process exiting immediately

        let mut transport =
            StdioTransport::new(client_side.try_clone().expect("clone"), client_side);
        let err = transport.call("ping", Value::Null).expect_err("peer is gone");
        assert!(matches!(err, DiscoveryError::Io(_)));
    }
}
