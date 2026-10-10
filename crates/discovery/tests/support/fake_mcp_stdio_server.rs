//! Fake MCP server for `discovery`'s stdio integration tests — not shipped, not part of
//! the harness's own build graph in any behavioural sense (ADR-007 §"Second language"
//! treats fixtures and test servers as throwaway).
//!
//! Speaks just enough stdio JSON-RPC to exercise `DiscoveryClient::discover` in both eras:
//! `server/discover` (spec `2026-07-28`), `initialize` + `notifications/initialized` (spec
//! `2025-11-25` and earlier), and `tools/list`. `argv[1]` selects a mode so one binary
//! covers a modern-only server, a legacy-only server, a dual-era server, the three modern
//! error codes that must *not* provoke a downgrade or must re-negotiate, and the adversarial
//! behaviours P0-01 requires rejecting rather than silently accepting.
//!
//! The modern modes validate the client's `_meta` block rather than ignoring it: a
//! `2026-07-28` request whose `_meta` lacks the required `protocolVersion` or
//! `clientCapabilities` is answered `-32021 MissingRequiredClientCapability`, which is what
//! a spec-compliant server does and what makes the test prove the client sends it. P0-09
//! shipped without it, so a fake that shrugged would have passed too.

use std::io::{self, BufRead, Write};

const MODERN: &str = "2026-07-28";
const LEGACY: &str = "2025-11-25";

/// The tools array every mode returns, so a pin computed from one era's response can be
/// compared against the other's.
fn tools() -> serde_json::Value {
    serde_json::json!([{
        "name": "read_file",
        "description": "Reads a file",
        "inputSchema": {
            "type": "object",
            "properties": { "path": { "type": "string" } }
        },
        "annotations": { "readOnlyHint": true }
    }])
}

/// Does `params` carry a well-formed `2026-07-28` `_meta` block at `MODERN`?
fn has_modern_meta(params: &serde_json::Value) -> bool {
    let Some(meta) = params.get("_meta") else { return false };
    meta.get("io.modelcontextprotocol/protocolVersion").and_then(|v| v.as_str()) == Some(MODERN)
        && meta.get("io.modelcontextprotocol/clientCapabilities").is_some_and(|v| v.is_object())
}

fn error(id: &serde_json::Value, code: i64, message: &str) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

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
        let params = request.get("params").cloned().unwrap_or(serde_json::Value::Null);

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

        // Exits rather than answering an unknown method. Real, not-unusual legacy behaviour,
        // and the one case where modern-first was strictly *worse* than initialize-first over
        // stdio: the probe destroys the channel, so the `initialize` fallback then wrote to a
        // closed stdin (`Broken pipe`) and the server could not be discovered at all. The
        // client now re-spawns for the fallback when the probe produced no bytes at all —
        // exactly this case — and this mode is what proves it, by answering `initialize`
        // normally once it gets a live channel again.
        if mode == "exit_on_unknown" && method == "server/discover" {
            std::process::exit(0);
        }

        // Every modern mode requires the `_meta` block on every request it serves.
        let modern_mode = matches!(mode.as_str(), "modern" | "dual");
        if modern_mode
            && matches!(method, "server/discover" | "tools/list")
            && !has_modern_meta(&params)
        {
            let response =
                error(&id, -32021, "missing required _meta protocolVersion/clientCapabilities");
            writeln!(stdout, "{response}").expect("write");
            stdout.flush().expect("flush");
            continue;
        }

        let response = match (method, mode.as_str()) {
            // ---- modern (2026-07-28) handshake ----
            ("server/discover", "modern" | "dual") => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resultType": "complete",
                    "supportedVersions": [MODERN],
                    "capabilities": { "tools": { "listChanged": true } },
                    "ttlMs": 0,
                    "cacheScope": "private",
                    "_meta": { "io.modelcontextprotocol/serverInfo":
                        { "name": "fake-mcp-stdio-server", "version": "0.0.0" } }
                }
            }),
            // A `DiscoverResult` whose version list names only a legacy revision: the client
            // must use the legacy handshake at that revision rather than guessing.
            ("server/discover", "legacy_only_versions") => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resultType": "complete",
                    "supportedVersions": [LEGACY],
                    "capabilities": {},
                    "ttlMs": 0,
                    "cacheScope": "private"
                }
            }),
            // `-32022 UnsupportedProtocolVersionError`, offering a legacy revision.
            ("server/discover", "unsupported_version") => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32022,
                    "message": "unsupported protocol version",
                    "data": { "requested": MODERN, "supported": [LEGACY] }
                }
            }),
            // `-32022` offering nothing this client implements: a discovery failure.
            ("server/discover", "unsupported_version_unknown") => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32022,
                    "message": "unsupported protocol version",
                    "data": { "requested": MODERN, "supported": ["2031-01-01"] }
                }
            }),
            // `-32020 HeaderMismatch`: modern, and the request was wrong. Must surface.
            ("server/discover", "header_mismatch") => {
                error(&id, -32020, "MCP-Protocol-Version header does not match _meta")
            }

            // ---- legacy (2025-11-25 and earlier) handshake ----
            // A modern-only server has no `initialize`. Answering it with a *success* in the
            // modes below would let a wrongly-downgrading client pass, so these answer with
            // the standard unrecognized-method error instead.
            ("initialize", "modern") => error(&id, -32601, "method not found: initialize"),
            ("initialize", "wrong_id") => serde_json::json!({
                "jsonrpc": "2.0",
                "id": 999_999,
                "result": {
                    "protocolVersion": LEGACY,
                    "capabilities": {},
                    "serverInfo": { "name": "fake-mcp-stdio-server", "version": "0.0.0" }
                }
            }),
            ("initialize", "error") => {
                error(&id, -32000, "initialize rejected by fake server")
            }
            // Every other mode, including `unsupported_version_unknown` and
            // `header_mismatch`, answers `initialize` successfully *on purpose*: a client
            // that downgraded when it must not would then succeed, and the test asserting a
            // failure would catch it.
            ("initialize", _) => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": LEGACY,
                    "capabilities": {},
                    "serverInfo": { "name": "fake-mcp-stdio-server", "version": "0.0.0" }
                }
            }),

            // ---- tools/list, in both shapes ----
            // `2026-07-28` adds the required `resultType`, `ttlMs` and `cacheScope` to every
            // result. They sit outside P0-02's per-tool pin preimage, which is what
            // `tests/era_pin.rs` proves.
            ("tools/list", "modern" | "dual") => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resultType": "complete",
                    "ttlMs": 0,
                    "cacheScope": "private",
                    "tools": tools()
                }
            }),
            ("tools/list", _) => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tools() }
            }),

            (other, _) => error(&id, -32601, &format!("method not found: {other}")),
        };

        writeln!(stdout, "{response}").expect("write response");
        stdout.flush().expect("flush");
    }
}
