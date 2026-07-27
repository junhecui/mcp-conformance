//! Fake MCP server for `discovery`'s stdio integration tests — not shipped, not part of
//! the harness's own build graph in any behavioural sense (ADR-007 §"Second language"
//! treats fixtures and test servers as throwaway).
//!
//! Speaks just enough stdio JSON-RPC to exercise `DiscoveryClient::discover`: responds to
//! `initialize` and `tools/list`, silently accepts the `notifications/initialized`
//! notification (no id, no response). `argv[1]` selects a mode so one binary covers the
//! happy path plus a few adversarial server behaviours P0-01 must reject rather than
//! silently accept.

use std::io::{self, BufRead, Write};

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "happy".to_string());
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = line.expect("read a line from the discovery client");
        if line.is_empty() {
            continue;
        }
        let request: serde_json::Value =
            serde_json::from_str(&line).expect("client always sends valid JSON-RPC");
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");

        // A notification carries no id and gets no response.
        let Some(id) = request.get("id").cloned() else {
            continue;
        };

        if mode == "malformed" && method == "initialize" {
            writeln!(stdout, "{{not valid json").expect("write");
            stdout.flush().expect("flush");
            continue;
        }

        // Never responds — stands in for a hung or deliberately stalling server, so
        // `stdio_with_timeout`'s watchdog has something real to kill.
        if mode == "hang" && method == "initialize" {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }

        let response = match method {
            "initialize" if mode == "wrong_id" => serde_json::json!({
                "jsonrpc": "2.0",
                "id": 999_999,
                "result": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "serverInfo": { "name": "fake-mcp-stdio-server", "version": "0.0.0" }
                }
            }),
            "initialize" if mode == "error" => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32000, "message": "initialize rejected by fake server" }
            }),
            "initialize" => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "serverInfo": { "name": "fake-mcp-stdio-server", "version": "0.0.0" }
                }
            }),
            "tools/list" => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [{
                        "name": "read_file",
                        "description": "Reads a file",
                        "inputSchema": {
                            "type": "object",
                            "properties": { "path": { "type": "string" } }
                        },
                        "annotations": { "readOnlyHint": true }
                    }]
                }
            }),
            other => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {other}") }
            }),
        };

        writeln!(stdout, "{response}").expect("write response");
        stdout.flush().expect("flush");
    }
}
