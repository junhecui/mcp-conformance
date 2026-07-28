//! P0-01 exit criterion, HTTP half: `initialize` + `tools/list` succeeds against a real
//! remote HTTP server.
//!
//! The fake server here is a minimal hand-rolled HTTP/1.1 responder over
//! `std::net::TcpListener` — enough to read one `Content-Length`-framed request and write
//! one response, which is all the Streamable HTTP single-JSON-response case needs. No new
//! dependency for a few dozen lines that exist only to drive a test.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use discovery::{DiscoveryClient, DiscoveryPath};

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
            lower
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse().expect("numeric Content-Length"))
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

#[test]
fn discover_succeeds_against_a_real_http_server() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!("http://{}/mcp", listener.local_addr().expect("local addr"));

    let server = thread::spawn(move || {
        let init_result = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "serverInfo": { "name": "fake-mcp-http-server", "version": "0.0.0" }
            }
        });
        let tools_result = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "tools": [{
                    "name": "read_file",
                    "description": "Reads a file",
                    "inputSchema": { "type": "object" },
                    "annotations": { "readOnlyHint": true }
                }]
            }
        });

        // initialize
        let (mut stream, _) = listener.accept().expect("accept #1 (initialize)");
        let request = read_one_http_request(&mut stream);
        let lower = request.to_ascii_lowercase();
        assert!(
            lower.contains("user-agent: mcp-conformance-harness"),
            "discovery must identify itself to third-party servers, not poll anonymously: {request}"
        );
        assert!(
            lower.contains("accept: application/json, text/event-stream"),
            "the Streamable HTTP spec requires both content types in Accept, or spec-compliant \
             servers correctly reject the request with 406: {request}"
        );
        write_json_response(&mut stream, &serde_json::to_vec(&init_result).unwrap());

        // notifications/initialized
        let (mut stream, _) = listener.accept().expect("accept #2 (notifications/initialized)");
        read_one_http_request(&mut stream);
        write_json_response(&mut stream, b"{}");

        // tools/list
        let (mut stream, _) = listener.accept().expect("accept #3 (tools/list)");
        read_one_http_request(&mut stream);
        write_json_response(&mut stream, &serde_json::to_vec(&tools_result).unwrap());
    });

    let mut client = DiscoveryClient::http(endpoint);
    let discovery = client.discover().expect("discover must succeed");
    server.join().expect("fake server thread must not panic");

    assert_eq!(discovery.negotiated_spec_revision, "2025-11-25");

    let tools: serde_json::Value =
        serde_json::from_slice(&discovery.tools_list_raw).expect("valid JSON");
    assert_eq!(tools["result"]["tools"][0]["name"], "read_file");
}

/// P0-09 over HTTP: `initialize` gets a JSON-RPC `-32601 Method not found` error (the shape
/// a server that only speaks spec `2026-07-28`'s handshake-free lifecycle is expected to
/// produce), so `discover()` retries with `server/discover` — skipping
/// `notifications/initialized`, since there is no `initialize` result to acknowledge — and
/// the negotiated version from that response is what gets sent as `MCP-Protocol-Version` on
/// the subsequent `tools/list` request.
#[test]
fn discover_falls_back_to_server_discover_over_http() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!("http://{}/mcp", listener.local_addr().expect("local addr"));

    let server = thread::spawn(move || {
        let init_error = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "error": { "code": -32601, "message": "method not found: initialize" }
        });
        let discover_result = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2026-07-28",
                "capabilities": {},
                "serverInfo": { "name": "fake-mcp-http-server", "version": "0.0.0" }
            }
        });
        let tools_result = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "tools": [{
                    "name": "read_file",
                    "description": "Reads a file",
                    "inputSchema": { "type": "object" },
                    "annotations": { "readOnlyHint": true }
                }]
            }
        });

        // initialize — rejected as unrecognized
        let (mut stream, _) = listener.accept().expect("accept #1 (initialize)");
        read_one_http_request(&mut stream);
        write_json_response(&mut stream, &serde_json::to_vec(&init_error).unwrap());

        // server/discover — the fallback
        let (mut stream, _) = listener.accept().expect("accept #2 (server/discover)");
        read_one_http_request(&mut stream);
        write_json_response(&mut stream, &serde_json::to_vec(&discover_result).unwrap());

        // tools/list — must carry MCP-Protocol-Version negotiated via server/discover
        let (mut stream, _) = listener.accept().expect("accept #3 (tools/list)");
        let request = read_one_http_request(&mut stream);
        let lower = request.to_ascii_lowercase();
        assert!(
            lower.contains("mcp-protocol-version: 2026-07-28"),
            "the version negotiated via server/discover must still be sent on later \
             requests, the same as it would be after a successful initialize: {request}"
        );
        write_json_response(&mut stream, &serde_json::to_vec(&tools_result).unwrap());
    });

    let mut client = DiscoveryClient::http(endpoint);
    let discovery = client.discover().expect("discover must succeed via the fallback path");
    server.join().expect("fake server thread must not panic");

    assert_eq!(discovery.negotiated_spec_revision, "2026-07-28");
    assert_eq!(discovery.discovery_path, DiscoveryPath::ServerDiscover);

    let tools: serde_json::Value =
        serde_json::from_slice(&discovery.tools_list_raw).expect("valid JSON");
    assert_eq!(tools["result"]["tools"][0]["name"], "read_file");
}

/// Review finding on P0-09: a transport-level failure (connection refused, DNS, TLS, and —
/// exercised here — hitting the configured request timeout) must **not** trigger the
/// `server/discover` fallback. Every such failure maps to `DiscoveryError::Transport` (see
/// `transport.rs`'s `post`), and a connection-level failure means the transport never
/// reached the server at all — retrying tells you nothing about which method the server
/// implements, unlike a `-32601`, and only doubles the cost of every unresponsive host in a
/// sweep-scale census run (`xtask/src/census_stage1.rs` tunes its timeout down specifically
/// to bound that cost).
///
/// The fake server here accepts connections but never writes a response, so `ureq`'s own
/// request timeout — not a server-side rejection — is what ends the call. Asserted two ways:
/// directly, by counting how many connections the server actually receives (exactly one,
/// the `initialize` attempt — a second would mean the fallback fired), and as a wall-clock
/// proxy (the whole call returns in roughly one timeout, not two), matching the convention
/// already used by `stdio_discovery.rs`'s watchdog test.
#[test]
fn discover_does_not_fall_back_after_a_transport_level_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("set_nonblocking");
    let endpoint = format!("http://{}/mcp", listener.local_addr().expect("local addr"));

    let timeout = Duration::from_millis(300);
    // Generous window to catch a regressed fallback: if `discover()` retried, the second
    // connection attempt would arrive within milliseconds of the first failure (TCP connect
    // itself doesn't wait out the timeout, only the response read does), well inside this.
    let observe_window = timeout * 2;

    let server = thread::spawn(move || {
        let start = Instant::now();
        // Held open for the lifetime of the thread so a connection never gets a response —
        // the client's own timeout, not a server action, must be what ends the call.
        let mut held_open = Vec::new();
        while start.elapsed() < observe_window && held_open.len() < 2 {
            match listener.accept() {
                Ok((stream, _)) => held_open.push(stream),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("accept error: {e}"),
            }
        }
        held_open.len()
    });

    let mut client = DiscoveryClient::http_with_timeout(endpoint, timeout);
    let started = Instant::now();
    let err = client.discover().expect_err("a connection-level timeout must surface, not be masked");
    let elapsed = started.elapsed();

    assert!(
        matches!(err, discovery::DiscoveryError::Transport(_)),
        "expected the original Transport error to surface untouched, got {err:?}"
    );
    assert!(
        elapsed < timeout * 2,
        "discover() must fail after roughly one timeout ({timeout:?}), not attempt a second \
         full round trip via server/discover: took {elapsed:?}"
    );

    let connection_count = server.join().expect("fake server thread must not panic");
    assert_eq!(
        connection_count, 1,
        "exactly one connection (the initialize attempt) must be made; a second means the \
         server/discover fallback was incorrectly attempted after a transport-level failure"
    );
}

#[test]
fn discover_rejects_an_sse_upgrade_it_does_not_support() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!("http://{}/mcp", listener.local_addr().expect("local addr"));

    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        read_one_http_request(&mut stream);
        let body = b"event: message\ndata: {}\n\n";
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(header.as_bytes()).expect("write header");
        stream.write_all(body).expect("write body");
        stream.flush().expect("flush");
    });

    let mut client = DiscoveryClient::http(endpoint);
    let err = client.discover().expect_err("SSE upgrade must be rejected, not mishandled");
    server.join().expect("fake server thread must not panic");

    assert!(matches!(err, discovery::DiscoveryError::Protocol(_)));
}
