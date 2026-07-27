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
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(msg) => write!(f, "registry transport error: {msg}"),
            Self::Malformed(msg) => write!(f, "registry response malformed: {msg}"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// A client for the registry's read-only listing endpoint.
pub struct RegistryClient {
    agent: ureq::Agent,
    base_url: String,
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
    #[must_use]
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        Self { agent, base_url: base_url.into() }
    }

    /// Fetch one page of the server listing.
    ///
    /// Always requests `version=latest`. Without it the registry returns every historical
    /// version of every server as a separate entry — found the hard way: an unfiltered
    /// first run produced 59,484 records for what turned out to be 18,664 distinct server
    /// names (some appearing over 1,000 times), a 3.2x inflation that would have corrupted
    /// every downstream count. There is no code path in this client that omits the filter.
    pub fn fetch_page(&self, cursor: Option<&str>, limit: u32) -> Result<Page, RegistryError> {
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
            .map_err(|e| RegistryError::Transport(e.to_string()))?;

        let bytes = response
            .body_mut()
            .read_to_vec()
            .map_err(|e| RegistryError::Transport(e.to_string()))?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|e| RegistryError::Malformed(format!("response is not valid UTF-8: {e}")))?;

        let parsed: ListResponse = serde_json::from_str(text)
            .map_err(|e| RegistryError::Malformed(format!("response is not the expected shape: {e}")))?;

        Ok(Page {
            entries_raw: parsed.servers.into_iter().map(|entry| entry.server.get().as_bytes().to_vec()).collect(),
            next_cursor: parsed.metadata.next_cursor,
        })
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
    pub fn fetch_all(
        &self,
        page_limit: u32,
        delay_between_pages: Duration,
        mut on_page: impl FnMut(&Page) -> bool,
    ) -> Result<(), RegistryError> {
        let mut cursor: Option<String> = None;
        loop {
            let page = self.fetch_page(cursor.as_deref(), page_limit)?;
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
        Ok(())
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
}
