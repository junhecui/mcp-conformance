//! P0-01 exit criterion, stdio half: `initialize` + `tools/list` succeeds against a real
//! stdio server (a real subprocess, not a mock in-process object), plus the adversarial
//! server behaviours the trust model requires rejecting rather than tolerating.

use discovery::{DiscoveryClient, DiscoveryError};

fn fake_server_path() -> &'static str {
    env!("CARGO_BIN_EXE_fake-mcp-stdio-server")
}

#[test]
fn discover_succeeds_against_a_real_stdio_server() {
    let mut client = DiscoveryClient::stdio(fake_server_path(), &["happy"]).expect("spawn");
    let discovery = client.discover().expect("discover must succeed");

    assert_eq!(discovery.negotiated_spec_revision, "2025-11-25");

    let initialize: serde_json::Value =
        serde_json::from_slice(&discovery.initialize_raw).expect("valid JSON");
    assert_eq!(initialize["result"]["serverInfo"]["name"], "fake-mcp-stdio-server");

    let tools: serde_json::Value =
        serde_json::from_slice(&discovery.tools_list_raw).expect("valid JSON");
    assert_eq!(tools["result"]["tools"][0]["name"], "read_file");
}

#[test]
fn discover_surfaces_a_json_rpc_error_from_the_server() {
    let mut client = DiscoveryClient::stdio(fake_server_path(), &["error"]).expect("spawn");
    let err = client.discover().expect_err("initialize was rejected");
    match err {
        DiscoveryError::ServerError { code, .. } => assert_eq!(code, -32000),
        other => panic!("expected ServerError, got {other:?}"),
    }
}

#[test]
fn discover_rejects_a_mismatched_response_id() {
    let mut client = DiscoveryClient::stdio(fake_server_path(), &["wrong_id"]).expect("spawn");
    let err = client.discover().expect_err("id mismatch must not be tolerated");
    assert!(matches!(err, DiscoveryError::Protocol(_)));
}

#[test]
fn discover_rejects_malformed_json_from_the_server() {
    let mut client = DiscoveryClient::stdio(fake_server_path(), &["malformed"]).expect("spawn");
    let err = client.discover().expect_err("malformed JSON must not be tolerated");
    assert!(matches!(err, DiscoveryError::Protocol(_)));
}
