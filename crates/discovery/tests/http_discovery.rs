//! P0-01 exit criterion, HTTP half: discovery succeeds against a real remote HTTP server.
//!
//! P0-11 adds the era-negotiation and HTTP-status halves. The fake server here is a minimal
//! hand-rolled HTTP/1.1 responder over `std::net::TcpListener` — enough to read one
//! `Content-Length`-framed request and write one scripted response, which is all the
//! Streamable HTTP single-JSON-response case needs. No new dependency for a few dozen lines
//! that exist only to drive a test.
//!
//! Every test here scripts an exact number of responses and then asserts on the exact
//! number of requests received. That is deliberate: under modern-first the *number* of round
//! trips a server costs is part of the contract (a 429 or a connection timeout must cost
//! one, a legacy fallback costs four), and P0-09's own regression was a doubled request
//! count that no assertion would have caught.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use discovery::{DiscoveryClient, DiscoveryError, DiscoveryPath, FallbackReason, RevisionSource};

/// One request as the fake server received it.
struct Recorded {
    headers: String,
    body: Vec<u8>,
}

impl Recorded {
    /// Does the request carry this header line (case-insensitively, value included)?
    fn has_header(&self, line: &str) -> bool {
        self.headers.to_ascii_lowercase().contains(&line.to_ascii_lowercase())
    }

    fn has_header_name(&self, name: &str) -> bool {
        self.headers
            .to_ascii_lowercase()
            .lines()
            .any(|l| l.starts_with(&format!("{}:", name.to_ascii_lowercase())))
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("the client always sends valid JSON")
    }

    fn method(&self) -> String {
        self.json()["method"].as_str().unwrap_or_default().to_string()
    }
}

/// One scripted response: status, `Content-Type`, body, and extra header lines.
struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    extra_headers: &'static str,
}

impl Reply {
    fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.into(),
            extra_headers: "",
        }
    }

    fn with_headers(mut self, extra: &'static str) -> Self {
        self.extra_headers = extra;
        self
    }

    fn content_type(mut self, ct: &'static str) -> Self {
        self.content_type = ct;
        self
    }
}

fn read_one_http_request(stream: &mut TcpStream) -> Recorded {
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
    Recorded { headers, body }
}

fn write_reply(stream: &mut TcpStream, reply: &Reply) {
    let reason = match reply.status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n",
        reply.status,
        reason,
        reply.content_type,
        reply.body.len(),
        reply.extra_headers,
    );
    stream.write_all(header.as_bytes()).expect("write header");
    stream.write_all(&reply.body).expect("write body");
    stream.flush().expect("flush");
}

/// Serve `script` in order, one request per connection, and hand back what was received.
///
/// Serves exactly `script.len()` requests and then returns, so a test that scripts N
/// responses and joins the handle is also asserting the client sent no more than N requests.
fn serve(listener: TcpListener, script: Vec<Reply>) -> thread::JoinHandle<Vec<Recorded>> {
    thread::spawn(move || {
        let mut received = Vec::new();
        for reply in &script {
            let (mut stream, _) = listener.accept().expect("accept a scripted request");
            received.push(read_one_http_request(&mut stream));
            write_reply(&mut stream, reply);
        }
        received
    })
}

fn bind() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!("http://{}/mcp", listener.local_addr().expect("local addr"));
    (listener, endpoint)
}

const DISCOVER_RESULT: &str = r#"{"jsonrpc":"2.0","id":0,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":true}},"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"fake-mcp-http-server","version":"0.0.0"}}}}"#;

/// Appendix A.4, verbatim: DeepWiki's HTTP 400 rejection of a correctly-shaped modern probe.
/// Note the substituted string id, which `decode_and_validate` must (and does) refuse — the
/// reason the era classifier is a separate function.
const DEEPWIKI_400: &str = r#"{"jsonrpc":"2.0","id":"server-error","error":{"code":-32600,"message":"Bad Request: Unsupported protocol version: 2026-07-28. Supported versions: 2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25"}}"#;

/// Appendix A.5, verbatim: GitMCP's HTTP 400, with `"id": null`.
const GITMCP_400: &str = r#"{"jsonrpc":"2.0","error":{"code":-32000,"message":"Bad Request: Mcp-Session-Id header is required"},"id":null}"#;

fn legacy_initialize_result(id: u64) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-11-25","capabilities":{{}},"serverInfo":{{"name":"fake-mcp-http-server","version":"0.0.0"}}}}}}"#
    )
}

fn tools_list_result(id: u64, modern: bool) -> String {
    let extra = if modern { r#""resultType":"complete","ttlMs":0,"cacheScope":"private","# } else { "" };
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"result":{{{extra}"tools":[{{"name":"read_file","description":"Reads a file","inputSchema":{{"type":"object"}},"annotations":{{"readOnlyHint":true}}}}]}}}}"#
    )
}

/// A modern-only server: `server/discover` returns a `DiscoverResult` and `tools/list`
/// follows on the modern path. Asserts every requirement §1.3 places on a `2026-07-28`
/// request — `_meta` with the two required members on *both* requests (not just the
/// handshake), `MCP-Protocol-Version` on both including the very first, `Mcp-Method` equal to
/// the body's method, and no `Mcp-Name` (which is required only for `tools/call`,
/// `resources/read` and `prompts/get`, none of which this crate may ever send).
#[test]
fn a_modern_only_http_server_is_discovered_through_server_discover() {
    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![
            Reply::json(200, DISCOVER_RESULT),
            Reply::json(200, tools_list_result(1, true)),
        ],
    );

    let mut client = DiscoveryClient::http(endpoint);
    let discovery = client.discover().expect("a 2026-07-28 server must be discoverable");
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(requests.len(), 2, "modern-first costs one probe plus one tools/list");
    assert_eq!(requests[0].method(), "server/discover");
    assert_eq!(requests[1].method(), "tools/list");

    for (i, request) in requests.iter().enumerate() {
        let params = &request.json()["params"];
        assert_eq!(
            params.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()),
            Some(vec!["_meta".to_string()]),
            "request {i} params must be exactly {{ _meta }}: {}",
            String::from_utf8_lossy(&request.body)
        );
        assert_eq!(params["_meta"]["io.modelcontextprotocol/protocolVersion"], "2026-07-28");
        assert!(params["_meta"]["io.modelcontextprotocol/clientCapabilities"].is_object());
        assert!(
            request.has_header("mcp-protocol-version: 2026-07-28"),
            "request {i} must carry MCP-Protocol-Version matching its _meta: {}",
            request.headers
        );
        assert!(!request.has_header_name("mcp-name"), "Mcp-Name is never sent by discovery");
    }
    assert!(requests[0].has_header("mcp-method: server/discover"), "{}", requests[0].headers);
    assert!(requests[1].has_header("mcp-method: tools/list"), "{}", requests[1].headers);

    assert_eq!(discovery.discovery_path, DiscoveryPath::ServerDiscover);
    assert_eq!(discovery.negotiated_spec_revision.as_deref(), Some("2026-07-28"));
    assert_eq!(discovery.era_provenance.fallback_reason, None);
    assert_eq!(discovery.era_provenance.offered_revisions, vec!["2026-07-28".to_string()]);

    let tools: serde_json::Value =
        serde_json::from_slice(&discovery.tools_list_raw).expect("valid JSON");
    assert_eq!(tools["result"]["tools"][0]["name"], "read_file");
}

/// A **real** `DiscoverResult`, replayed byte for byte: the response
/// `https://docs.mcp.cloudflare.com/mcp` returned on 2026-10-08T05:47:13Z to exactly the
/// request this client now sends (the full transcript is summarised in `docs/tasks.md` under
/// P0-11). P0-09's failure was that its wire shape was extrapolated and never checked
/// against a server, so the fix carries a fixture that came off the wire rather than out of
/// a schema example.
///
/// This body is used in preference to the two other modern-capable hosts probed because it
/// is the one of the three carrying no `instructions` field. That field is server-authored
/// prose written imperatively at a language model, and this repository's rule is to record
/// such text as evidence rather than circulate it — a test fixture is not the place for it.
/// The other two responses are described in `docs/tasks.md` without quoting their prose.
#[test]
fn a_real_cloudflare_discover_result_is_accepted_byte_for_byte() {
    // Verbatim, including the server echoing this client's integer id rather than the
    // string id the spec example uses.
    const LIVE: &str = r#"{"result":{"supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":true},"prompts":{"listChanged":true}},"resultType":"complete","ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"docs-ai-search","version":"0.4.13"}}},"jsonrpc":"2.0","id":0}"#;

    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![Reply::json(200, LIVE), Reply::json(200, tools_list_result(1, true))],
    );

    let mut client = DiscoveryClient::http(endpoint);
    let discovery = client.discover().expect("a real DiscoverResult must be accepted");
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(requests.len(), 2);
    assert_eq!(discovery.discovery_path, DiscoveryPath::ServerDiscover);
    assert_eq!(discovery.negotiated_spec_revision.as_deref(), Some("2026-07-28"));
    assert_eq!(discovery.era_provenance.offered_revisions, vec!["2026-07-28".to_string()]);
    assert_eq!(discovery.era_provenance.fallback_reason, None);
    assert_eq!(discovery.handshake_raw, LIVE.as_bytes(), "evidence is the bytes received");
}

/// A **real** non-modern 400, replayed byte for byte: what `https://gitmcp.io/docs` returned
/// at 2026-10-08T05:47:19Z — and it arrived with `Content-Type: text/plain;charset=UTF-8`,
/// not `application/json`. The era decision must come from the body, so a legacy server that
/// mislabels its JSON-RPC error still gets classified and still gets the fallback.
#[test]
fn a_real_gitmcp_400_with_a_text_plain_content_type_still_falls_back() {
    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![
            Reply::json(400, GITMCP_400).content_type("text/plain;charset=UTF-8"),
            Reply::json(200, legacy_initialize_result(1)),
            Reply::json(200, "{}"),
            Reply::json(200, tools_list_result(2, false)),
        ],
    );

    let mut client = DiscoveryClient::http(endpoint);
    let discovery = client.discover().expect("the fallback must still happen");
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(requests.len(), 4);
    assert_eq!(discovery.discovery_path, DiscoveryPath::Initialize);
    assert_eq!(
        discovery.era_provenance.fallback_reason,
        Some(FallbackReason::NonModernErrorBody)
    );
}

/// **The case P0-09 structurally could not reach.** A legacy-only HTTP server rejects the
/// modern probe with HTTP 400 and a non-modern error body; the client must read that body,
/// classify it, and fall back to `initialize`. Under P0-09 the 400 became
/// `DiscoveryError::Transport` with the body discarded, and `is_initialize_unavailable`
/// excluded `Transport`, so the fallback never fired at all.
///
/// Both real captured bodies from the re-survey's Appendix A are exercised, because the
/// point is that the decision is made on the *body* and not on HTTP 400 alone.
#[test]
fn a_legacy_only_http_server_falls_back_after_a_non_modern_400_body() {
    for (label, probe_body) in [("deepwiki -32600", DEEPWIKI_400), ("gitmcp -32000", GITMCP_400)] {
        let (listener, endpoint) = bind();
        let server = serve(
            listener,
            vec![
                Reply::json(400, probe_body),
                Reply::json(200, legacy_initialize_result(1)),
                Reply::json(200, "{}"),
                Reply::json(200, tools_list_result(2, false)),
            ],
        );

        let mut client = DiscoveryClient::http(endpoint);
        let discovery = client.discover().unwrap_or_else(|e| panic!("{label}: {e}"));
        let requests = server.join().expect("fake server thread must not panic");

        assert_eq!(requests.len(), 4, "{label}: probe, initialize, initialized, tools/list");
        assert_eq!(requests[0].method(), "server/discover");
        assert_eq!(requests[1].method(), "initialize");
        assert_eq!(requests[2].method(), "notifications/initialized");
        assert_eq!(requests[3].method(), "tools/list");

        // The legacy request must not announce a revision the server just refused, and
        // `Mcp-Method` is undefined before 2026-07-28.
        assert!(
            !requests[1].has_header_name("mcp-protocol-version"),
            "{label}: the modern probe's version header must be cleared before initialize: {}",
            requests[1].headers
        );
        assert!(!requests[1].has_header_name("mcp-method"), "{label}: {}", requests[1].headers);
        assert!(requests[1].json()["params"]["protocolVersion"] == "2025-11-25");
        // After initialize, the negotiated revision rides every later request, as before.
        assert!(
            requests[3].has_header("mcp-protocol-version: 2025-11-25"),
            "{label}: {}",
            requests[3].headers
        );

        assert_eq!(discovery.discovery_path, DiscoveryPath::Initialize, "{label}");
        assert_eq!(discovery.negotiated_spec_revision.as_deref(), Some("2025-11-25"), "{label}");
        assert_eq!(
            discovery.era_provenance.fallback_reason,
            Some(FallbackReason::NonModernErrorBody),
            "{label}"
        );
    }
}

/// **The era classification's preimage must be keepable.** On every fallback the probe
/// response used to be dropped, which left `fallback_reason` an unauditable client assertion
/// — the one derived field in the system with nothing to replay it against
/// (architecture.md §6 invariant 2).
///
/// It is not only about hiding modern capability. A dual-era server's modern and legacy
/// handlers may expose *different tool sets with different annotations*, so answering the
/// probe with garbage steers the harness onto the handler the server picked; P0-02's pin
/// keeps the verdict honest about the snapshot tested, but nothing in the evidence showed
/// that a selection had happened.
#[test]
fn the_probe_response_that_caused_a_fallback_is_kept_as_evidence() {
    // A marker a reader can grep for: if these bytes are reachable from the result, the
    // classification has a preimage.
    const PROBE: &str = r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32600,"message":"probe-evidence-marker"}}"#;

    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![
            Reply::json(400, PROBE),
            Reply::json(200, legacy_initialize_result(1)),
            Reply::json(200, "{}"),
            Reply::json(200, tools_list_result(2, false)),
        ],
    );

    let mut client = DiscoveryClient::http(endpoint);
    let discovery = client.discover().expect("the fallback must succeed");
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(requests.len(), 4);
    assert_eq!(discovery.discovery_path, DiscoveryPath::Initialize);
    assert_eq!(
        discovery.probe_raw,
        discovery::ProbeEvidence::Captured(PROBE.as_bytes().to_vec()),
        "the bytes the fallback decision was made on must survive the fallback"
    );
    assert!(
        !String::from_utf8_lossy(&discovery.handshake_raw).contains("probe-evidence-marker"),
        "and they are genuinely not in handshake_raw, which is why a third blob is needed"
    );
}

/// The other half of the same contract: where no bytes ever existed, that is *recorded*, so
/// "no evidence" is distinguishable from "evidence not kept". An SSE probe reply is rejected
/// before its body is read, so the bytes existed on the wire and this client does not have
/// them — `body_not_read`, not `no_response`.
#[test]
fn a_probe_whose_body_was_never_read_records_that_rather_than_claiming_no_response() {
    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![
            Reply::json(200, "event: message\ndata: {}\n\n").content_type("text/event-stream"),
            Reply::json(200, legacy_initialize_result(1)),
            Reply::json(200, "{}"),
            Reply::json(200, tools_list_result(2, false)),
        ],
    );

    let mut client = DiscoveryClient::http(endpoint);
    let discovery = client.discover().expect("an SSE probe reply falls back, as before");
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(requests.len(), 4);
    assert_eq!(
        discovery.probe_raw,
        discovery::ProbeEvidence::Absent(discovery::ProbeAbsence::BodyNotRead),
        "the absence must name its reason, not be an absent field"
    );
    assert_eq!(discovery.probe_raw.state(), "body_not_read");
    assert_eq!(
        discovery.era_provenance.fallback_reason,
        Some(FallbackReason::MalformedResponse)
    );
}

/// A dual-era server answers both handshakes; modern-first must take the modern one and
/// never send `initialize` at all. Asserted by the request log, not just by the recorded
/// path — the same server would be recorded `initialize` under P0-09's policy, which is why
/// `era_provenance.policy` exists.
#[test]
fn a_dual_era_http_server_is_recorded_as_modern_and_never_sent_initialize() {
    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![Reply::json(200, DISCOVER_RESULT), Reply::json(200, tools_list_result(1, true))],
    );

    let mut client = DiscoveryClient::http(endpoint);
    let discovery = client.discover().expect("discover must succeed");
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(requests.len(), 2);
    assert!(
        requests.iter().all(|r| r.method() != "initialize"),
        "a dual-era server must not be asked for the legacy handshake under modern-first"
    );
    assert_eq!(discovery.discovery_path, DiscoveryPath::ServerDiscover);
    assert_eq!(discovery.era_provenance.policy, "modern_first");
}

/// A server that 400s *everything*, including the legacy `initialize` — the shape a
/// modern-only HTTP server presents to a legacy client, per the spec's compatibility matrix.
/// The probe's 400 carries an empty body (a proxy, or a server that says nothing), which
/// classifies as non-modern and triggers the fallback; the fallback's own 400 must then
/// surface as `HttpStatus { status: 400 }`.
///
/// This is the hazard-2 assertion in its sharpest form: before P0-11 this was
/// `Transport("http status: 400")`, and a naive `http_status_as_error(false)` would have
/// turned it into `Protocol(..)` by handing an empty body to `decode_and_validate`. It is
/// neither — the status is captured explicitly and the failure stays keyed on it.
#[test]
fn an_http_400_on_the_legacy_fallback_surfaces_as_a_status_failure_not_a_protocol_error() {
    let (listener, endpoint) = bind();
    let server =
        serve(listener, vec![Reply::json(400, ""), Reply::json(400, "")]);

    let mut client = DiscoveryClient::http(endpoint);
    let err = client.discover().expect_err("a 400 on initialize must fail discovery");
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(requests.len(), 2, "one modern probe, one legacy fallback, no more");
    match err {
        DiscoveryError::HttpStatus { status, .. } => assert_eq!(status, 400),
        other => panic!("expected HttpStatus {{ status: 400 }}, got {other:?}"),
    }
}

/// Hazard 2, the comparability requirement, on the request that matters most: `tools/list`.
///
/// `http_status_as_error(false)` is agent-wide, so a 500's HTML body would now reach
/// `decode_and_validate` and surface as `Protocol(..)`, silently moving HTTP-level failures
/// into the `protocol` bucket and breaking comparability with the July census split (435
/// `transport` against 312 `protocol`). It must stay a status-keyed failure, and its
/// rendering must stay byte-identical to what `ureq::Error::StatusCode` produced, because
/// that string is what the July results files contain.
#[test]
fn an_http_500_on_tools_list_stays_a_status_failure_with_the_july_detail_string() {
    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![
            Reply::json(400, DEEPWIKI_400),
            Reply::json(200, legacy_initialize_result(1)),
            Reply::json(200, "{}"),
            Reply::json(500, "<html><head><title>500</title></head><body>oops</body></html>")
                .content_type("text/html"),
        ],
    );

    let mut client = DiscoveryClient::http(endpoint);
    let err = client.discover().expect_err("a 500 on tools/list must fail discovery");
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(requests.len(), 4);
    match &err {
        DiscoveryError::HttpStatus { status, .. } => assert_eq!(*status, 500),
        other => panic!(
            "a 500 must stay an HTTP-status failure, not become a Protocol error from its \
             HTML body: got {other:?}"
        ),
    }
    assert_eq!(
        err.to_string(),
        "http status: 500",
        "the failure detail census records must not drift from what July recorded"
    );
}

/// A 429 is its own failure category — July's Class B results hold two `http status: 429`
/// responses indistinguishable from dead hosts, so self-inflicted throttling was
/// unmeasurable. It must also cost exactly one request: no fallback, no retry inside the
/// sweep. `Retry-After` is captured so a sweep can skip the host rather than retry it.
#[test]
fn an_http_429_is_captured_with_retry_after_and_never_triggers_a_fallback() {
    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![Reply::json(429, r#"{"error":"slow down"}"#).with_headers("Retry-After: 120\r\n")],
    );

    let mut client = DiscoveryClient::http(endpoint);
    let err = client.discover().expect_err("a 429 must fail discovery");
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(
        requests.len(),
        1,
        "a throttled host must cost exactly one request — retrying it inside a sweep is how \
         the O-01 probe lost a transcript"
    );
    match err {
        DiscoveryError::HttpStatus { status, retry_after } => {
            assert_eq!(status, 429);
            assert_eq!(retry_after.as_deref(), Some("120"));
        }
        other => panic!("expected HttpStatus {{ status: 429 }}, got {other:?}"),
    }
}

/// HTTP 404 plus `-32601` is `2026-07-28`'s unknown-method framing, so the client stays
/// modern and goes straight to `tools/list` with the modern `_meta` rather than downgrading
/// to a handshake that revision deleted.
///
/// **The record must say the revision was *assumed*, not negotiated.** A server reached this
/// way named no revision at all; filling `negotiated_spec_revision` from a client-side
/// constant published a claim the server never made — and one that could not be contradicted
/// offline, because `negotiated_spec_revision()` *errors* on that same stored 404 body. The
/// honest shape is `None`, `RevisionSource::Assumed`, and the assumed value visible in
/// `chosen_revision`, which is also what makes a populated `chosen_revision` next to an
/// empty `offered_revisions` coherent rather than self-contradictory.
#[test]
fn a_404_method_not_found_keeps_the_modern_path_with_an_assumed_not_negotiated_revision() {
    const PROBE_404: &str =
        r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"Method not found"}}"#;

    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![Reply::json(404, PROBE_404), Reply::json(200, tools_list_result(1, true))],
    );

    let mut client = DiscoveryClient::http(endpoint);
    let discovery = client.discover().expect("the modern path must continue past a 404/-32601");
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].method(), "tools/list");
    assert!(requests[1].has_header("mcp-method: tools/list"), "{}", requests[1].headers);
    assert_eq!(
        requests[1].json()["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
        "2026-07-28"
    );
    assert_eq!(discovery.discovery_path, DiscoveryPath::ModernWithoutDiscover);
    assert_eq!(discovery.era_provenance.fallback_reason, None);

    assert_eq!(
        discovery.negotiated_spec_revision, None,
        "nothing was negotiated on this path, so nothing may be published as negotiated"
    );
    assert_eq!(discovery.era_provenance.revision_source, RevisionSource::Assumed);
    assert_eq!(discovery.era_provenance.chosen_revision, "2026-07-28");
    assert!(discovery.era_provenance.offered_revisions.is_empty());
    // The live record and an offline re-derivation from the same bytes must agree that the
    // handshake evidence carries no revision. Before, one said `2026-07-28` and the other
    // errored — the published claim could not be checked against its own evidence.
    assert!(
        discovery::negotiated_spec_revision(&discovery.handshake_raw).is_err(),
        "a 404 error body carries no revision; the live field must not claim otherwise"
    );
    assert_eq!(
        discovery.probe_raw,
        discovery::ProbeEvidence::Captured(PROBE_404.as_bytes().to_vec()),
        "the bytes that justified the era classification must be kept"
    );
}

/// **The commonest legacy shape over HTTP, and it was covered only over stdio.** Every other
/// HTTP fallback test here uses an HTTP 400; a legacy SDK server answering an unknown method
/// behind an HTTP **200** is what Cloudflare actually did to P0-09's payload (re-survey
/// Appendix A.1) and what most pre-`2026-07-28` servers do, since nothing before that
/// revision required a status mapping at all. Only a 404 makes `-32601` a modern signal, so
/// this must fall back — and that distinction had no HTTP-level test.
#[test]
fn an_http_200_method_not_found_probe_reply_falls_back_to_the_legacy_handshake() {
    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![
            Reply::json(
                200,
                r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"Method not found"}}"#,
            ),
            Reply::json(200, legacy_initialize_result(1)),
            Reply::json(200, "{}"),
            Reply::json(200, tools_list_result(2, false)),
        ],
    );

    let mut client = DiscoveryClient::http(endpoint);
    let discovery = client.discover().expect("a 200/-32601 server must still be discoverable");
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(requests.len(), 4, "probe, initialize, initialized, tools/list — no more");
    assert_eq!(requests[0].method(), "server/discover");
    assert_eq!(requests[1].method(), "initialize");
    assert_eq!(requests[2].method(), "notifications/initialized");
    assert_eq!(requests[3].method(), "tools/list");
    assert_eq!(discovery.discovery_path, DiscoveryPath::Initialize);
    assert_eq!(
        discovery.era_provenance.fallback_reason,
        Some(FallbackReason::NonModernErrorBody),
        "behind a 200, -32601 is a legacy dispatch and not 2026-07-28's unknown-method shape"
    );
    assert_eq!(discovery.negotiated_spec_revision.as_deref(), Some("2025-11-25"));
}

/// **The 404/`-32601` trap, closed.** Mapping JSON-RPC `-32601` onto HTTP 404 is a documented
/// JSON-RPC-over-HTTP convention, not a `2026-07-28` invention, so a *legacy* server can
/// present exactly the shape the modern classifier reads as modern. Such a server used to be
/// locked onto the modern path: it never got an `initialize` at all, and so failed discovery
/// where P0-09 discovered it. (Sizing from the committed July file: 60 of 1,000 Class B
/// servers returned `http status: 404`, so the exposure was 0–60 per 1,000 and
/// *undeterminable*, because that sweep predates evidence persistence.)
///
/// So an optimistic modern guess is correctable: when the `tools/list` that follows
/// `modern_without_discover` fails, discovery retries the legacy handshake and records why.
/// Only this path is correctable — a server that returned a `DiscoverResult` or a `-32022`
/// has *told* us it is modern, and downgrading it is what the spec forbids.
#[test]
fn a_legacy_server_whose_404_mimics_the_modern_unknown_method_shape_is_still_discovered() {
    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![
            // A legacy server using the plain `-32601` → 404 convention, envelope and all.
            Reply::json(
                404,
                r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"Method not found"}}"#,
            ),
            // ... which then rejects `tools/list` because no `initialize` ever happened.
            Reply::json(
                200,
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"not initialized"}}"#,
            ),
            Reply::json(200, legacy_initialize_result(2)),
            Reply::json(200, "{}"),
            Reply::json(200, tools_list_result(3, false)),
        ],
    );

    let mut client = DiscoveryClient::http(endpoint);
    let discovery =
        client.discover().expect("the optimistic modern guess must be corrected, not fatal");
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(
        requests.len(),
        5,
        "probe, the optimistic modern tools/list, then the full legacy handshake — and \
         nothing beyond it, since the correction is one shot and never a loop"
    );
    assert_eq!(requests[0].method(), "server/discover");
    assert_eq!(requests[1].method(), "tools/list");
    assert_eq!(requests[2].method(), "initialize");
    assert_eq!(requests[3].method(), "notifications/initialized");
    assert_eq!(requests[4].method(), "tools/list");
    // The legacy leg must not carry the modern headers the probe set.
    assert!(!requests[2].has_header_name("mcp-method"), "{}", requests[2].headers);
    assert!(!requests[2].has_header_name("mcp-protocol-version"), "{}", requests[2].headers);

    assert_eq!(discovery.discovery_path, DiscoveryPath::Initialize);
    assert_eq!(
        discovery.era_provenance.fallback_reason,
        Some(FallbackReason::ModernWithoutDiscoverToolsListFailed),
        "the correction gets its own reason code, so this population is countable in a census"
    );
    assert_eq!(discovery.negotiated_spec_revision.as_deref(), Some("2025-11-25"));
    assert_eq!(discovery.era_provenance.revision_source, RevisionSource::Negotiated);
}

/// The complement, so the test above cannot be satisfied by downgrading indiscriminately: a
/// server that *confirmed* it is modern with a `DiscoverResult` and then fails `tools/list`
/// must surface that failure, never retry a handshake its revision deleted.
#[test]
fn a_confirmed_modern_server_failing_tools_list_is_not_downgraded() {
    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![
            Reply::json(200, DISCOVER_RESULT),
            Reply::json(
                200,
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"internal"}}"#,
            ),
        ],
    );

    let mut client = DiscoveryClient::http(endpoint);
    let err = client.discover().expect_err("a confirmed modern server's failure must surface");
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(
        requests.len(),
        2,
        "no legacy retry: the server told us it is modern, and the spec forbids downgrading \
         on a recognised modern signal"
    );
    assert!(requests.iter().all(|r| r.method() != "initialize"), "no initialize may be sent");
    match err {
        DiscoveryError::ServerError { code, .. } => assert_eq!(code, -32603),
        other => panic!("expected the tools/list error to surface, got {other:?}"),
    }
}

/// A `supportedVersions` list offering nothing this client implements is a discovery
/// failure, not a downgrade — and it must not cost a second request either.
#[test]
fn supported_versions_offering_nothing_implemented_fails_without_a_second_request() {
    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![Reply::json(
            200,
            r#"{"jsonrpc":"2.0","id":0,"result":{"resultType":"complete","supportedVersions":["2031-01-01"],"capabilities":{},"ttlMs":0,"cacheScope":"public"}}"#,
        )],
    );

    let mut client = DiscoveryClient::http(endpoint);
    let err = client.discover().expect_err("an empty version intersection must not succeed");
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(requests.len(), 1);
    match err {
        DiscoveryError::Protocol(msg) => {
            assert!(msg.contains("version negotiation failed"), "got: {msg}");
            assert!(msg.contains("2031-01-01"), "must name what the server offered: {msg}");
        }
        other => panic!("expected a Protocol negotiation failure, got {other:?}"),
    }
}

/// P0-09's review finding, preserved verbatim in effect: a transport-level failure
/// (connection refused, DNS, TLS, and — exercised here — hitting the configured request
/// timeout) must **not** trigger a second round trip. A connection-level failure means the
/// transport never reached the server, so retrying says nothing about which lifecycle it
/// speaks and only doubles the cost of every unresponsive host in a sweep
/// (`xtask/src/census_stage1.rs` tunes its timeout down specifically to bound that cost).
///
/// The fake server here accepts connections but never writes a response, so `ureq`'s own
/// request timeout — not a server-side rejection — is what ends the call. Asserted two ways:
/// by counting connections (exactly one, the modern probe), and as a wall-clock proxy (the
/// whole call returns in roughly one timeout, not two).
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
    let err =
        client.discover().expect_err("a connection-level timeout must surface, not be masked");
    let elapsed = started.elapsed();

    assert!(
        matches!(err, DiscoveryError::Transport(_)),
        "expected the original Transport error to surface untouched, got {err:?}"
    );
    assert!(
        elapsed < timeout * 2,
        "discover() must fail after roughly one timeout ({timeout:?}), not attempt a second \
         full round trip: took {elapsed:?}"
    );

    let connection_count = server.join().expect("fake server thread must not panic");
    assert_eq!(
        connection_count, 1,
        "exactly one connection (the modern probe) must be made; a second means a \
         transport-level failure was wrongly treated as an era signal"
    );
}

/// SSE is out of scope for this transport in both eras. The modern probe's SSE upgrade
/// classifies as a malformed modern response and triggers the fallback (which is correct —
/// two of the three dual-era servers in the re-survey answer a *legacy* `initialize` over
/// SSE while answering `server/discover` as JSON), and the fallback's own SSE response is
/// what finally fails.
#[test]
fn discover_rejects_an_sse_upgrade_it_does_not_support() {
    let (listener, endpoint) = bind();
    let sse = || {
        Reply::json(200, "event: message\ndata: {}\n\n").content_type("text/event-stream")
    };
    let server = serve(listener, vec![sse(), sse()]);

    let mut client = DiscoveryClient::http(endpoint);
    let err = client.discover().expect_err("SSE upgrade must be rejected, not mishandled");
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(requests.len(), 2, "the probe's SSE triggers one fallback attempt, no more");
    assert!(matches!(err, DiscoveryError::Protocol(_)), "got {err:?}");
}
