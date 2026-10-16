//! Registry client (census Stage 0): fetch server listings from the official MCP Registry
//! (<https://registry.modelcontextprotocol.io>) so [`crate::catalogue::ingest`] has
//! something to ingest.
//!
//! Read-only GET requests only, paginated per the registry's own OpenAPI spec
//! (`cursor`/`limit` query parameters; `{"servers": [...], "metadata": {"nextCursor": ...}}`
//! response shape, verified against the live spec rather than assumed). No authentication
//! is required or attempted — every request carries a `User-Agent` identifying this harness
//! and its purpose, so registry operators can see who is polling and why. Visible polling,
//! not anonymous.

use std::time::Duration;

use serde::Deserialize;
use serde_json::value::RawValue;

/// The registry this client is built against.
pub const DEFAULT_BASE_URL: &str = "https://registry.modelcontextprotocol.io";

const USER_AGENT: &str = concat!(
    "mcp-conformance-harness/",
    env!("CARGO_PKG_VERSION"),
    " (registry census; read-only; contact: see repository)"
);

/// One page of registry entries.
#[derive(Debug)]
pub struct Page {
    /// Raw bytes of each entry's `server` object — already unwrapped from the list
    /// envelope's `{"server": ..., "_meta": ...}` wrapper, in exactly the shape
    /// [`crate::catalogue::ingest`] expects.
    pub entries_raw: Vec<Vec<u8>>,
    /// The cursor to request the next page with, or `None` if this was the last page.
    pub next_cursor: Option<String>,
}

/// Why a registry fetch failed.
#[derive(Debug)]
pub enum RegistryError {
    /// A transport-level failure: connection, TLS, timeout, non-2xx status.
    Transport(String),
    /// The response was not the expected JSON shape.
    Malformed(String),
    /// [`RegistryClient::fetch_all`] stopped retrying a page that kept failing transiently —
    /// either because that page used up [`FetchPolicy::max_attempts_per_page`], or because
    /// the whole fetch had already spent [`FetchPolicy::max_retries_per_fetch`].
    ///
    /// Only ever wraps a *transient* failure: a permanent one (`Malformed`, a 4xx other than
    /// 429) is returned as itself on its first occurrence, never retried and never wrapped.
    RetriesExhausted {
        /// 1-based index of the page that failed. Every page before it was fetched and
        /// delivered to the caller's `on_page`.
        page: usize,
        /// Requests made for this page, all of which failed.
        attempts: u32,
        /// `true` if the stop was the fetch-wide retry budget rather than the per-page bound.
        fetch_budget_spent: bool,
        /// The failure of the final attempt.
        last: Box<RegistryError>,
    },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(msg) => write!(f, "registry transport error: {msg}"),
            Self::Malformed(msg) => write!(f, "registry response malformed: {msg}"),
            Self::RetriesExhausted { page, attempts, fetch_budget_spent: false, last } => write!(
                f,
                "registry page {page} failed on all {attempts} attempts (the per-page retry bound); \
                 last error: {last}"
            ),
            Self::RetriesExhausted { page, attempts, fetch_budget_spent: true, last } => write!(
                f,
                "registry page {page} failed after {attempts} attempt(s) and this fetch's retry \
                 budget is already spent — the registry is failing too often to trust this walk; \
                 last error: {last}"
            ),
        }
    }
}

impl std::error::Error for RegistryError {}

/// How long one registry request may take, and how hard [`RegistryClient::fetch_all`] tries
/// before giving up on a page.
///
/// Retry is **per page**: a retry re-requests the page that failed, with the same cursor, so
/// every page already delivered to the caller stays delivered and the walk never restarts.
/// Only transient failures are retried — timeouts, connection-level I/O errors, HTTP 5xx and
/// HTTP 429. A response that arrived and was wrong (`Malformed`, any other 4xx, a TLS
/// failure, an over-long body) is returned at once: retrying it would spend requests on a
/// registry that already answered, and would bury a real defect under a retry loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchPolicy {
    /// `ureq`'s `timeout_global` for one request, DNS through the end of the body.
    pub request_timeout: Duration,
    /// Requests per page, including the first. `1` disables retry.
    pub max_attempts_per_page: u32,
    /// Retries (requests beyond each page's first) the whole fetch may spend. Bounds a
    /// registry that is failing on many pages, which the per-page bound alone does not.
    pub max_retries_per_fetch: u32,
    /// Pause after a page's first failure; doubles after each further failure of that page.
    pub initial_backoff: Duration,
    /// The longest single pause between two attempts, whatever asked for it — the backoff
    /// schedule or a server's `Retry-After`. This is the cap that stops a hostile or broken
    /// `Retry-After: 86400` from parking a sweep for a day.
    pub max_wait: Duration,
}

impl FetchPolicy {
    /// The policy every live client uses.
    ///
    /// - **`request_timeout` stays 30 s.** Sampled live pages took 0.3–6.6 s, and one page
    ///   requested twice back to back took 0.65 s then 1.24 s — the slowness is per request,
    ///   not a fixed property of a page, which is exactly the case a retry fixes and a longer
    ///   timeout does not. 30 s is ~4.5× the slowest page seen. Raising it would only make
    ///   each *stalled* attempt cost more and double every worst-case bound below; lowering
    ///   it would turn a registry that is uniformly slow but working into a failed fetch.
    /// - **4 attempts per page** (3 retries), pausing 1 s, 2 s, 4 s.
    /// - **20 retries per fetch.** Two failed sweeps each hit one bad page in ~190; 20 is an
    ///   order of magnitude of headroom over that, and still stops a walk where something is
    ///   plainly wrong rather than limping through it at a minute a page.
    /// - **30 s longest single pause**, so a `Retry-After` is honoured up to 30 s and clamped
    ///   beyond it.
    ///
    /// Worst case, from [`Self::worst_case_added_per_page`] and
    /// [`Self::worst_case_added_per_fetch`]: **180 s added to one page** (3 retries × (30 s
    /// pause + 30 s request)), **20 min added to a whole fetch** (20 retries × 60 s). Typical
    /// cases are far below that: a page that times out three times and then succeeds adds
    /// 97 s (3 × 30 s + 1 + 2 + 4 s); a prompt 503 that clears adds 1 s. A registry that is
    /// down outright fails on page one after ~127 s of wall time (four 30 s timeouts plus
    /// 7 s of backoff), or ~7 s if connections are refused. The 97 s above is time *added*
    /// by retries; this is total time to failure.
    pub const DEFAULT: Self = Self {
        request_timeout: Duration::from_secs(30),
        max_attempts_per_page: 4,
        max_retries_per_fetch: 20,
        initial_backoff: Duration::from_secs(1),
        max_wait: Duration::from_secs(30),
    };

    /// The pause before retrying a page whose `failed_attempt`-th (1-based) attempt just
    /// failed.
    ///
    /// The larger of the backoff schedule and the server's `Retry-After` — a `Retry-After: 0`
    /// does not license hammering — and never more than [`Self::max_wait`].
    #[must_use]
    pub fn delay_before_retry(&self, failed_attempt: u32, retry_after: Option<Duration>) -> Duration {
        let doublings = failed_attempt.saturating_sub(1).min(31);
        let backoff = self.initial_backoff.saturating_mul(1u32 << doublings);
        backoff.max(retry_after.unwrap_or(Duration::ZERO)).min(self.max_wait)
    }

    /// The most time retries can add to a single page: every retry costs at most one pause
    /// and one full request timeout.
    #[must_use]
    pub fn worst_case_added_per_page(&self) -> Duration {
        self.per_retry_ceiling().saturating_mul(self.max_attempts_per_page.saturating_sub(1))
    }

    /// The most time retries can add to a whole [`RegistryClient::fetch_all`].
    #[must_use]
    pub fn worst_case_added_per_fetch(&self) -> Duration {
        self.per_retry_ceiling().saturating_mul(self.max_retries_per_fetch)
    }

    fn per_retry_ceiling(&self) -> Duration {
        self.request_timeout.saturating_add(self.max_wait)
    }
}

impl Default for FetchPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One retry [`RegistryClient::fetch_all`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryEvent {
    /// 1-based index of the page that was retried.
    pub page: usize,
    /// Which attempt at that page failed (1 = the first request for it).
    pub failed_attempt: u32,
    /// Why it failed, as the error would have read had it not been retried.
    pub cause: String,
    /// The server's `Retry-After`, in seconds, if it sent a parseable one. Uncapped, so a
    /// reader can see what was asked for as well as what was done (`waited`).
    pub retry_after_secs: Option<u64>,
    /// How long the client actually paused before the next attempt.
    pub waited: Duration,
}

/// What a successful [`RegistryClient::fetch_all`] took — enough for a results header to
/// show whether the walk was clean or fragile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchReport {
    /// The policy the fetch ran under.
    pub policy: FetchPolicy,
    /// Pages fetched and delivered to `on_page`.
    pub pages_fetched: usize,
    /// HTTP requests sent, retries included.
    pub requests_sent: usize,
    /// Pages that needed at least one retry before they succeeded.
    pub pages_retried: usize,
    /// Every retry, in order. Bounded by [`FetchPolicy::max_retries_per_fetch`].
    pub retries: Vec<RetryEvent>,
}

impl FetchReport {
    fn new(policy: FetchPolicy) -> Self {
        Self { policy, pages_fetched: 0, requests_sent: 0, pages_retried: 0, retries: Vec::new() }
    }

    /// Total time spent pausing between attempts.
    #[must_use]
    pub fn time_waited(&self) -> Duration {
        self.retries.iter().map(|r| r.waited).sum()
    }

    /// The report as a JSON object for a results header: the policy (what was promised) and
    /// the accounting (what happened), in the shape P0-12's `pacing` block established.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        serde_json::json!({
            "policy": {
                "request_timeout_ms": ms(self.policy.request_timeout),
                "max_attempts_per_page": self.policy.max_attempts_per_page,
                "max_retries_per_fetch": self.policy.max_retries_per_fetch,
                "initial_backoff_ms": ms(self.policy.initial_backoff),
                "max_wait_ms": ms(self.policy.max_wait),
                "retried": "timeouts, connection errors, HTTP 5xx, HTTP 429 (Retry-After honoured up to max_wait)",
                "worst_case_added_per_page_ms": ms(self.policy.worst_case_added_per_page()),
                "worst_case_added_per_fetch_ms": ms(self.policy.worst_case_added_per_fetch()),
            },
            "pages_fetched": self.pages_fetched,
            "requests_sent": self.requests_sent,
            "pages_retried": self.pages_retried,
            "retries_total": self.retries.len(),
            "time_waited_ms": ms(self.time_waited()),
            "retries": self.retries.iter().map(|r| serde_json::json!({
                "page": r.page,
                "failed_attempt": r.failed_attempt,
                "cause": r.cause,
                "retry_after_secs": r.retry_after_secs,
                "waited_ms": ms(r.waited),
            })).collect::<Vec<_>>(),
        })
    }
}

/// One failed request, with whether it is worth repeating.
struct AttemptFailure {
    error: RegistryError,
    transient: bool,
    /// The server's `Retry-After` in seconds, for a transient HTTP status that carried one.
    retry_after_secs: Option<u64>,
}

impl AttemptFailure {
    fn permanent(error: RegistryError) -> Self {
        Self { error, transient: false, retry_after_secs: None }
    }

    /// Classify a `ureq` error. Transient means the request may simply not have completed:
    /// a timeout, a connection that could not be made or was cut, DNS that did not answer.
    /// Everything else — TLS and certificate failures, protocol violations, an over-long
    /// body, a bad URI — is a defect a retry would only repeat.
    fn from_ureq(error: ureq::Error) -> Self {
        let transient = matches!(
            error,
            ureq::Error::Timeout(_) | ureq::Error::Io(_) | ureq::Error::ConnectionFailed | ureq::Error::HostNotFound
        );
        Self { error: RegistryError::Transport(error.to_string()), transient, retry_after_secs: None }
    }
}

/// `Retry-After` in its delay-seconds form. The HTTP-date form, and anything unparseable or
/// too large for a `u64`, reads as absent — the backoff schedule then applies, so a garbled
/// header can only ever fall back to the bounded default, never extend it.
fn parse_retry_after(headers: &ureq::http::HeaderMap) -> Option<u64> {
    let value = headers.get("retry-after")?.to_str().ok()?.trim();
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// A client for the registry's read-only listing endpoint.
pub struct RegistryClient {
    agent: ureq::Agent,
    base_url: String,
    policy: FetchPolicy,
}

impl Default for RegistryClient {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistryClient {
    /// A client against the real, live registry.
    #[must_use]
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    /// A client against an arbitrary base URL — for tests, a local fake server.
    ///
    /// **`max_redirects(0)`, configured here rather than through `discovery::pace`.** P0-12
    /// closed the redirect-following defect class everywhere else by making agent
    /// construction require a `Permit` with the no-redirect policy baked in, and left this
    /// one client alone for two reasons: routing it through `pace` would create an
    /// `intake → discovery` crate edge architecture.md §8 does not want, and tightening
    /// redirects on the one client a sweep cannot start without would have been an untested
    /// behavioural change on the eve of a sweep. The second objection is resolved — the
    /// registry endpoint was verified to answer `GET /v0.1/servers?limit=1` with HTTP/2 200
    /// directly, no redirect — so the flag is set directly and the crate edge is not
    /// created. The reasoning for *why* zero (see `discovery::MAX_REDIRECTS` and ADR-014 §1)
    /// applies unchanged: a followed `Location` is a server-chosen request made below any
    /// pacing or accounting hook, and the risk here is narrower only because this is one
    /// first-party endpoint rather than ~1,000 hosts under test — narrower is not a reason
    /// to leave ten hops reachable.
    #[must_use]
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self::with_policy(base_url, FetchPolicy::DEFAULT)
    }

    /// A client against an arbitrary base URL under an explicit [`FetchPolicy`] — for tests
    /// that need retries to run in milliseconds rather than seconds.
    ///
    /// `http_status_as_error(false)` is so a 429 or 503 arrives as a response whose
    /// `Retry-After` can be read; [`Self::fetch_page`] then rejects every status ≥ 400 itself,
    /// with the same `http status: N` wording `ureq` used, so nothing a status-as-error agent
    /// refused is accepted now. One textual difference: a 429 or 5xx carrying `Retry-After`
    /// gains a ` (retry-after: Ns)` suffix. It appears only in a retry's `cause` and in
    /// `RetriesExhausted`, never in a per-server failure category, because a registry error
    /// aborts the sweep rather than becoming one, so census comparability is unaffected.
    /// A 3xx is unaffected either way (`ureq` never treated it as an
    /// error) and still reaches the body parser exactly as before.
    #[must_use]
    pub fn with_policy(base_url: impl Into<String>, policy: FetchPolicy) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(policy.request_timeout))
            .max_redirects(0)
            .http_status_as_error(false)
            .build()
            .into();
        Self { agent, base_url: base_url.into(), policy }
    }

    /// Fetch one page of the server listing.
    ///
    /// Always requests `version=latest`. Without it the registry returns every historical
    /// version of every server as a separate entry — found the hard way: an unfiltered
    /// first run produced 59,484 records for what turned out to be 18,664 distinct server
    /// names (some appearing over 1,000 times), a 3.2x inflation that would have corrupted
    /// every downstream count. There is no code path in this client that omits the filter.
    ///
    /// **One request, no retry.** This is the primitive; [`Self::fetch_all`] is where the
    /// retry policy lives.
    pub fn fetch_page(&self, cursor: Option<&str>, limit: u32) -> Result<Page, RegistryError> {
        self.fetch_page_once(cursor, limit).map_err(|failure| failure.error)
    }

    fn fetch_page_once(&self, cursor: Option<&str>, limit: u32) -> Result<Page, AttemptFailure> {
        let limit = limit.clamp(1, 100);
        let mut url = format!("{}/v0.1/servers?version=latest&limit={limit}", self.base_url);
        if let Some(c) = cursor {
            url.push_str("&cursor=");
            url.push_str(&percent_encode_minimal(c));
        }

        let mut response = self
            .agent
            .get(&url)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "application/json")
            .call()
            .map_err(AttemptFailure::from_ureq)?;

        let status = response.status().as_u16();
        if status >= 400 {
            let transient = status == 429 || (500..=599).contains(&status);
            let retry_after_secs = if transient { parse_retry_after(response.headers()) } else { None };
            let mut msg = format!("http status: {status}");
            if let Some(secs) = retry_after_secs {
                msg.push_str(&format!(" (retry-after: {secs}s)"));
            }
            return Err(AttemptFailure { error: RegistryError::Transport(msg), transient, retry_after_secs });
        }

        let bytes = response.body_mut().read_to_vec().map_err(AttemptFailure::from_ureq)?;
        let text = std::str::from_utf8(&bytes).map_err(|e| {
            AttemptFailure::permanent(RegistryError::Malformed(format!("response is not valid UTF-8: {e}")))
        })?;

        let parsed: ListResponse = serde_json::from_str(text).map_err(|e| {
            AttemptFailure::permanent(RegistryError::Malformed(format!("response is not the expected shape: {e}")))
        })?;

        Ok(Page {
            entries_raw: parsed.servers.into_iter().map(|entry| entry.server.get().as_bytes().to_vec()).collect(),
            next_cursor: parsed.metadata.next_cursor,
        })
    }

    /// Fetch one page under the retry policy, recording every request and retry in `report`.
    ///
    /// The cursor is the caller's and is never touched here, so every attempt asks for the
    /// same page. A failed attempt's partial bytes are discarded with it — only a complete,
    /// parsed page is ever returned — so a retry cannot duplicate, drop or reorder entries.
    fn fetch_page_retrying(
        &self,
        cursor: Option<&str>,
        limit: u32,
        report: &mut FetchReport,
    ) -> Result<Page, RegistryError> {
        let page = report.pages_fetched + 1;
        let mut attempt: u32 = 1;
        loop {
            report.requests_sent += 1;
            let failure = match self.fetch_page_once(cursor, limit) {
                Ok(fetched) => return Ok(fetched),
                Err(failure) if !failure.transient => return Err(failure.error),
                Err(failure) => failure,
            };

            let page_bound_hit = attempt >= self.policy.max_attempts_per_page;
            let fetch_budget_spent = report.retries.len() >= self.policy.max_retries_per_fetch as usize;
            if page_bound_hit || fetch_budget_spent {
                return Err(RegistryError::RetriesExhausted {
                    page,
                    attempts: attempt,
                    fetch_budget_spent: fetch_budget_spent && !page_bound_hit,
                    last: Box::new(failure.error),
                });
            }

            let waited = self
                .policy
                .delay_before_retry(attempt, failure.retry_after_secs.map(Duration::from_secs));
            if attempt == 1 {
                report.pages_retried += 1;
            }
            report.retries.push(RetryEvent {
                page,
                failed_attempt: attempt,
                cause: failure.error.to_string(),
                retry_after_secs: failure.retry_after_secs,
                waited,
            });
            std::thread::sleep(waited);
            attempt += 1;
        }
    }

    /// Fetch pages, calling `on_page` as each one arrives rather than buffering the whole
    /// registry in memory before the caller sees anything.
    ///
    /// `on_page` returns `true` to keep going, `false` to stop after this page — a caller
    /// that only needs the first N matching entries (Stage 1's Class B sample, for
    /// instance) can stop as soon as it has enough, rather than scanning every remaining
    /// page in the registry for no reason.
    ///
    /// `delay_between_pages` is a voluntary politeness pause — the registry's OpenAPI spec
    /// documents no rate limit, so this is a courtesy, not a measured requirement. Pass
    /// [`Duration::ZERO`] in tests.
    ///
    /// Each page is fetched under this client's [`FetchPolicy`]: a transient failure re-asks
    /// for *that* page after a backoff, so one slow page out of hundreds costs a retry rather
    /// than the whole walk, while a page that keeps failing — or a fetch that keeps needing
    /// retries — still stops with [`RegistryError::RetriesExhausted`] inside a stated bound.
    /// The returned [`FetchReport`] says how many pages needed retries, for a results header.
    pub fn fetch_all(
        &self,
        page_limit: u32,
        delay_between_pages: Duration,
        mut on_page: impl FnMut(&Page) -> bool,
    ) -> Result<FetchReport, RegistryError> {
        let mut report = FetchReport::new(self.policy);
        let mut cursor: Option<String> = None;
        loop {
            let page = self.fetch_page_retrying(cursor.as_deref(), page_limit, &mut report)?;
            report.pages_fetched += 1;
            let next = page.next_cursor.clone();
            let keep_going = on_page(&page);
            if !keep_going {
                break;
            }
            match next {
                Some(c) => {
                    cursor = Some(c);
                    std::thread::sleep(delay_between_pages);
                }
                None => break,
            }
        }
        Ok(report)
    }
}

/// A minimal percent-encoder for the handful of characters that would otherwise break a
/// query string. Registry cursors are opaque tokens; pulling in a URL-encoding crate for
/// one call site is not worth it.
fn percent_encode_minimal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[derive(Deserialize)]
struct ListResponse<'a> {
    #[serde(borrow)]
    servers: Vec<ListEntry<'a>>,
    metadata: ListMetadata,
}

#[derive(Deserialize)]
struct ListEntry<'a> {
    #[serde(borrow)]
    server: &'a RawValue,
}

#[derive(Deserialize)]
struct ListMetadata {
    #[serde(rename = "nextCursor")]
    next_cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

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
        String::from_utf8_lossy(&buf).into_owned()
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
    fn fetch_page_parses_a_single_page_response() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().expect("addr"));

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let request = read_one_http_request(&mut stream);
            assert!(request.starts_with("GET /v0.1/servers?version=latest&limit=30"));
            assert!(
                request.to_ascii_lowercase().contains("user-agent: mcp-conformance-harness"),
                "request must identify itself: {request}"
            );

            let body = serde_json::json!({
                "servers": [{
                    "server": { "name": "io.example/one", "packages": [{"registryType":"npm","identifier":"x","version":"1.0.0"}] }
                }],
                "metadata": { "count": 1, "nextCursor": null }
            });
            write_json_response(&mut stream, &serde_json::to_vec(&body).unwrap());
        });

        let client = RegistryClient::with_base_url(base_url);
        let page = client.fetch_page(None, 30).expect("fetch");
        server.join().expect("server thread");

        assert_eq!(page.entries_raw.len(), 1);
        assert!(page.next_cursor.is_none());

        // Prove the extracted bytes are exactly what catalogue::ingest expects.
        let outcome = crate::catalogue::ingest(&page.entries_raw[0]);
        match outcome {
            crate::catalogue::IngestOutcome::Resolved(server) => {
                assert_eq!(server.name, "io.example/one");
            }
            other => panic!("expected Resolved, got {other:?}"),
        }
    }

    #[test]
    fn fetch_all_walks_every_page_via_the_cursor() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().expect("addr"));

        let server = thread::spawn(move || {
            // Page 1: has a next cursor.
            let (mut stream, _) = listener.accept().expect("accept page 1");
            let request = read_one_http_request(&mut stream);
            assert!(!request.contains("cursor="), "first request must not carry a cursor");
            let body = serde_json::json!({
                "servers": [{"server": {"name": "io.example/a", "packages": [{"registryType":"npm","identifier":"a","version":"1.0.0"}]}}],
                "metadata": { "count": 1, "nextCursor": "page2token" }
            });
            write_json_response(&mut stream, &serde_json::to_vec(&body).unwrap());

            // Page 2: no next cursor, ends pagination.
            let (mut stream, _) = listener.accept().expect("accept page 2");
            let request = read_one_http_request(&mut stream);
            assert!(request.contains("cursor=page2token"), "second request must carry the cursor: {request}");
            let body = serde_json::json!({
                "servers": [{"server": {"name": "io.example/b", "remotes": [{"type":"sse","url":"https://example.com/sse"}]}}],
                "metadata": { "count": 1, "nextCursor": null }
            });
            write_json_response(&mut stream, &serde_json::to_vec(&body).unwrap());
        });

        let client = RegistryClient::with_base_url(base_url);
        let mut collected = Vec::new();
        client
            .fetch_all(30, Duration::ZERO, |page| {
                collected.extend(page.entries_raw.iter().cloned());
                true
            })
            .expect("fetch_all");
        server.join().expect("server thread");

        assert_eq!(collected.len(), 2);
    }

    /// Proves the stop is real, not just "the caller stops accumulating locally": the fake
    /// server only ever serves one page, despite that page's `nextCursor` promising a
    /// second one. If `fetch_all` requested a second page anyway despite the callback
    /// returning `false`, this test would hang waiting for a connection nobody sends.
    #[test]
    fn fetch_all_stops_early_when_the_callback_returns_false() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().expect("addr"));

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept the only page served");
            read_one_http_request(&mut stream);
            let body = serde_json::json!({
                "servers": [{"server": {"name": "io.example/a", "packages": [{"registryType":"npm","identifier":"a","version":"1.0.0"}]}}],
                "metadata": { "count": 1, "nextCursor": "would-be-page-2" }
            });
            write_json_response(&mut stream, &serde_json::to_vec(&body).unwrap());
        });

        let client = RegistryClient::with_base_url(base_url);
        let mut pages_seen = 0;
        client
            .fetch_all(30, Duration::ZERO, |_page| {
                pages_seen += 1;
                false
            })
            .expect("fetch_all");
        server.join().expect("server thread");

        assert_eq!(pages_seen, 1);
    }

    /// The one remaining `ureq` agent in the tree that still followed redirects (ureq's
    /// default of 10). Asserted against `ureq`'s own config getter rather than the builder
    /// call, because the defect P0-12 measured elsewhere was precisely that the call was
    /// *absent* — a test that re-reads the builder line would pass for a client that never
    /// set it.
    #[test]
    fn the_registry_agent_will_not_follow_a_redirect() {
        let client = RegistryClient::with_base_url("http://127.0.0.1:1");
        assert_eq!(
            client.agent.config().max_redirects(),
            0,
            "the registry client would follow a redirect"
        );
        // The live client, not only the test constructor, since `new()` is what a sweep uses.
        assert_eq!(RegistryClient::new().agent.config().max_redirects(), 0);
    }

    /// The behavioural half: a real 302 pointing at a second real listener costs exactly one
    /// request and the target is never contacted. Scripted and counted on both sides, in the
    /// shape `discovery`'s own redirect test established.
    #[test]
    fn a_registry_redirect_is_not_followed_and_the_target_is_never_contacted() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let elsewhere = TcpListener::bind("127.0.0.1:0").expect("bind target");
        let elsewhere_url = format!("http://{}", elsewhere.local_addr().expect("addr"));
        let elsewhere_hits = Arc::new(AtomicUsize::new(0));
        {
            let hits = Arc::clone(&elsewhere_hits);
            elsewhere.set_nonblocking(true).expect("nonblocking");
            thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while std::time::Instant::now() < deadline {
                    if let Ok((mut stream, _)) = elsewhere.accept() {
                        hits.fetch_add(1, Ordering::SeqCst);
                        let _ = read_one_http_request(&mut stream);
                        write_json_response(&mut stream, b"{\"servers\":[],\"metadata\":{}}");
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            });
        }

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind redirector");
        let base_url = format!("http://{}", listener.local_addr().expect("addr"));
        let redirector = thread::spawn(move || {
            let mut seen = 0usize;
            // Accept up to three, so a client that *did* follow would be counted rather
            // than deadlocking this test.
            listener.set_nonblocking(true).expect("nonblocking");
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while seen < 3 && std::time::Instant::now() < deadline {
                if let Ok((mut stream, _)) = listener.accept() {
                    seen += 1;
                    let _ = read_one_http_request(&mut stream);
                    let header = format!(
                        "HTTP/1.1 302 Found\r\nLocation: {elsewhere_url}/v0.1/servers\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.flush();
                }
                thread::sleep(Duration::from_millis(5));
            }
            seen
        });

        let client = RegistryClient::with_base_url(base_url);
        let err = client.fetch_page(None, 1).expect_err("a 302 is not a registry page");

        let hits_here = redirector.join().expect("redirector thread");
        assert_eq!(hits_here, 1, "a 302 must cost exactly one request, not a chain of them");
        assert_eq!(
            elsewhere_hits.load(Ordering::SeqCst),
            0,
            "the redirect target must never be contacted"
        );
        // Whatever the label, no page is produced from a redirect — which is the property
        // that matters. Disclosed rather than asserted away: `ureq`'s
        // `http_status_as_error` covers 4xx and 5xx only, so an unfollowed 3xx arrives as an
        // ordinary response with an empty body and this client reports it as `Malformed`
        // rather than naming the status. That is a worse *label* than `discovery` produces
        // (which checks the status itself), and it is deliberately left alone here: giving
        // this client a status check is a behavioural change beyond the redirect fix, and
        // the fix's whole point is that nothing a `Location` names is ever contacted.
        assert!(
            matches!(err, RegistryError::Malformed(_) | RegistryError::Transport(_)),
            "a redirect must never yield a page: {err:?}"
        );
    }

    #[test]
    fn malformed_response_is_a_registry_error_not_a_panic() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().expect("addr"));

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            read_one_http_request(&mut stream);
            write_json_response(&mut stream, b"not json");
        });

        let client = RegistryClient::with_base_url(base_url);
        let err = client.fetch_page(None, 30).expect_err("must fail");
        server.join().expect("server thread");

        assert!(matches!(err, RegistryError::Malformed(_)));
    }

    // ---- Per-page retry ----
    //
    // Every test below drives `fetch_all` against a scripted fake that answers each incoming
    // connection with the next step of its script and records the request line it received.
    // The fake keeps accepting until the client has returned, so a request the script did not
    // plan for is *counted* (and answered 500) rather than deadlocking the test — which is
    // what lets each test assert the exact number of requests rather than a lower bound.

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    enum Step {
        /// Answer with these raw HTTP response bytes.
        Respond(Vec<u8>),
        /// Read the request, then never answer — the client's request timeout must fire.
        Stall,
        /// Read the request, then close the connection without a byte of response.
        Hangup,
    }

    fn http(status_line: &str, extra_headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n");
        for (name, value) in extra_headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()));
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    fn page(name: &str, next_cursor: Option<&str>) -> Step {
        let body = serde_json::json!({
            "servers": [{"server": {"name": name, "packages": [{"registryType":"npm","identifier":"x","version":"1.0.0"}]}}],
            "metadata": { "count": 1, "nextCursor": next_cursor }
        });
        Step::Respond(http("200 OK", &[], &serde_json::to_vec(&body).unwrap()))
    }

    fn status(status_line: &str, extra_headers: &[(&str, &str)]) -> Step {
        Step::Respond(http(status_line, extra_headers, b"{}"))
    }

    struct Fake {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        done: Arc<AtomicBool>,
        handle: thread::JoinHandle<()>,
    }

    impl Fake {
        fn spawn(script: Vec<Step>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let base_url = format!("http://{}", listener.local_addr().expect("addr"));
            listener.set_nonblocking(true).expect("nonblocking");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let done = Arc::new(AtomicBool::new(false));
            let handle = {
                let (requests, done) = (Arc::clone(&requests), Arc::clone(&done));
                thread::spawn(move || {
                    let mut script = script.into_iter();
                    let mut held = Vec::new(); // stalled connections, kept open until the end
                    loop {
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                stream.set_nonblocking(false).expect("blocking stream");
                                let request = read_one_http_request(&mut stream);
                                let line = request.lines().next().unwrap_or_default().to_string();
                                requests.lock().unwrap().push(line);
                                match script.next() {
                                    Some(Step::Respond(bytes)) => {
                                        let _ = stream.write_all(&bytes);
                                        let _ = stream.flush();
                                    }
                                    Some(Step::Stall) => held.push(stream),
                                    Some(Step::Hangup) => drop(stream),
                                    None => {
                                        let _ = stream.write_all(&http("500 Unscripted", &[], b"{}"));
                                    }
                                }
                            }
                            // Drain every queued connection before honouring `done`: the
                            // client is sequential and has returned, so anything it sent is
                            // already in the backlog.
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                if done.load(Ordering::SeqCst) {
                                    break;
                                }
                                thread::sleep(Duration::from_millis(2));
                            }
                            Err(e) => panic!("accept: {e}"),
                        }
                    }
                    drop(held);
                })
            };
            Self { base_url, requests, done, handle }
        }

        /// Stop the fake and return every request line it received, in order.
        fn finish(self) -> Vec<String> {
            self.done.store(true, Ordering::SeqCst);
            self.handle.join().expect("fake registry thread");
            Arc::try_unwrap(self.requests).expect("sole owner").into_inner().unwrap()
        }
    }

    /// The production policy's shape (4 attempts, 20-retry budget), with durations shrunk so
    /// the suite runs in seconds. The production values themselves are pinned separately in
    /// `the_live_client_runs_under_the_default_policy`.
    fn fast_policy() -> FetchPolicy {
        FetchPolicy {
            request_timeout: Duration::from_millis(500),
            initial_backoff: Duration::from_millis(10),
            max_wait: Duration::from_secs(2),
            ..FetchPolicy::DEFAULT
        }
    }

    fn names(pages: &[Vec<Vec<u8>>]) -> Vec<String> {
        pages
            .iter()
            .flatten()
            .map(|raw| serde_json::from_slice::<serde_json::Value>(raw).unwrap()["name"].as_str().unwrap().to_string())
            .collect()
    }

    fn collect_all(client: &RegistryClient) -> (Result<FetchReport, RegistryError>, Vec<Vec<Vec<u8>>>) {
        let mut pages = Vec::new();
        let result = client.fetch_all(30, Duration::ZERO, |p| {
            pages.push(p.entries_raw.clone());
            true
        });
        (result, pages)
    }

    /// The failure this change exists to fix: a page that times out mid-walk. It is
    /// re-requested with *its own* cursor — not from page one — and the walk completes with
    /// every entry exactly once, in order.
    #[test]
    fn a_page_that_times_out_is_retried_with_its_cursor_and_earlier_pages_are_kept() {
        let fake = Fake::spawn(vec![
            page("io.example/a", Some("page2token")),
            Step::Stall,
            Step::Stall,
            page("io.example/b", None),
        ]);
        let client = RegistryClient::with_policy(&fake.base_url, fast_policy());
        let (result, pages) = collect_all(&client);
        let requests = fake.finish();

        let report = result.expect("two timeouts on one page must not abort the fetch");
        assert_eq!(requests.len(), 4, "1 request for page 1, 3 for page 2: {requests:#?}");
        assert!(!requests[0].contains("cursor="), "page 1 is requested exactly once, uncursored");
        for r in &requests[1..] {
            assert!(r.contains("cursor=page2token"), "every retry re-asks for page 2, not page 1: {r}");
        }
        assert_eq!(names(&pages), ["io.example/a", "io.example/b"], "no entry lost, duplicated or reordered");

        assert_eq!(report.pages_fetched, 2);
        assert_eq!(report.requests_sent, 4);
        assert_eq!(report.pages_retried, 1);
        assert_eq!(report.retries.len(), 2);
        assert!(report.retries.iter().all(|r| r.page == 2 && r.cause.contains("timeout")), "{:#?}", report.retries);
        assert_eq!(report.retries.iter().map(|r| r.failed_attempt).collect::<Vec<_>>(), [1, 2]);
        // Exponential: 10 ms after the first failure, 20 ms after the second.
        assert_eq!(report.retries[0].waited, Duration::from_millis(10));
        assert_eq!(report.retries[1].waited, Duration::from_millis(20));
    }

    #[test]
    fn a_503_is_retried_and_the_fetch_succeeds() {
        let fake = Fake::spawn(vec![status("503 Service Unavailable", &[]), page("io.example/a", None)]);
        let client = RegistryClient::with_policy(&fake.base_url, fast_policy());
        let (result, pages) = collect_all(&client);
        let requests = fake.finish();

        let report = result.expect("a 503 is transient");
        assert_eq!(requests.len(), 2, "{requests:#?}");
        assert_eq!(names(&pages), ["io.example/a"]);
        assert_eq!(report.retries.len(), 1);
        assert_eq!(report.retries[0].cause, "registry transport error: http status: 503");
        assert_eq!(report.retries[0].retry_after_secs, None);
        assert_eq!(report.retries[0].waited, Duration::from_millis(10), "no Retry-After: the backoff applies");
    }

    /// `Retry-After: 1` on a 429 lengthens the pause from the 10 ms backoff to the full second
    /// the server asked for — checked against both the report and the wall clock.
    #[test]
    fn a_429_retry_after_is_honoured() {
        let fake = Fake::spawn(vec![status("429 Too Many Requests", &[("Retry-After", "1")]), page("io.example/a", None)]);
        let client = RegistryClient::with_policy(&fake.base_url, fast_policy());
        let started = Instant::now();
        let (result, _) = collect_all(&client);
        let elapsed = started.elapsed();
        let requests = fake.finish();

        let report = result.expect("a 429 is transient");
        assert_eq!(requests.len(), 2, "{requests:#?}");
        assert_eq!(report.retries[0].retry_after_secs, Some(1));
        assert_eq!(report.retries[0].waited, Duration::from_secs(1));
        assert!(elapsed >= Duration::from_secs(1), "the client must actually wait: {elapsed:?}");
        assert!(report.retries[0].cause.contains("429"));
    }

    /// `Retry-After: 86400` is recorded as asked for but slept for only `max_wait`.
    #[test]
    fn an_absurd_retry_after_is_capped() {
        let policy = FetchPolicy { max_wait: Duration::from_millis(200), ..fast_policy() };
        let fake =
            Fake::spawn(vec![status("429 Too Many Requests", &[("Retry-After", "86400")]), page("io.example/a", None)]);
        let client = RegistryClient::with_policy(&fake.base_url, policy);
        let started = Instant::now();
        let (result, _) = collect_all(&client);
        let elapsed = started.elapsed();
        let requests = fake.finish();

        let report = result.expect("a 429 is transient");
        assert_eq!(requests.len(), 2, "{requests:#?}");
        assert_eq!(report.retries[0].retry_after_secs, Some(86400), "what was asked for is kept");
        assert_eq!(report.retries[0].waited, Duration::from_millis(200), "what was done is capped");
        assert!(elapsed < Duration::from_secs(5), "a day-long Retry-After must not be slept: {elapsed:?}");
    }

    /// The same cap, on the production policy: a day becomes 30 s. Pure, so no fake needed.
    #[test]
    fn the_production_policy_caps_retry_after_at_thirty_seconds() {
        let p = FetchPolicy::DEFAULT;
        assert_eq!(p.delay_before_retry(1, Some(Duration::from_secs(86_400))), Duration::from_secs(30));
        assert_eq!(p.delay_before_retry(1, Some(Duration::from_secs(u64::MAX))), Duration::from_secs(30));
        assert_eq!(p.delay_before_retry(1, Some(Duration::from_secs(10))), Duration::from_secs(10));
        // A Retry-After shorter than the backoff does not shorten it.
        assert_eq!(p.delay_before_retry(3, Some(Duration::ZERO)), Duration::from_secs(4));
        // The schedule itself: 1 s, 2 s, 4 s, and it can never exceed the cap however long it runs.
        let schedule: Vec<_> = (1..=3).map(|a| p.delay_before_retry(a, None)).collect();
        assert_eq!(schedule, [1, 2, 4].map(Duration::from_secs));
        assert_eq!(p.delay_before_retry(u32::MAX, None), Duration::from_secs(30));
    }

    /// A header that is not a plain delay-seconds integer — an HTTP-date, a negative number,
    /// one too large for `u64` — reads as absent, so the bounded backoff applies.
    #[test]
    fn an_unparseable_retry_after_falls_back_to_the_backoff() {
        for value in ["Wed, 21 Oct 2026 07:28:00 GMT", "-5", "1.5", "99999999999999999999999", ""] {
            let mut headers = ureq::http::HeaderMap::new();
            headers.insert("retry-after", value.parse().unwrap());
            assert_eq!(parse_retry_after(&headers), None, "{value:?}");
        }
        let mut headers = ureq::http::HeaderMap::new();
        headers.insert("retry-after", " 7 ".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(7));
    }

    /// A response that arrived and was wrong is a defect, not a hiccup: exactly one request.
    #[test]
    fn a_malformed_page_is_not_retried() {
        let fake = Fake::spawn(vec![Step::Respond(http("200 OK", &[], b"not json")), page("io.example/a", None)]);
        let client = RegistryClient::with_policy(&fake.base_url, fast_policy());
        let (result, pages) = collect_all(&client);
        let requests = fake.finish();

        assert_eq!(requests.len(), 1, "Malformed must cost exactly one request: {requests:#?}");
        assert!(matches!(result, Err(RegistryError::Malformed(_))), "returned as itself, unwrapped: {result:?}");
        assert!(pages.is_empty());
    }

    /// A 4xx other than 429 says the request itself is wrong; repeating it cannot help.
    #[test]
    fn a_404_is_not_retried() {
        let fake = Fake::spawn(vec![status("404 Not Found", &[("Retry-After", "1")]), page("io.example/a", None)]);
        let client = RegistryClient::with_policy(&fake.base_url, fast_policy());
        let (result, _) = collect_all(&client);
        let requests = fake.finish();

        assert_eq!(requests.len(), 1, "a 404 must cost exactly one request: {requests:#?}");
        match result {
            Err(RegistryError::Transport(msg)) => assert_eq!(msg, "http status: 404"),
            other => panic!("expected an unwrapped Transport error, got {other:?}"),
        }
    }

    /// A connection cut before a single response byte is a connection error, so it is retried.
    #[test]
    fn a_connection_dropped_before_any_response_is_retried() {
        let fake = Fake::spawn(vec![Step::Hangup, page("io.example/a", None)]);
        let client = RegistryClient::with_policy(&fake.base_url, fast_policy());
        let (result, pages) = collect_all(&client);
        let requests = fake.finish();

        let report = result.expect("a dropped connection is transient");
        assert_eq!(requests.len(), 2, "{requests:#?}");
        assert_eq!(names(&pages), ["io.example/a"]);
        assert_eq!(report.retries.len(), 1);
        // Classified as the hang-up it was, promptly — not left to the request timeout.
        assert!(!report.retries[0].cause.contains("timeout"), "{}", report.retries[0].cause);
    }

    /// A page that never recovers costs exactly `max_attempts_per_page` requests — the fifth
    /// scripted step, a good page, must never be requested — and then a clear error.
    #[test]
    fn a_persistently_failing_page_gives_up_after_exactly_the_bound() {
        let fake = Fake::spawn(vec![
            status("503 Service Unavailable", &[]),
            status("503 Service Unavailable", &[]),
            status("503 Service Unavailable", &[]),
            status("503 Service Unavailable", &[]),
            page("io.example/never-reached", None),
        ]);
        let client = RegistryClient::with_policy(&fake.base_url, fast_policy());
        let (result, pages) = collect_all(&client);
        let requests = fake.finish();

        assert_eq!(requests.len(), 4, "exactly max_attempts_per_page requests: {requests:#?}");
        assert!(pages.is_empty());
        let err = result.expect_err("a page that never recovers must fail the fetch");
        let RegistryError::RetriesExhausted { page, attempts, fetch_budget_spent, last } = &err else {
            panic!("expected RetriesExhausted, got {err:?}");
        };
        assert_eq!((*page, *attempts, *fetch_budget_spent), (1, 4, false));
        assert!(matches!(&**last, RegistryError::Transport(msg) if msg == "http status: 503"));
        assert_eq!(
            err.to_string(),
            "registry page 1 failed on all 4 attempts (the per-page retry bound); \
             last error: registry transport error: http status: 503"
        );
    }

    /// The fetch-wide budget: with 2 retries allowed, pages 1 and 2 each spend one, and page 3's
    /// first failure ends the fetch — 5 requests in all, not page 3's full 4 attempts.
    #[test]
    fn the_fetch_wide_retry_budget_bounds_a_struggling_registry() {
        let policy = FetchPolicy { max_retries_per_fetch: 2, ..fast_policy() };
        let fake = Fake::spawn(vec![
            status("503 Service Unavailable", &[]),
            page("io.example/a", Some("p2")),
            status("502 Bad Gateway", &[]),
            page("io.example/b", Some("p3")),
            status("503 Service Unavailable", &[]),
            page("io.example/never-reached", None),
        ]);
        let client = RegistryClient::with_policy(&fake.base_url, policy);
        let (result, pages) = collect_all(&client);
        let requests = fake.finish();

        assert_eq!(requests.len(), 5, "{requests:#?}");
        assert_eq!(names(&pages), ["io.example/a", "io.example/b"], "pages before the stop were delivered");
        match result {
            Err(RegistryError::RetriesExhausted { page: 3, attempts: 1, fetch_budget_spent: true, .. }) => {}
            other => panic!("expected the fetch budget to stop page 3, got {other:?}"),
        }
    }

    /// The live client's numbers, pinned, so the worst cases stated on
    /// [`FetchPolicy::DEFAULT`] are checked rather than asserted in prose.
    #[test]
    fn the_live_client_runs_under_the_default_policy() {
        let client = RegistryClient::new();
        assert_eq!(client.policy, FetchPolicy::DEFAULT);
        assert_eq!(client.agent.config().timeouts().global, Some(Duration::from_secs(30)));
        assert_eq!(RegistryClient::with_base_url("http://127.0.0.1:1").policy, FetchPolicy::DEFAULT);

        let p = FetchPolicy::DEFAULT;
        assert_eq!((p.max_attempts_per_page, p.max_retries_per_fetch), (4, 20));
        assert_eq!(p.worst_case_added_per_page(), Duration::from_secs(180));
        assert_eq!(p.worst_case_added_per_fetch(), Duration::from_secs(20 * 60));
    }

    /// The header block a sweep publishes: policy and accounting both present.
    #[test]
    fn the_fetch_report_renders_policy_and_accounting() {
        let fake = Fake::spawn(vec![status("503 Service Unavailable", &[]), page("io.example/a", None)]);
        let client = RegistryClient::with_policy(&fake.base_url, fast_policy());
        let (result, _) = collect_all(&client);
        fake.finish();

        let json = result.expect("fetch").to_json();
        assert_eq!(json["pages_fetched"], 1);
        assert_eq!(json["requests_sent"], 2);
        assert_eq!(json["pages_retried"], 1);
        assert_eq!(json["retries_total"], 1);
        assert_eq!(json["retries"][0]["page"], 1);
        assert_eq!(json["retries"][0]["waited_ms"], 10);
        assert_eq!(json["policy"]["max_attempts_per_page"], 4);
        assert_eq!(json["policy"]["request_timeout_ms"], 500);
    }
}
