//! P0-12, over the wire: the intra-host delay and the registrable-domain request budget,
//! driven end to end through a real [`DiscoveryClient`] against a real `TcpListener`.
//!
//! Same discipline as `http_discovery.rs`, which this file borrows its fake server from:
//! every test scripts an exact number of responses and then asserts the exact number of
//! requests that arrived. Pacing must change a sweep's *timing* and nothing else — if any
//! test here saw a different request count than its unpaced equivalent in
//! `http_discovery.rs`, pacing would have altered the protocol, which it must not.
//!
//! Two further properties are asserted that the unit tests in `discovery::pace` cannot
//! reach, because they are about the wiring rather than the policy:
//!
//! - **Every request that goes on the wire is charged.** `requests.len()` from the fake
//!   server is compared against `Pacer::accounting().requests_sent`, so a request built on
//!   a path that bypassed the pacing hook would show up as a mismatch.
//! - **A refused host costs no connection at all.** The listener is scripted with zero
//!   replies and asserted to have accepted nothing.
//!
//! **Wall-clock** timing assertions here are one-sided (at least N intervals), which is
//! what makes them safe on a loaded machine: a slow host or a descheduled thread can only
//! make elapsed time *longer*, never shorter than the spacing the pacer imposed.
//!
//! That reasoning does **not** transfer to `Accounting::waited`, and assuming it did is how
//! this file once grew a test that failed 3/3 under CPU saturation. Accounted wait is
//! `Σ max(0, slot − now)`: it is the spacing the pacer *had to add*, so the longer the
//! previous request and the scheduler take, the closer `now` creeps to `slot` and the
//! **smaller** `waited` gets. A lower bound near `N × interval` therefore asserts the exact
//! negation of the property the module advertises — "the previous request's time on the wire
//! counts toward the interval, so a slow host costs no extra spacing at all" — and on a
//! machine slow enough it can legitimately reach zero. Accounted wait is asserted here only
//! as *non-zero*, with the real claim (three intervals of observed wall clock) carried by
//! the wall-clock assertion that precedes it.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use discovery::{
    DiscoveryClient, DiscoveryError, DiscoveryPath, MAX_REQUESTS_PER_DISCOVERY, PacePolicy,
    Pacer, StopReason,
};

/// Short enough to keep the suite fast, long enough that three of them are unambiguous
/// against scheduler noise.
const INTERVAL: Duration = Duration::from_millis(120);

struct Recorded {
    body: Vec<u8>,
}

impl Recorded {
    fn method(&self) -> String {
        let json: serde_json::Value =
            serde_json::from_slice(&self.body).expect("the client always sends valid JSON");
        json["method"].as_str().unwrap_or_default().to_string()
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
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse().expect("numeric Content-Length"))
        })
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    stream.read_exact(&mut body).expect("read body");
    Recorded { body }
}

fn write_json(stream: &mut TcpStream, body: &str) {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).expect("write header");
    stream.write_all(body.as_bytes()).expect("write body");
    stream.flush().expect("flush");
}

/// Serve exactly `script.len()` requests, then return what arrived. Joining the handle is
/// therefore also an assertion that the client sent no more than `script.len()` requests.
fn serve(listener: TcpListener, script: Vec<String>) -> thread::JoinHandle<Vec<Recorded>> {
    thread::spawn(move || {
        let mut received = Vec::new();
        for body in &script {
            let (mut stream, _) = listener.accept().expect("accept a scripted request");
            received.push(read_one_http_request(&mut stream));
            write_json(&mut stream, body);
        }
        received
    })
}

fn bind() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!("http://{}/mcp", listener.local_addr().expect("local addr"));
    (listener, endpoint)
}

fn pacer(cap: usize) -> Arc<Pacer> {
    Arc::new(Pacer::new(PacePolicy {
        min_interval: INTERVAL,
        max_requests_per_registrable_domain: cap,
    }))
}

const DISCOVER_RESULT: &str = r#"{"jsonrpc":"2.0","id":0,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{},"ttlMs":0,"cacheScope":"private"}}"#;
const LEGACY_PROBE_REFUSAL: &str =
    r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"method not found"}}"#;
const LEGACY_INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"fake","version":"0"}}}"#;

fn tools_list(id: u64, modern: bool) -> String {
    let extra = if modern { r#""resultType":"complete","ttlMs":0,"cacheScope":"private","# } else { "" };
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"result":{{{extra}"tools":[{{"name":"read_file","inputSchema":{{"type":"object"}}}}]}}}}"#
    )
}

/// **The headline claim.** A legacy server's whole four-request sequence — P0-11's
/// modern-first probe, `initialize`, `notifications/initialized`, `tools/list` — is spaced,
/// not just the gap between one host and the next. Three gaps, three intervals.
///
/// This is the case the delay exists for: ~75% of reachable hosts in the committed July
/// corpus are legacy, and modern-first took them from three requests to four, all of which
/// previously went out back to back.
#[test]
fn every_request_of_a_legacy_fallback_sequence_is_spaced() {
    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![
            LEGACY_PROBE_REFUSAL.to_string(),
            LEGACY_INITIALIZE.to_string(),
            "{}".to_string(),
            tools_list(2, false),
        ],
    );

    let pacer = pacer(100);
    let permit = pacer.admit(&endpoint).expect("budget available");
    let mut client = DiscoveryClient::http_paced(endpoint, Duration::from_secs(5), permit);

    let start = Instant::now();
    let discovery = client.discover().expect("a legacy server must be discoverable");
    let elapsed = start.elapsed();
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(requests.len(), 4, "pacing must not change the request sequence");
    assert_eq!(requests[0].method(), "server/discover");
    assert_eq!(requests[1].method(), "initialize");
    assert_eq!(requests[2].method(), "notifications/initialized");
    assert_eq!(requests[3].method(), "tools/list");
    assert_eq!(discovery.discovery_path, DiscoveryPath::Initialize);

    assert!(
        elapsed >= 3 * INTERVAL,
        "four requests to one host must cost three intervals ({:?}), took {elapsed:?}",
        3 * INTERVAL
    );
    // Every request on the wire was charged — including the `notifications/initialized`
    // POST, which is a notification rather than a call and so is the one most easily missed.
    drop(client);
    let accounting = pacer.accounting();
    assert_eq!(accounting.requests_sent, requests.len(), "every wire request must be charged");
    assert_eq!(accounting.max_requests_to_one_registrable_domain, 4);
    assert_eq!(accounting.registrable_domains_contacted, 1);
    assert_eq!(accounting.overshoot_requests, 0);
    // The wall-clock assertion above is the claim; this one only checks the accounting is
    // wired up at all. It is deliberately `> ZERO` and not `>= 3 × INTERVAL − ε`: accounted
    // wait is the spacing the pacer *had to add*, so the slower the machine the smaller it
    // gets, and a tight lower bound here fails 3/3 under CPU saturation while asserting the
    // negation of this module's own "a slow host costs no extra spacing" property. See the
    // file doc comment.
    assert!(
        accounting.waited > Duration::ZERO,
        "three spaced gaps must account for some wait, got {:?}",
        accounting.waited
    );
}

/// A modern server's two-request sequence costs exactly one interval — so pacing's cost is
/// proportional to the requests a host actually receives, not to a fixed per-host penalty.
#[test]
fn a_modern_success_costs_two_requests_and_one_interval() {
    let (listener, endpoint) = bind();
    let server = serve(listener, vec![DISCOVER_RESULT.to_string(), tools_list(1, true)]);

    let pacer = pacer(100);
    let permit = pacer.admit(&endpoint).expect("budget available");
    let mut client = DiscoveryClient::http_paced(endpoint, Duration::from_secs(5), permit);

    let start = Instant::now();
    let discovery = client.discover().expect("a modern server must be discoverable");
    let elapsed = start.elapsed();
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(requests.len(), 2);
    assert_eq!(discovery.discovery_path, DiscoveryPath::ServerDiscover);
    assert!(elapsed >= INTERVAL, "one gap must be paid, took {elapsed:?}");
    assert!(elapsed < 10 * INTERVAL, "no more than one gap is paid, took {elapsed:?}");
    drop(client);
    assert_eq!(pacer.accounting().requests_sent, 2);
}

/// **Exhaustion is recorded, and costs nothing on the wire.** The budget is consulted before
/// a client exists, so a refused host never opens a connection — asserted by scripting the
/// listener with zero replies and confirming it accepted nothing.
#[test]
fn a_host_whose_domain_budget_is_spent_is_refused_without_a_connection() {
    let (listener, endpoint) = bind();
    let server = serve(listener, Vec::new());

    // A cap of exactly one reservation: the first sequence's permit consumes all headroom.
    let pacer = pacer(MAX_REQUESTS_PER_DISCOVERY);
    let held = pacer.admit(&endpoint).expect("the first sequence fits exactly");

    let denied = pacer.admit(&endpoint).expect_err("the second must be refused");
    assert_eq!(denied.registrable_domain, "127.0.0.1", "an IP literal is its own key");
    assert_eq!(denied.cap, MAX_REQUESTS_PER_DISCOVERY);
    assert!(denied.to_string().contains("no request was made to this host"), "{denied}");

    drop(held);
    let requests = server.join().expect("fake server thread must not panic");
    assert!(requests.is_empty(), "a refused host must not be connected to at all");

    let accounting = pacer.accounting();
    assert_eq!(accounting.sequences_denied, 1, "the refusal is counted, not swallowed");
    assert_eq!(accounting.requests_sent, 0);
}

/// Two registry entries pointing at the same vendor are spaced *from each other*, which a
/// per-request delay scoped to one `discover()` call would not do. Both loopback endpoints
/// share the registrable domain `127.0.0.1`, which is exactly the shape the July corpus's
/// 149 `gateway.pipeworx.io` entries and 18 `*.klymax402.com` hosts have.
#[test]
fn two_entries_sharing_a_registrable_domain_are_spaced_across_discoveries() {
    let pacer = pacer(100);
    let mut total = 0usize;
    let start = Instant::now();

    for _ in 0..2 {
        let (listener, endpoint) = bind();
        let server = serve(listener, vec![DISCOVER_RESULT.to_string(), tools_list(1, true)]);
        let permit = pacer.admit(&endpoint).expect("budget available");
        let mut client = DiscoveryClient::http_paced(endpoint, Duration::from_secs(5), permit);
        client.discover().expect("discoverable");
        drop(client);
        total += server.join().expect("fake server thread must not panic").len();
    }

    assert_eq!(total, 4, "two entries, two requests each");
    assert!(
        start.elapsed() >= 3 * INTERVAL,
        "four requests to one registrable domain must cost three intervals, took {:?}",
        start.elapsed()
    );
    assert_eq!(pacer.accounting().requests_sent, 4);
}

/// An unpaced client is unchanged: no permit, no waiting, and the same request sequence.
/// This is what keeps every pre-P0-12 caller and every request-count test in
/// `http_discovery.rs` meaning what it meant.
#[test]
fn an_unpaced_client_sends_the_same_sequence_with_no_delay() {
    let (listener, endpoint) = bind();
    let server = serve(listener, vec![DISCOVER_RESULT.to_string(), tools_list(1, true)]);

    let start = Instant::now();
    let mut client = DiscoveryClient::http_with_timeout(endpoint, Duration::from_secs(5));
    client.discover().expect("discoverable");
    let elapsed = start.elapsed();
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(requests.len(), 2);
    assert!(elapsed < INTERVAL, "an unpaced client must not sleep, took {elapsed:?}");
}

/// **B1, end to end.** A redirect is not followed, so it costs exactly one request and
/// lands in the failure bucket a `redirect failed` already landed in.
///
/// This is the test the defect needed. Before the fix the agent carried `ureq`'s default of
/// ten redirects, and the loop lived *inside* `HttpTransport::post`, below the pacing hook:
/// one `post` under a redirect chain was up to eleven requests with zero spacing between
/// them, all but the first uncharged, and a cross-host `Location` returned another host's
/// bytes under the listed server's name. Here the fake server answers a `302` pointing at a
/// second listener, and the assertions are that the second listener is never contacted, that
/// exactly one request was sent, and that the request was charged exactly once.
///
/// **July comparability:** with no redirect followed the `302` arrives as an ordinary
/// response, fails `call_raw`'s status gate as `DiscoveryError::HttpStatus { status: 302 }`,
/// and `census_report::http_status_category` maps any non-`429` status to `transport` ---
/// which is where the committed July file's 15 `redirect failed` entries already sit. Only
/// the detail string changes, from `redirect failed` to `http status: 302`.
#[test]
fn a_redirect_is_not_followed_costs_one_request_and_surfaces_as_a_status() {
    // The host a `Location` header would send us to, if anything followed one. In the
    // measured defect this reached services on the operator's own machine that no registry
    // entry named; here it is a loopback listener that must never be touched.
    let (elsewhere, elsewhere_endpoint) = bind();
    let elsewhere_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let hits = Arc::clone(&elsewhere_hits);
        elsewhere.set_nonblocking(true).expect("nonblocking");
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if let Ok((mut stream, _)) = elsewhere.accept() {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = read_one_http_request(&mut stream);
                    write_json(&mut stream, &tools_list(0, true));
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
    }

    let (listener, endpoint) = bind();
    let redirector = {
        let location = elsewhere_endpoint.clone();
        thread::spawn(move || {
            let mut seen = 0usize;
            // Accept up to three, so a client that *did* follow the redirect would be
            // counted rather than deadlocking the test.
            listener.set_nonblocking(true).expect("nonblocking");
            let deadline = Instant::now() + Duration::from_secs(2);
            while seen < 3 && Instant::now() < deadline {
                if let Ok((mut stream, _)) = listener.accept() {
                    seen += 1;
                    let _ = read_one_http_request(&mut stream);
                    let header = format!(
                        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\
                         Connection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.flush();
                }
                thread::sleep(Duration::from_millis(5));
            }
            seen
        })
    };

    let pacer = pacer(100);
    let permit = pacer.admit(&endpoint).expect("budget available");
    let stats = permit.stats();
    let mut client = DiscoveryClient::http_paced(endpoint, Duration::from_secs(5), permit);
    let error = client.discover().expect_err("a 302 is not a discoverable server");
    drop(client);

    let hits_here = redirector.join().expect("redirector thread");
    assert_eq!(hits_here, 1, "a 302 must cost exactly one request, not a chain of them");
    assert_eq!(
        elsewhere_hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the redirect target must never be contacted: following a `Location` is an SSRF \
         reachable from a server under test, and the harness treats server output as \
         evidence, never instruction"
    );

    // The status surfaces as itself, so the taxonomy keeps its July meaning.
    match error {
        DiscoveryError::HttpStatus { status, .. } => assert_eq!(status, 302),
        other => panic!("a 302 must surface as HttpStatus, got {other:?}"),
    }

    // And it was charged exactly once: the accounting cannot understate real load, which is
    // what a hop below the pacing hook would have made it do.
    let accounting = pacer.accounting();
    assert_eq!(accounting.requests_sent, 1);
    assert_eq!(stats.requests_sent(), 1);
    assert_eq!(accounting.overshoot_requests, 0);
}

/// A discovery sequence never exceeds the reservation the budget arithmetic assumes.
/// `discover()`'s documented bound is five requests; the budget's hard-cap property rests on
/// that, so it is asserted against real behaviour rather than trusted to a doc comment.
#[test]
fn a_real_discovery_never_exceeds_the_per_discovery_reservation() {
    // The most expensive path there is: an optimistic `modern_without_discover` guess whose
    // `tools/list` fails, corrected by a full legacy handshake. Five requests.
    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![
            // 404 + -32601 would normally come with a 404 status; this fake only writes 200,
            // so the legacy `-32601` shape is used and the sequence is the four-request one.
            LEGACY_PROBE_REFUSAL.to_string(),
            LEGACY_INITIALIZE.to_string(),
            "{}".to_string(),
            tools_list(2, false),
        ],
    );

    let pacer = Arc::new(Pacer::new(PacePolicy {
        min_interval: Duration::ZERO,
        max_requests_per_registrable_domain: 1_000,
    }));
    let permit = pacer.admit(&endpoint).expect("budget available");
    let mut client = DiscoveryClient::http_paced(endpoint, Duration::from_secs(5), permit);
    client.discover().expect("discoverable");
    drop(client);

    let requests = server.join().expect("fake server thread must not panic");
    let accounting = pacer.accounting();
    assert_eq!(accounting.requests_sent, requests.len());
    assert!(
        accounting.requests_sent <= MAX_REQUESTS_PER_DISCOVERY,
        "one discovery sent {} requests, over the reservation of {MAX_REQUESTS_PER_DISCOVERY}",
        accounting.requests_sent
    );
    assert_eq!(accounting.overshoot_requests, 0);
}

// ---- P0-13: pagination against the domain budget ----

/// One `tools/list` page with an optional cursor, for the pagination tests below.
fn tools_list_page(id: u64, next_cursor: Option<&str>) -> String {
    let cursor = match next_cursor {
        Some(c) => format!(r#","nextCursor":"{c}""#),
        None => String::new(),
    };
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":[{{"name":"t{id}","inputSchema":{{}}}}]{cursor}}}}}"#
    )
}

/// **The property the budget decision turns on.** A paginating walk *buys* each page beyond
/// the first, so its permit's reservation grows to match what it actually spent and
/// `overshoot_requests` stays zero.
///
/// Before P0-13 the only available shape was to let the walk spend past its 5-request
/// reservation and publish the excess as `overshoot_requests` after the fact. That makes the
/// per-domain cap soft: the extra requests are charged against headroom reserved for *later
/// hosts on the same domain*, and the number that discloses it appears only once the requests
/// have already gone out. Buying ahead keeps the cap hard and keeps the disclosure honest —
/// a non-zero overshoot now means a genuine accounting bug, not "a server paginated".
#[test]
fn a_paginated_walk_buys_each_page_and_never_overshoots_its_reservation() {
    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![
            DISCOVER_RESULT.to_string(),
            tools_list_page(1, Some("c2")),
            tools_list_page(2, Some("c3")),
            tools_list_page(3, None),
        ],
    );

    let pacer = pacer(100);
    let permit = pacer.admit(&endpoint).expect("budget available");
    let stats = permit.stats();
    assert_eq!(stats.reservation(), MAX_REQUESTS_PER_DISCOVERY, "admission reserves five");

    let mut client = DiscoveryClient::http_paced(endpoint, Duration::from_secs(5), permit);
    let discovery = client.discover().expect("a paginated walk must be discoverable");
    let requests = server.join().expect("fake server thread must not panic");
    drop(client);

    assert_eq!(requests.len(), 4, "probe plus three pages");
    assert_eq!(discovery.pagination.pages_fetched, 3);
    assert_eq!(stats.requests_sent(), 4, "every request on the wire must be charged");
    assert_eq!(
        stats.reservation(),
        MAX_REQUESTS_PER_DISCOVERY + 2,
        "two pages beyond the first must each have been bought, not merely spent"
    );
    assert_eq!(stats.overshoot(), 0);

    let accounting = pacer.accounting();
    assert_eq!(accounting.requests_sent, 4);
    assert_eq!(accounting.overshoot_requests, 0, "a paginating walk is not overshoot");
    assert_eq!(accounting.page_extensions_denied, 0);
}

/// A paginating server cannot quietly erode the budget for later hosts on its domain: when
/// the cap has no room for the next page, the walk is **truncated and recorded** rather than
/// sending the request anyway.
///
/// Two halves, same domain, one pacer — because the point is the interaction. The first host
/// paginates until the budget refuses it; the second then still gets admitted, because the
/// first could not spend headroom it had not bought.
#[test]
fn a_walk_that_cannot_buy_its_next_page_is_truncated_and_the_refusal_is_counted() {
    // The *domain's* headroom must be what refuses here, not the sequence's own share
    // (`one_sequence_cannot_buy_more_than_its_share_of_the_domain` covers that branch). So
    // the cap is the census default — a share of 15, which this walk never approaches — and
    // a co-tenant holds 110 of it for the whole walk. Admission takes reserved to 115; after
    // probe + page 1 the domain shows charged 2. Page 2 needs 2 + 116 = 118 <= 120, page 3
    // needs 3 + 117 = 120 <= 120, page 4 would need 4 + 118 = 122 > 120. Three pages, and
    // the fourth refused.
    let pacer = pacer(120);

    let (listener, endpoint) = bind();
    let _co_tenant = pacer.admit_with_reservation(&endpoint, 110).expect("budget available");
    let server = serve(
        listener,
        vec![
            DISCOVER_RESULT.to_string(),
            tools_list_page(1, Some("c2")),
            tools_list_page(2, Some("c3")),
            tools_list_page(3, Some("c4")),
        ],
    );
    let permit = pacer.admit(&endpoint).expect("budget available");
    let stats = permit.stats();
    let mut client = DiscoveryClient::http_paced(endpoint, Duration::from_secs(5), permit);
    let discovery = client.discover().expect("a budget-truncated walk is still a discovery");
    let requests = server.join().expect("fake server thread must not panic");
    drop(client);

    assert_eq!(requests.len(), 4, "the page the budget refused must never be requested");
    assert_eq!(discovery.pagination.pages_fetched, 3);
    assert_eq!(
        discovery.pagination.stopped_because,
        StopReason::BudgetExhausted,
        "the harness's own control truncated this list, and the record must say so rather \
         than reporting a short tool list as complete"
    );
    assert!(!discovery.pagination.cursors_exhausted());
    assert_eq!(stats.overshoot(), 0, "a refused page is not an overshot one");

    let accounting = pacer.accounting();
    assert_eq!(accounting.page_extensions_denied, 1);
    assert_eq!(
        accounting.sequences_denied, 0,
        "a refused page is not a refused host — the two must not share a bucket, because \
         one means a truncated tool list and the other means no contact at all"
    );
    assert_eq!(accounting.overshoot_requests, 0);
    let row = &accounting.domains[0];
    assert_eq!(row.page_extensions_denied, 1);
    assert_eq!(
        row.page_extensions_denied_by_sequence_share, 0,
        "this refusal was the domain's headroom, not the walk's own share"
    );
}

/// **B2.** One sequence may never hold more than `cap / MAX_SEQUENCE_SHARE_DIVISOR` of its
/// domain's budget, however much headroom the domain still has — so a paginating listing
/// cannot spend the budget its co-tenants are admitted from.
///
/// At the census cap of 120 the share is 15: the 5-request admission plus ten bought pages,
/// so eleven pages are fetched and the twelfth is refused and never requested, with the
/// domain more than 85% empty. The refusal is published per domain as a *share* refusal, so
/// a reader can tell "this host hit its own ceiling" from "this domain was full".
#[test]
fn one_sequence_cannot_buy_more_than_its_share_of_the_domain() {
    const PAGES_GRANTED: usize = 11;
    let pacer = pacer(120);
    let (listener, endpoint) = bind();
    let mut script = vec![DISCOVER_RESULT.to_string()];
    for page in 1..=PAGES_GRANTED as u64 {
        script.push(tools_list_page(page, Some(&format!("c{}", page + 1))));
    }
    let server = serve(listener, script);

    let permit = pacer.admit(&endpoint).expect("budget available");
    let stats = permit.stats();
    let mut client = DiscoveryClient::http_paced(endpoint, Duration::from_secs(5), permit);
    let discovery = client.discover().expect("a share-truncated walk is still a discovery");
    let requests = server.join().expect("fake server thread must not panic");
    drop(client);

    assert_eq!(requests.len(), 1 + PAGES_GRANTED, "the page past the share is never requested");
    assert_eq!(discovery.pagination.pages_fetched, PAGES_GRANTED);
    assert_eq!(discovery.pagination.stopped_because, StopReason::BudgetExhausted);
    assert_eq!(stats.reservation(), 15, "the sequence stopped exactly at its share");
    assert_eq!(stats.overshoot(), 0);

    let accounting = pacer.accounting();
    assert_eq!(accounting.max_reservation_per_sequence, 15);
    assert_eq!(accounting.page_extensions_denied, 1);
    let row = &accounting.domains[0];
    assert_eq!(row.page_extensions_granted, PAGES_GRANTED - 1);
    assert_eq!(row.page_extensions_denied_by_sequence_share, 1);
    assert_eq!(row.largest_sequence_reservation, 15);
    assert!(row.requests < 120 - 100, "the domain had ample room; the share is what refused");
}

/// The budget a paginating walk bought is **returned** when its permit drops, or the walk
/// would strand headroom and refuse later hosts for budget nobody is using.
#[test]
fn an_extended_reservation_is_released_with_its_permit() {
    // Cap 48: the smallest cap whose share (48 / 8 = 6) admits the one extension this walk
    // buys, so the release is tested rather than the share refusing it.
    let pacer = pacer(48);
    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![DISCOVER_RESULT.to_string(), tools_list_page(1, Some("c2")), tools_list_page(2, None)],
    );

    let permit = pacer.admit(&endpoint).expect("budget available");
    let mut client = DiscoveryClient::http_paced(endpoint.clone(), Duration::from_secs(5), permit);
    client.discover().expect("discover");
    server.join().expect("fake server thread must not panic");
    drop(client);

    // 3 requests charged, and the 6-request reservation (5 + 1 extension) fully returned, so
    // 48 - 3 = 45 of headroom remains: exactly nine further sequences, and not a tenth.
    let held: Vec<_> = (0..9)
        .map(|i| {
            pacer
                .admit(&endpoint)
                .unwrap_or_else(|_| panic!("sequence {i} refused: the extension was stranded"))
        })
        .collect();
    assert!(held.iter().all(|p| p.reservation() == MAX_REQUESTS_PER_DISCOVERY));
    assert!(pacer.admit(&endpoint).is_err(), "45 of headroom admits nine, not ten");
    assert_eq!(pacer.accounting().requests_sent, 3);
}

/// Each page beyond the first is spaced like any other request, which is the cost disclosed
/// rather than hidden: a walk of N pages is an N-interval conversation with one host.
#[test]
fn every_page_of_a_walk_is_spaced_like_any_other_request() {
    let (listener, endpoint) = bind();
    let server = serve(
        listener,
        vec![
            DISCOVER_RESULT.to_string(),
            tools_list_page(1, Some("c2")),
            tools_list_page(2, Some("c3")),
            tools_list_page(3, None),
        ],
    );

    let pacer = pacer(100);
    let permit = pacer.admit(&endpoint).expect("budget available");
    let mut client = DiscoveryClient::http_paced(endpoint, Duration::from_secs(5), permit);

    let start = Instant::now();
    client.discover().expect("discover");
    let elapsed = start.elapsed();
    let requests = server.join().expect("fake server thread must not panic");

    assert_eq!(requests.len(), 4);
    // Four requests, three gaps. One-sided, per this file's own doc comment: a loaded
    // machine can only make wall clock longer.
    assert!(
        elapsed >= 3 * INTERVAL,
        "four requests to one domain must be spaced by three intervals, took {elapsed:?}"
    );
}
