//! P0-01 exit criterion, stdio half: discovery succeeds against a real stdio server (a real
//! subprocess, not a mock in-process object), plus the adversarial server behaviours the
//! trust model requires rejecting rather than tolerating.
//!
//! P0-11 adds the era-negotiation half: modern-only, legacy-only and dual-era servers, and
//! the three `2026-07-28` error codes whose handling the spec prescribes and P0-09 got wrong.

use discovery::{DiscoveryClient, DiscoveryError, DiscoveryPath, FallbackReason, RevisionSource};

fn fake_server_path() -> &'static str {
    env!("CARGO_BIN_EXE_fake-mcp-stdio-server")
}

/// A legacy-only server: `server/discover` is just an unknown method to it, so modern-first
/// probes, gets `-32601`, and falls back. The provenance records that it did and why.
#[test]
fn discover_succeeds_against_a_real_stdio_server() {
    let mut client = DiscoveryClient::stdio(fake_server_path(), &["happy"]).expect("spawn");
    let discovery = client.discover().expect("discover must succeed");

    assert_eq!(discovery.negotiated_spec_revision.as_deref(), Some("2025-11-25"));
    assert_eq!(
        discovery.discovery_path,
        DiscoveryPath::Initialize,
        "a legacy-only server must be recorded as the legacy path, not left implicit"
    );
    assert_eq!(
        discovery.era_provenance.fallback_reason,
        Some(FallbackReason::NonModernErrorBody),
        "the downgrade must be recorded with its reason, so a server cannot shift the \
         published era distribution without that showing up"
    );
    assert_eq!(discovery.era_provenance.policy, "modern_first");
    assert_eq!(discovery.era_provenance.chosen_revision, "2025-11-25");
    assert!(discovery.era_provenance.offered_revisions.is_empty());

    let handshake: serde_json::Value =
        serde_json::from_slice(&discovery.handshake_raw).expect("valid JSON");
    assert_eq!(handshake["result"]["serverInfo"]["name"], "fake-mcp-stdio-server");

    let tools: serde_json::Value =
        serde_json::from_slice(&discovery.tools_list_raw).expect("valid JSON");
    assert_eq!(tools["result"]["tools"][0]["name"], "read_file");
}

/// P0-11's central case: a server that speaks only `2026-07-28`. It has no `initialize` at
/// all (the fake answers that method `-32601`), so this can only pass if the client probes
/// `server/discover` first, sends the required `_meta` block — the fake answers `-32021` if
/// it is missing or wrong — and reads `supportedVersions` rather than a `protocolVersion`
/// that does not exist in a `DiscoverResult`.
#[test]
fn discover_succeeds_against_a_modern_only_stdio_server() {
    let mut client = DiscoveryClient::stdio(fake_server_path(), &["modern"]).expect("spawn");
    let discovery = client.discover().expect("a 2026-07-28 server must be discoverable");

    assert_eq!(discovery.discovery_path, DiscoveryPath::ServerDiscover);
    assert_eq!(discovery.negotiated_spec_revision.as_deref(), Some("2026-07-28"));
    assert_eq!(discovery.era_provenance.fallback_reason, None);
    assert_eq!(discovery.era_provenance.offered_revisions, vec!["2026-07-28".to_string()]);
    assert_eq!(discovery.era_provenance.chosen_revision, "2026-07-28");

    let handshake: serde_json::Value =
        serde_json::from_slice(&discovery.handshake_raw).expect("valid JSON");
    assert_eq!(handshake["result"]["supportedVersions"][0], "2026-07-28");
    assert!(
        handshake["result"].get("protocolVersion").is_none(),
        "a DiscoverResult has no protocolVersion — P0-09's parser required one and so could \
         never succeed here"
    );

    let tools: serde_json::Value =
        serde_json::from_slice(&discovery.tools_list_raw).expect("valid JSON");
    assert_eq!(tools["result"]["tools"][0]["name"], "read_file");
    assert_eq!(
        tools["result"]["resultType"], "complete",
        "the modern tools/list result shape must have been accepted, extra fields and all"
    );
}

/// A dual-era server answers both handshakes. Under modern-first it must be recorded as
/// modern — which is exactly why [`discovery::era::ERA_POLICY`] travels on the record: the
/// same server would have been recorded `initialize` under P0-09's policy.
#[test]
fn a_dual_era_stdio_server_is_recorded_as_modern_under_modern_first() {
    let mut client = DiscoveryClient::stdio(fake_server_path(), &["dual"]).expect("spawn");
    let discovery = client.discover().expect("discover must succeed");

    assert_eq!(discovery.discovery_path, DiscoveryPath::ServerDiscover);
    assert_eq!(discovery.negotiated_spec_revision.as_deref(), Some("2026-07-28"));
    assert_eq!(discovery.era_provenance.fallback_reason, None);
}

/// A `DiscoverResult` that offers only a pre-`2026-07-28` revision: the client must use the
/// legacy handshake at the revision the server named, and say so.
#[test]
fn a_discover_result_offering_only_legacy_revisions_uses_the_legacy_handshake() {
    let mut client =
        DiscoveryClient::stdio(fake_server_path(), &["legacy_only_versions"]).expect("spawn");
    let discovery = client.discover().expect("discover must succeed via the legacy handshake");

    assert_eq!(discovery.discovery_path, DiscoveryPath::Initialize);
    assert_eq!(
        discovery.era_provenance.fallback_reason,
        Some(FallbackReason::OnlyLegacyRevisionsOffered),
        "a legacy-only `supportedVersions` list gets its own reason code, distinct from a \
         -32022 that offers only legacy revisions — two different server behaviours"
    );
    assert_eq!(discovery.era_provenance.offered_revisions, vec!["2025-11-25".to_string()]);
    assert_eq!(discovery.negotiated_spec_revision.as_deref(), Some("2025-11-25"));
    assert_eq!(discovery.era_provenance.revision_source, RevisionSource::Negotiated);
}

/// `-32022 UnsupportedProtocolVersionError` naming a legacy revision: re-negotiate down to
/// that revision through the legacy handshake, recording the server's offer.
#[test]
fn an_unsupported_version_error_renegotiates_to_the_revision_the_server_offered() {
    let mut client =
        DiscoveryClient::stdio(fake_server_path(), &["unsupported_version"]).expect("spawn");
    let discovery = client.discover().expect("discover must succeed after re-negotiation");

    assert_eq!(discovery.discovery_path, DiscoveryPath::Initialize);
    assert_eq!(
        discovery.era_provenance.fallback_reason,
        Some(FallbackReason::OnlyLegacyRevisionsAfterUnsupportedVersion),
        "the -32022 route is its own reason code — the server *rejected* the modern probe \
         rather than advertising a legacy-only version list"
    );
    assert_eq!(discovery.era_provenance.offered_revisions, vec!["2025-11-25".to_string()]);
}

/// `-32022` offering nothing this client implements is a **discovery failure, not a
/// downgrade**. The fake answers `initialize` successfully on purpose: a client that
/// downgraded anyway would succeed here, and this assertion would catch it.
#[test]
fn an_unsupported_version_error_offering_nothing_implemented_fails_rather_than_downgrading() {
    let mut client =
        DiscoveryClient::stdio(fake_server_path(), &["unsupported_version_unknown"])
            .expect("spawn");
    let err = client.discover().expect_err("an empty version intersection must not succeed");
    match err {
        DiscoveryError::Protocol(msg) => {
            assert!(msg.contains("version negotiation failed"), "got: {msg}");
            assert!(msg.contains("2031-01-01"), "must name what the server offered: {msg}");
        }
        other => panic!("expected a Protocol negotiation failure, got {other:?}"),
    }
}

/// `-32020 HeaderMismatch` means the server is modern and the *request* was wrong — a
/// harness bug or an unmet server requirement. It must surface, not provoke a silent
/// downgrade that would hide exactly the class of defect P0-11 exists to fix. As above, the
/// fake would let a downgrading client succeed.
#[test]
fn a_modern_header_mismatch_error_surfaces_instead_of_downgrading() {
    let mut client =
        DiscoveryClient::stdio(fake_server_path(), &["header_mismatch"]).expect("spawn");
    let err = client.discover().expect_err("a modern-era request error must not be masked");
    match err {
        DiscoveryError::ServerError { code, .. } => assert_eq!(code, -32020),
        other => panic!("expected the -32020 to surface untouched, got {other:?}"),
    }
}

/// **The stdio regression modern-first introduced, fixed.** A legacy server that *exits*
/// rather than answering an unknown method had its channel destroyed by the probe: the
/// `initialize` fallback then wrote to a closed stdin, got `Broken pipe`, and the server
/// could not be discovered at all — where initialize-first discovered it fine. The fallback
/// re-spawns the child when the probe produced no bytes whatsoever, which is exactly this
/// case and the watchdog-killed-silent-server case, and nothing else.
#[test]
fn a_stdio_server_that_exits_on_an_unknown_method_is_still_discovered() {
    let mut client =
        DiscoveryClient::stdio(fake_server_path(), &["exit_on_unknown"]).expect("spawn");
    let discovery = client
        .discover()
        .expect("a server that exits on the probe must still reach the legacy handshake");

    assert_eq!(discovery.discovery_path, DiscoveryPath::Initialize);
    assert_eq!(discovery.negotiated_spec_revision.as_deref(), Some("2025-11-25"));
    assert_eq!(
        discovery.era_provenance.fallback_reason,
        Some(FallbackReason::NoResponse),
        "no bytes came back from the probe, which is the gate the re-spawn is keyed on"
    );
    assert_eq!(
        discovery.probe_raw,
        discovery::ProbeEvidence::Absent(discovery::ProbeAbsence::NoResponse),
        "and the absence is recorded, not left as an absent field"
    );

    let tools: serde_json::Value =
        serde_json::from_slice(&discovery.tools_list_raw).expect("valid JSON");
    assert_eq!(tools["result"]["tools"][0]["name"], "read_file");
}

/// The re-spawn must be narrow: a legacy server that *answers* the probe has a live channel,
/// and re-spawning it would double the cost of every legacy stdio server in a sweep (and,
/// under Stage 2, re-launch a container that is still up). Asserted through the request log
/// the fake keeps by construction — a single process serving the whole exchange, with
/// `tools/list`'s id being `2` rather than `0`, is only possible if no re-spawn happened
/// (a fresh child's id counter restarts at zero).
#[test]
fn a_stdio_server_that_answers_the_probe_is_not_respawned_for_the_fallback() {
    let mut client = DiscoveryClient::stdio(fake_server_path(), &["happy"]).expect("spawn");
    let discovery = client.discover().expect("discover must succeed");

    assert_eq!(discovery.discovery_path, DiscoveryPath::Initialize);
    let handshake: serde_json::Value =
        serde_json::from_slice(&discovery.handshake_raw).expect("valid JSON");
    assert_eq!(
        handshake["id"], 1,
        "initialize must be request 1 on the same transport the probe used (request 0); a \
         re-spawned child would have restarted its id counter at 0"
    );
    let tools: serde_json::Value =
        serde_json::from_slice(&discovery.tools_list_raw).expect("valid JSON");
    assert_eq!(tools["id"], 2, "and tools/list must be request 2 on that same transport");
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
    assert!(
        matches!(err, DiscoveryError::Io(_)),
        "expected the watchdog's kill to surface as Io, got {err:?}"
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(10),
        "watchdog should have unblocked discover() within a few seconds of the 2s timeout, took {:?}",
        start.elapsed()
    );
}
