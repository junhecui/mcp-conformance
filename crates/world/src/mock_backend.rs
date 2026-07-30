//! P3-03: mock backend redirection. When `sandbox::NetworkBridge` (P3-02) points a
//! network-isolated sandbox's every outbound connection at this module's listener instead of
//! `observe::connection_log`'s recording-only one, a tool calling what it believes is a real
//! external API is transparently served a generic, valid HTTP response instead of a refused
//! or hanging connection — architecture.md §4.4's "instrumented arm," the half of the
//! `openWorldHint` decision tree the strict arm alone (P3-01) cannot resolve on its own.
//!
//! **Must not:** classify a destination or decide what serving (or not serving) a request
//! means for a verdict — that is P3-04 (destination classification) and P3-05
//! (`openWorldHint`)'s job. This module only answers every request the same way, regardless
//! of who asked or what for.
//!
//! # Reuses this crate's own generic fixture content, not a second invented shape
//!
//! The response body below is exactly [`crate::generic_fixture_entries`]'s own seeded
//! `items` rows, serialised as JSON — the same three-row `(id, name, value)` seed
//! `SEED_SQL` already puts in the generic fixture's database. A tool exercising "some generic
//! external API" and a tool exercising "the generic seeded database" get the same underlying
//! generic content either way, rather than this crate maintaining two unrelated notions of
//! "generic."

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The generic mock's canned response body. See this module's own doc comment for why this
/// is exactly [`crate::generic_fixture_entries`]'s seed rows, not a second invented shape.
const RESPONSE_BODY: &str = "[\
{\"id\":1,\"name\":\"alpha\",\"value\":\"seed-value-1\"},\
{\"id\":2,\"name\":\"beta\",\"value\":\"seed-value-2\"},\
{\"id\":3,\"name\":\"gamma\",\"value\":\"seed-value-3\"}\
]";

/// A request is never allowed to make this module buffer unboundedly — a generic mock
/// answers the same way regardless of what was asked, so there is no reason to ever need
/// more than a bounded read of the request's headers.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Why starting the generic mock backend failed.
#[derive(Debug)]
pub struct MockBackendError(std::io::Error);

impl std::fmt::Display for MockBackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mock backend error: {}", self.0)
    }
}

impl std::error::Error for MockBackendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl From<std::io::Error> for MockBackendError {
    fn from(e: std::io::Error) -> Self {
        Self(e)
    }
}

/// A running generic mock HTTP backend. Bind with [`Self::start`], point
/// `sandbox::NetworkBridge::set_up`'s `proxy_port` at [`Self::port`], and [`Self::stop`] once
/// the run is over.
pub struct GenericMockBackend {
    port: u16,
    stop_flag: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl GenericMockBackend {
    /// Bind an OS-assigned port on every local interface (`0.0.0.0`) — see
    /// `observe::connection_log`'s own doc comment for why: `iptables REDIRECT` rewrites a
    /// redirected packet's destination to the primary address of the interface it arrived
    /// on, not to loopback — and start answering every connection on a background thread.
    ///
    /// # Errors
    ///
    /// Returns [`MockBackendError`] if binding the listener, or reading back its assigned
    /// local address, fails.
    pub fn start() -> Result<Self, MockBackendError> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;

        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop_flag = Arc::clone(&stop_flag);
        let thread = std::thread::spawn(move || loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    // A single request/response per connection is all a generic mock needs
                    // to promise — nothing in this codebase's own MCP-server test targets
                    // keep an outbound HTTP connection alive across multiple requests.
                    let _ = serve_one(stream);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if thread_stop_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        });

        Ok(Self { port, stop_flag, thread })
    }

    /// The port this backend actually bound — pass this to
    /// `sandbox::NetworkBridge::set_up`'s `proxy_port`.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Stop accepting new connections. Unlike `observe::connection_log::ConnectionLog::stop`,
    /// this needs no grace-drain period: a connection already accepted is served to
    /// completion synchronously by `serve_one` before the accept loop ever checks the stop
    /// flag again, so there is no "already past the kernel handshake but not yet served"
    /// window for a late connection to fall into.
    pub fn stop(self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        let _ = self.thread.join();
    }
}

/// Drain one HTTP request far enough to know the client has finished sending its headers,
/// then answer with the fixed generic response — a real HTTP/1.1 response a well-behaved
/// client can parse, not a bare payload dump.
fn serve_one(mut stream: TcpStream) -> std::io::Result<()> {
    // `accept()` on a non-blocking listener does not guarantee the returned stream inherits
    // that mode on every platform — set it explicitly rather than relying on inheritance,
    // with a bounded read timeout standing in for the "stop reading eventually" a nonblocking
    // loop would otherwise provide.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    let mut request = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        request.extend_from_slice(&buf[..n]);
        if request.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if request.len() >= MAX_REQUEST_BYTES {
            break;
        }
    }

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        RESPONSE_BODY.len(),
        RESPONSE_BODY,
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    /// Proves the mock actually speaks HTTP, not merely that a socket accepts bytes: a real
    /// `TcpStream` sends a real HTTP/1.1 GET request and gets back a response whose status
    /// line, `Content-Length`, and body are exactly what a well-behaved HTTP client expects
    /// to be able to parse.
    #[test]
    fn a_real_http_get_receives_the_generic_json_response() {
        let backend = GenericMockBackend::start().expect("start");
        let mut stream =
            TcpStream::connect((Ipv4Addr::LOCALHOST, backend.port())).expect("connect");
        stream
            .write_all(b"GET /whatever/path HTTP/1.1\r\nHost: example.invalid\r\n\r\n")
            .expect("write request");

        let mut reader = std::io::BufReader::new(&stream);
        let mut status_line = String::new();
        reader.read_line(&mut status_line).expect("read status line");
        assert_eq!(status_line.trim_end(), "HTTP/1.1 200 OK");

        let mut headers = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read header line");
            if line == "\r\n" || line.is_empty() {
                break;
            }
            headers.push_str(&line);
        }
        assert!(
            headers.contains(&format!("Content-Length: {}", RESPONSE_BODY.len())),
            "headers must declare the real body length: {headers:?}"
        );

        let mut body = String::new();
        reader.read_to_string(&mut body).expect("read body");
        assert_eq!(body, RESPONSE_BODY);

        backend.stop();
    }

    /// The exact generic content this module serves is the seeded fixture's own rows, not
    /// an unrelated invented shape — checked directly against the same seed values
    /// `generic_fixture_entries`'s own tests already verify are actually in the database.
    #[test]
    fn the_response_body_matches_the_generic_fixtures_seeded_items() {
        assert!(RESPONSE_BODY.contains("\"name\":\"alpha\""));
        assert!(RESPONSE_BODY.contains("\"name\":\"beta\""));
        assert!(RESPONSE_BODY.contains("\"name\":\"gamma\""));
    }

    /// Two independent connections must each get served in full — proves the accept loop
    /// actually loops rather than only ever answering the first connection.
    #[test]
    fn multiple_independent_connections_are_each_served() {
        let backend = GenericMockBackend::start().expect("start");

        for _ in 0..3 {
            let mut stream =
                TcpStream::connect((Ipv4Addr::LOCALHOST, backend.port())).expect("connect");
            stream.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").expect("write request");
            let mut response = String::new();
            stream.read_to_string(&mut response).expect("read response");
            assert!(response.ends_with(RESPONSE_BODY));
        }

        backend.stop();
    }
}
