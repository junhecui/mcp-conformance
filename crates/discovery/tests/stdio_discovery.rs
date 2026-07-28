//! P0-01 exit criterion, stdio half: `initialize` + `tools/list` succeeds against a real
//! stdio server (a real subprocess, not a mock in-process object), plus the adversarial
//! server behaviours the trust model requires rejecting rather than tolerating.

use discovery::{DiscoveryClient, DiscoveryError, DiscoveryPath};

fn fake_server_path() -> &'static str {
    env!("CARGO_BIN_EXE_fake-mcp-stdio-server")
}

#[test]
fn discover_succeeds_against_a_real_stdio_server() {
    let mut client = DiscoveryClient::stdio(fake_server_path(), &["happy"]).expect("spawn");
    let discovery = client.discover().expect("discover must succeed");

    assert_eq!(discovery.negotiated_spec_revision, "2025-11-25");
    assert_eq!(
        discovery.discovery_path,
        DiscoveryPath::Initialize,
        "the ordinary handshake must be recorded as its own provenance value, not left implicit"
    );

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

/// Stage 2 census's reason for `stdio_with_timeout` existing at all: a server that never
/// responds must not hang the caller forever. Asserts on wall-clock time, not just the
/// error variant, so a regression that silently drops the watchdog thread (and falls back
/// to blocking forever) would hang this test rather than pass it — a stronger failure
/// signal for CI than a false green.
#[test]
fn discover_is_unblocked_by_the_watchdog_when_the_server_never_responds() {
    let start = std::time::Instant::now();
    let mut client = DiscoveryClient::stdio_with_timeout(
        fake_server_path(),
        &["hang"],
        std::time::Duration::from_secs(2),
    )
    .expect("spawn");
    let err = client.discover().expect_err("a hung server must not yield a successful discovery");
    assert!(matches!(err, DiscoveryError::Io(_)), "expected the watchdog's kill to surface as Io, got {err:?}");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(10),
        "watchdog should have unblocked discover() within a few seconds of the 2s timeout, took {:?}",
        start.elapsed()
    );
}

/// P0-09's exit criterion: a server that answers `initialize` with a JSON-RPC `-32601
/// Method not found` error (the shape a server that has moved to spec `2026-07-28`'s
/// handshake-free lifecycle is expected to produce) is not a bare discovery failure —
/// `discover()` retries with `server/discover` and succeeds via that path.
#[test]
fn discover_falls_back_to_server_discover_when_initialize_is_unrecognized() {
    let mut client =
        DiscoveryClient::stdio(fake_server_path(), &["server_discover"]).expect("spawn");
    let discovery = client.discover().expect("discover must succeed via the fallback path");

    assert_eq!(
        discovery.discovery_path,
        DiscoveryPath::ServerDiscover,
        "must record that server/discover, not initialize, produced this result"
    );
    assert_eq!(
        discovery.negotiated_spec_revision, "2026-07-28",
        "spec_revision must reflect what the server actually negotiated, regardless of \
         which handshake produced it"
    );

    let handshake: serde_json::Value =
        serde_json::from_slice(&discovery.initialize_raw).expect("valid JSON");
    assert_eq!(handshake["result"]["protocolVersion"], "2026-07-28");

    let tools: serde_json::Value =
        serde_json::from_slice(&discovery.tools_list_raw).expect("valid JSON");
    assert_eq!(tools["result"]["tools"][0]["name"], "read_file");
}

/// The fallback must be narrow: an `initialize` rejection for a reason *other* than
/// "unrecognized method" (here, the existing `error` mode's `-32000`) is a real discovery
/// failure and must not silently retry via `server/discover` — see
/// `is_initialize_unavailable`'s doc comment in `client.rs` for why.
#[test]
fn discover_does_not_fall_back_on_a_non_method_not_found_server_error() {
    let mut client = DiscoveryClient::stdio(fake_server_path(), &["error"]).expect("spawn");
    let err = client.discover().expect_err("a non-(-32601) server error must not be masked");
    match err {
        DiscoveryError::ServerError { code, .. } => assert_eq!(code, -32000),
        other => panic!("expected the original ServerError to surface untouched, got {other:?}"),
    }
}
