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

use discovery::DiscoveryClient;

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
