//! Minimal JSON-RPC 2.0 message shapes — just enough to frame `initialize` and
//! `tools/list` requests and to route a response (id match, `result` vs. `error`).
//!
//! Deliberately not a general JSON-RPC library: this crate speaks exactly two request
//! methods and one notification (see `client.rs`), and a fuller implementation would be
//! surface area nothing here needs.

use serde::{Deserialize, Serialize};
use serde_json::Value;

const JSONRPC_VERSION: &str = "2.0";

#[derive(Serialize)]
struct Request<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: Value,
}

#[derive(Serialize)]
struct Notification<'a> {
    jsonrpc: &'static str,
    method: &'a str,
    params: Value,
}

#[derive(Deserialize)]
pub(crate) struct ErrorObject {
    pub code: i64,
    pub message: String,
}

#[derive(Deserialize)]
pub(crate) struct ResponseEnvelope {
    #[serde(default)]
    pub id: Value,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<ErrorObject>,
}

pub(crate) fn encode_request(id: u64, method: &str, params: Value) -> Vec<u8> {
    serde_json::to_vec(&Request { jsonrpc: JSONRPC_VERSION, id, method, params })
        .expect("a JSON-RPC request over Value params always serialises")
}

pub(crate) fn encode_notification(method: &str, params: Value) -> Vec<u8> {
    serde_json::to_vec(&Notification { jsonrpc: JSONRPC_VERSION, method, params })
        .expect("a JSON-RPC notification over Value params always serialises")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_request_produces_valid_json_rpc_2_0() {
        let bytes = encode_request(7, "tools/list", serde_json::json!({}));
        let value: Value = serde_json::from_slice(&bytes).expect("valid JSON");
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 7);
        assert_eq!(value["method"], "tools/list");
    }

    #[test]
    fn encode_notification_has_no_id() {
        let bytes = encode_notification("notifications/initialized", serde_json::json!({}));
        let value: Value = serde_json::from_slice(&bytes).expect("valid JSON");
        assert!(value.get("id").is_none(), "a notification must not carry an id");
    }
}
