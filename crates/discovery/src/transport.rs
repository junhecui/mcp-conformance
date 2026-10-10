//! Transport implementations. `pub(crate)` throughout — this is the structural half of
//! P0-01's "must not call any tool": [`Transport::call`] takes an arbitrary method string,
//! and nothing outside this crate can name the trait to reach it. [`crate::DiscoveryClient`]
//! is the only public door in, and it only ever calls `call`/`call_raw`/`notify` with the
//! four MCP method names literal in `client.rs` (`server/discover`, `initialize`,
//! `notifications/initialized`, `tools/list`).
//!
//! **P0-11 reworked how HTTP status codes are handled, and it is load-bearing.** The
//! `2026-07-28` dual-era algorithm requires a client to *read the body* of an HTTP 400 or
//! 404 to decide whether a server is modern or legacy, so the agent is built with
//! `http_status_as_error(false)`: `ureq::Error::StatusCode` carries only a number — no
//! response, no body (`ureq-3.3.0/src/error.rs:14`) — and therefore cannot satisfy that
//! requirement at all.
//!
//! That flag is agent-wide, so it changes every request, `tools/list` included. Left
//! unmanaged it would silently reclassify HTTP-level failures: a 403 or 500 body (usually
//! HTML) would reach [`decode_and_validate`] and surface as
//! [`crate::DiscoveryError::Protocol`] where it used to short-circuit as `Transport`, which
//! would break comparability with the July census split (435 `transport` against 312
//! `protocol`). So the status is captured explicitly and the failure taxonomy stays keyed on
//! it: every non-2xx status that is *not* era-signalling becomes
//! [`crate::DiscoveryError::HttpStatus`], carrying the number, and the two era-signalling
//! statuses (400, 404) are returned as data for [`crate::era`] to classify.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use serde_json::Value;

use crate::DiscoveryError;
use crate::jsonrpc::{self, ResponseEnvelope};

/// Identifies this harness to any HTTP server it discovers against — visible polling, not
/// anonymous, the same disclosure posture `intake::registry` uses against the registry
/// itself.
const USER_AGENT: &str = concat!(
    "mcp-conformance-harness/",
    env!("CARGO_PKG_VERSION"),
    " (annotation conformance research; read-only discovery; contact: see repository)"
);

/// Upper bound on one stdio JSON-RPC line. A `tools/list` response measured in megabytes
/// is already extraordinary; past this it is indistinguishable from a deliberate
/// memory-exhaustion attempt by a hostile server, and the read fails instead of buffering.
const MAX_LINE_BYTES: usize = 10 * 1024 * 1024;

/// Upper bound on one HTTP response body, stated explicitly rather than inherited.
///
/// `ureq`'s 10 MiB default applies to `Body::read_to_vec()` specifically;
/// `with_config().reader()` and `read_json()` are unbounded without an explicit `.limit()`,
/// per ureq's own documentation. Setting the limit here means the bound is visible in this
/// crate and survives a future switch to a different read method.
const MAX_BODY_BYTES: u64 = 10 * 1024 * 1024;

/// Upper bound on a `Retry-After` header value kept for reporting. A sweep uses it to skip
/// a host, never to sleep, so only its first few characters could ever matter.
const MAX_RETRY_AFTER_CHARS: usize = 64;

/// Upper bound on how many bytes of a spawned child's stderr are relayed to the harness's
/// own stderr. See [`relay_child_stderr`].
const MAX_CHILD_STDERR_BYTES: usize = 8 * 1024;

/// The two HTTP statuses whose *body* decides a server's era, per
/// `basic/transports/streamable-http.mdx` (revision `2026-07-28`): 400 for a rejected
/// request (which may or may not be a recognised modern error) and 404 for an unknown
/// method. Both are returned to the caller as data; every other non-2xx status is an
/// [`DiscoveryError::HttpStatus`] failure, because no era conclusion follows from it.
const ERA_SIGNALLING_STATUSES: [u16; 2] = [400, 404];

/// One raw JSON-RPC response, exactly as received, before any parsing.
///
/// P0-01: "the pin depends on this." These bytes ride untouched all the way out to
/// [`crate::Discovery`], alongside — never instead of — whatever gets parsed out of them
/// to route the response.
#[derive(Debug)]
pub(crate) struct RawResponse {
    pub bytes: Vec<u8>,
}

/// One request's outcome with **nothing interpreted**: the id that was sent, the HTTP
/// status if the transport has one, and the body as received.
///
/// Exists so [`crate::DiscoveryClient`] can classify a `server/discover` response that
/// [`decode_and_validate`] would (correctly) refuse — see [`crate::era`].
#[derive(Debug)]
pub(crate) struct RawOutcome {
    /// The JSON-RPC id this transport put on the request.
    pub request_id: u64,
    /// The HTTP status, or `None` for stdio, which has no such concept.
    pub status: Option<u16>,
    /// A sanitised `Retry-After`, if the server sent one.
    pub retry_after: Option<String>,
    /// The response body, exactly as received.
    pub bytes: Vec<u8>,
}

pub(crate) trait Transport {
    /// Send one request and return its outcome uninterpreted — see [`RawOutcome`].
    ///
    /// Errors are reserved for outcomes that carry no readable body: a connection-level
    /// failure, a non-era-signalling HTTP status, an SSE upgrade this transport does not
    /// speak, or a body that could not be read within [`MAX_BODY_BYTES`].
    fn call_raw(&mut self, method: &str, params: Value) -> Result<RawOutcome, DiscoveryError>;

    fn notify(&mut self, method: &str, params: Value) -> Result<(), DiscoveryError>;

    /// Set the HTTP header policy for subsequent requests: the value of
    /// `MCP-Protocol-Version` (`None` removes it), and whether to send `Mcp-Method`, which
    /// `2026-07-28` requires on every request and earlier revisions do not define.
    ///
    /// One call rather than two setters so the two can never disagree — modern headers
    /// without a version would get a spec-compliant server to answer `-32020`
    /// `HeaderMismatch`. A no-op for stdio, which has no headers.
    fn configure_http_headers(&mut self, _protocol_version: Option<&str>, _send_method: bool) {}

    /// Send one request and validate its envelope: the cooked counterpart to
    /// [`Self::call_raw`], used for every request whose response must be a well-formed
    /// JSON-RPC result before discovery proceeds.
    ///
    /// Provided, not implemented per transport, so the id-correlation and
    /// status-to-taxonomy rules cannot drift between stdio and HTTP.
    fn call(&mut self, method: &str, params: Value) -> Result<RawResponse, DiscoveryError> {
        let outcome = self.call_raw(method, params)?;
        if let Some(status) = outcome.status {
            if !(200..300).contains(&status) {
                // Keyed on the status, never on the body: this is the path that keeps
                // `transport` meaning "HTTP-level failure" after `http_status_as_error`
                // stopped doing it for us. See the module doc comment.
                return Err(DiscoveryError::HttpStatus { status, retry_after: outcome.retry_after });
            }
        }
        decode_and_validate(&outcome.bytes, outcome.request_id)
    }
}

/// Parse just enough of a response to route it, without discarding the bytes it came from.
/// Both transports funnel through this so id-mismatch and JSON-RPC-error handling live in
/// one place. A wrong or reused id is treated as a protocol violation, not tolerated —
/// discovery runs against a server the trust model assumes is actively hostile.
///
/// **Unchanged by P0-11, deliberately.** The integer-id requirement below is an
/// anti-hostile-server measure, and two real `2026-07-28`-era 4xx bodies violate it (a
/// substituted string id; a `null` id). Those bodies are classified by [`crate::era`],
/// which reads the body and ignores the id entirely, rather than by relaxing anything here.
fn decode_and_validate(bytes: &[u8], expected_id: u64) -> Result<RawResponse, DiscoveryError> {
    let envelope: ResponseEnvelope = serde_json::from_slice(bytes)
        .map_err(|e| DiscoveryError::Protocol(format!("response is not valid JSON-RPC: {e}")))?;

    let got_id = envelope
        .id
        .as_u64()
        .ok_or_else(|| DiscoveryError::Protocol("response id is missing or not an integer".into()))?;
    if got_id != expected_id {
        return Err(DiscoveryError::Protocol(format!(
            "response id {got_id} does not match request id {expected_id}"
        )));
    }

    if let Some(error) = envelope.error {
        return Err(DiscoveryError::ServerError { code: error.code, message: error.message });
    }
    if envelope.result.is_none() {
        return Err(DiscoveryError::Protocol("response has neither result nor error".into()));
    }

    Ok(RawResponse { bytes: bytes.to_vec() })
}

// ---- stdio ----

/// Newline-delimited JSON-RPC over a pair of byte streams (MCP's stdio framing: one
/// message per line, no embedded newlines).
///
/// Generic over `Read`/`Write` so tests can drive it over an in-process `UnixStream` pair
/// instead of a real subprocess; [`ChildProcessTransport`] below is what production code
/// actually constructs.
pub(crate) struct StdioTransport<R, W> {
    reader: BufReader<R>,
    writer: W,
    next_id: u64,
}

impl<R: Read, W: Write> StdioTransport<R, W> {
    pub(crate) fn new(reader: R, writer: W) -> Self {
        Self { reader: BufReader::new(reader), writer, next_id: 0 }
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), DiscoveryError> {
        debug_assert!(!bytes.contains(&b'\n'), "a JSON-RPC line must not embed a newline");
        self.writer.write_all(bytes).map_err(DiscoveryError::Io)?;
        self.writer.write_all(b"\n").map_err(DiscoveryError::Io)?;
        self.writer.flush().map_err(DiscoveryError::Io)
    }

    fn recv_line(&mut self) -> Result<Vec<u8>, DiscoveryError> {
        // Bounded read: the peer is assumed hostile (design.md §3), and an unbounded
        // `read_line` would buffer however many bytes it streams without a newline
        // straight into host memory. `Read::take` caps that at MAX_LINE_BYTES (+2 so a
        // response of exactly the cap may still terminate with `\r\n`), matching the
        // ~10 MiB bound the HTTP transport already gets from ureq's `read_to_vec` default.
        // `read_until` instead of `read_line` also drops the UTF-8 requirement — these
        // bytes are captured verbatim and validated as JSON downstream, not as a `String`.
        let mut line = Vec::new();
        let n = (&mut self.reader)
            .take(MAX_LINE_BYTES as u64 + 2)
            .read_until(b'\n', &mut line)
            .map_err(DiscoveryError::Io)?;
        if n == 0 {
            return Err(DiscoveryError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "stdio transport closed before a response was received",
            )));
        }
        while line.last().is_some_and(|&b| b == b'\n' || b == b'\r') {
            line.pop();
        }
        if line.len() > MAX_LINE_BYTES {
            return Err(DiscoveryError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("stdio response line exceeds the {MAX_LINE_BYTES}-byte cap"),
            )));
        }
        Ok(line)
    }
}

impl<R: Read, W: Write> Transport for StdioTransport<R, W> {
    fn call_raw(&mut self, method: &str, params: Value) -> Result<RawOutcome, DiscoveryError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&jsonrpc::encode_request(id, method, params))?;
        let bytes = self.recv_line()?;
        Ok(RawOutcome { request_id: id, status: None, retry_after: None, bytes })
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), DiscoveryError> {
        self.send(&jsonrpc::encode_notification(method, params))
    }
}

/// A [`StdioTransport`] over a spawned child process's stdio, owning the [`Child`] so it
/// gets reaped on drop instead of left as a zombie or orphan. Matters at census scale: a
/// discovery target that never exits on its own must not accumulate across a run of
/// thousands of servers.
pub(crate) struct ChildProcessTransport {
    child: Child,
    inner: StdioTransport<ChildStdout, ChildStdin>,
}

impl ChildProcessTransport {
    /// Spawn `program`, speak MCP over its stdio, and optionally impose a hard wall-clock
    /// deadline on the child's lifetime (`None` for no deadline).
    ///
    /// Exists for Stage 2 census (`docker run` wrapping a locally-launchable Class A
    /// package): a hostile or merely broken tool under `initialize`/`tools/list` can hang
    /// indefinitely, and `StdioTransport::recv_line` blocks with no timeout of its own. A
    /// detached watcher thread sleeps `timeout` and then sends the child a kill signal
    /// *unconditionally* — there is no liveness check and no way to cancel it, so it also
    /// fires long after a child that answered promptly has been reaped by `Drop`, where it
    /// harmlessly reports no such process. The killed process's stdout closing is what
    /// unblocks a pending `recv_line` (it surfaces as the ordinary "peer closed"
    /// `DiscoveryError::Io`, the same path an early-exiting server already takes).
    ///
    /// That unconditional kill is a known gap, not an oversight: the real fix is a shared
    /// "reaped" flag taken under one lock across {check, kill} and {set, wait}, so the kill
    /// cannot fire after `wait()`. It is deliberately deferred to a follow-up task and
    /// recorded under P0-10's deferred findings in `docs/tasks.md`.
    ///
    /// Best-effort by construction, not a containment mechanism: this is a watchdog for a
    /// hung *discovery* call, not the sandbox. If the child already exited before the
    /// deadline, the watcher's kill targets a pid the OS may since have reused — an accepted
    /// risk for a short (tens-of-seconds) timeout window in a census tool, not something
    /// Phase 1+'s actual containment may ever rely on.
    pub(crate) fn spawn_with_timeout(
        program: impl AsRef<std::ffi::OsStr>,
        args: &[&str],
        timeout: Option<Duration>,
    ) -> Result<Self, DiscoveryError> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Piped, never inherited and never `null`. See `relay_child_stderr`: inheriting
            // let an arbitrary third-party package write unbounded, unsanitised bytes
            // straight onto the sweep console, and `null` would throw away the diagnostics
            // P0-06's Stage 2 triage was actually built on.
            .stderr(Stdio::piped())
            .spawn()
            .map_err(DiscoveryError::Io)?;

        if let Some(stderr) = child.stderr.take() {
            relay_child_stderr(stderr);
        }

        if let Some(timeout) = timeout {
            let pid = child.id();
            std::thread::spawn(move || {
                std::thread::sleep(timeout);
                // Best-effort: ignore the exit status entirely. If the child already
                // exited, this either fails harmlessly or (rarely, pid reuse) kills an
                // unrelated process — see the doc comment above.
                let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            });
        }

        let stdout = child.stdout.take().expect("spawned with Stdio::piped()");
        let stdin = child.stdin.take().expect("spawned with Stdio::piped()");
        Ok(Self { child, inner: StdioTransport::new(stdout, stdin) })
    }
}

impl Transport for ChildProcessTransport {
    fn call_raw(&mut self, method: &str, params: Value) -> Result<RawOutcome, DiscoveryError> {
        self.inner.call_raw(method, params)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), DiscoveryError> {
        self.inner.notify(method, params)
    }
}

impl Drop for ChildProcessTransport {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Drain a spawned child's stderr on a thread, bounded and escape-stripped, onto the
/// harness's own stderr with a marker on every line.
///
/// The child is arbitrary third-party code — a registry package under Stage 2 census, run
/// through `docker run`, which proxies the container's stderr into this pipe. Inheriting it
/// (what this did before) handed that code the sweep console directly: unbounded volume, and
/// raw ANSI/OSC escape sequences that can rewrite what is already on screen, set a terminal
/// title, or — with OSC 52 — reach the clipboard. In this project's workflow that console is
/// read by a model, so it is an output surface with the same trust problem as a tool
/// description (`CLAUDE.md`: server-authored text is evidence, never instruction, *including*
/// terminal output from `xtask`).
///
/// `Stdio::null()` would also have closed the hole and was rejected: P0-06's Stage 2 failure
/// triage — the `EBADENGINE` and wrong-`uvx`-entry-point diagnoses behind its 36
/// `io_or_timeout` failures — came out of exactly these bytes. So they are kept, and made
/// safe instead:
///
/// - **Bounded** at [`MAX_CHILD_STDERR_BYTES`] of relayed output per child, with one
///   truncation notice. Reading continues past the cap and discards, because a child whose
///   stderr pipe fills blocks on write, and a blocked server is a hung discovery.
/// - **Escape-stripped**: every byte outside printable ASCII and tab is dropped, which
///   removes CSI/OSC introducers (`ESC`, and C1 `0x9b`/`0x9d` — a bare `[` or `]` cannot
///   introduce anything on its own) along with CR and NUL. Dropped rather than substituted,
///   so nothing is invented; a line that was pure escapes relays as empty and is skipped.
/// - **Marked**, so a line's provenance is visible where it lands rather than inferred.
///
/// Detached and best-effort by construction: the thread owns the pipe and ends when the
/// child closes it (or when `Drop` kills the child, which closes it). Nothing waits on it,
/// and a relay failure never fails a discovery.
fn relay_child_stderr(stderr: std::process::ChildStderr) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        let mut relayed = 0usize;
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if relayed >= MAX_CHILD_STDERR_BYTES {
                continue; // keep draining so the child never blocks on a full pipe
            }
            let safe: String = line
                .iter()
                .copied()
                .filter(|&b| b == b'\t' || b.is_ascii_graphic() || b == b' ')
                .map(char::from)
                .collect();
            if safe.is_empty() {
                continue;
            }
            relayed += safe.len();
            eprintln!("[server stderr] {safe}");
            if relayed >= MAX_CHILD_STDERR_BYTES {
                eprintln!(
                    "[server stderr] ... further output suppressed after \
                     {MAX_CHILD_STDERR_BYTES} bytes"
                );
            }
        }
    });
}

// ---- HTTP (Streamable HTTP, single-JSON-response case) ----

/// Streamable HTTP transport, limited to the non-streaming case: POST a JSON-RPC message,
/// get a `Content-Type: application/json` body back. A server that upgrades to
/// `text/event-stream` gets a clear [`DiscoveryError::Protocol`] rather than silent
/// mishandling — SSE support is out of scope for P0-01's thinnest path.
pub(crate) struct HttpTransport {
    endpoint: String,
    agent: ureq::Agent,
    next_id: u64,
    protocol_version: Option<String>,
    send_method_header: bool,
}

impl HttpTransport {
    pub(crate) fn new(endpoint: String) -> Self {
        Self::with_timeout(endpoint, Duration::from_secs(30))
    }

    pub(crate) fn with_timeout(endpoint: String, timeout: Duration) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            // Mandatory, not a preference: see the module doc comment. `Error::StatusCode`
            // carries only a number, so a 4xx body — which the `2026-07-28` dual-era
            // algorithm requires reading — is unreachable while this is true.
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            endpoint,
            agent,
            next_id: 0,
            protocol_version: None,
            send_method_header: false,
        }
    }

    fn post(
        &self,
        method: &str,
        body: &[u8],
    ) -> Result<ureq::http::Response<ureq::Body>, DiscoveryError> {
        let mut builder = self
            .agent
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            // The Streamable HTTP spec (2025-03-26) requires both content types here even
            // though this transport only handles the single-JSON-response case: "the client
            // MUST include an Accept header, listing both application/json and
            // text/event-stream." Advertising only application/json gets a spec-compliant
            // server to correctly reject the request with 406 — found running Stage 1
            // against live servers: 14 of 78 failures were exactly this, not a real
            // reachability problem. The scope exclusion is unaffected: if a server responds
            // with an SSE stream anyway, that is still rejected below, just no longer
            // provoked by an under-declared Accept header in the first place.
            .header("Accept", "application/json, text/event-stream")
            .header("User-Agent", USER_AGENT);
        if let Some(v) = &self.protocol_version {
            builder = builder.header("MCP-Protocol-Version", v);
        }
        if self.send_method_header {
            // Required on every request by `2026-07-28`, and must equal the body's method
            // or the server answers 400 `-32020`. `method` is always one of the literals in
            // `client.rs`, never caller-supplied — see that module's structural guarantee.
            builder = builder.header("Mcp-Method", method);
        }
        builder.send(body).map_err(|e| DiscoveryError::Transport(e.to_string()))
    }
}

/// A `Retry-After` value safe to carry into an error and a results file: present,
/// non-empty, printable ASCII, and short. A sweep uses it to *skip* a host rather than to
/// sleep, so nothing downstream parses it — this only has to be safe to store and print.
fn sanitised_retry_after(response: &ureq::http::Response<ureq::Body>) -> Option<String> {
    let raw = response.headers().get("retry-after")?.to_str().ok()?.trim();
    let usable = !raw.is_empty()
        && raw.chars().count() <= MAX_RETRY_AFTER_CHARS
        && raw.bytes().all(|b| b.is_ascii_graphic() || b == b' ');
    usable.then(|| raw.to_string())
}

impl Transport for HttpTransport {
    fn call_raw(&mut self, method: &str, params: Value) -> Result<RawOutcome, DiscoveryError> {
        let id = self.next_id;
        self.next_id += 1;
        let body = jsonrpc::encode_request(id, method, params);

        let mut response = self.post(method, &body)?;
        let status = response.status().as_u16();
        let retry_after = sanitised_retry_after(&response);

        // A status that decides nothing about the server's era is a failure keyed on the
        // status itself, and its body is never read: a 500's HTML page is of no use here,
        // and declining to read it is also the cheapest thing to do at sweep scale.
        if !(200..300).contains(&status) && !ERA_SIGNALLING_STATUSES.contains(&status) {
            return Err(DiscoveryError::HttpStatus { status, retry_after });
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if content_type.contains("text/event-stream") {
            return Err(DiscoveryError::Protocol(
                "server responded with an SSE stream; this transport handles only the \
                 single-JSON-response case of Streamable HTTP"
                    .into(),
            ));
        }

        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .read_to_vec()
            .map_err(|e| DiscoveryError::Transport(e.to_string()))?;
        Ok(RawOutcome { request_id: id, status: Some(status), retry_after, bytes })
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), DiscoveryError> {
        let body = jsonrpc::encode_notification(method, params);
        let response = self.post(method, &body)?;
        let status = response.status().as_u16();
        // Checked explicitly: with `http_status_as_error(false)` a rejected notification
        // would otherwise pass silently, where before P0-11 it failed discovery outright.
        if !(200..300).contains(&status) {
            return Err(DiscoveryError::HttpStatus {
                status,
                retry_after: sanitised_retry_after(&response),
            });
        }
        Ok(())
    }

    fn configure_http_headers(&mut self, protocol_version: Option<&str>, send_method: bool) {
        self.protocol_version = protocol_version.map(str::to_string);
        self.send_method_header = send_method;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_and_validate_accepts_a_matching_result() {
        let bytes = br#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#;
        let raw = decode_and_validate(bytes, 3).expect("must decode");
        assert_eq!(raw.bytes, bytes, "raw bytes must be returned untouched");
    }

    #[test]
    fn decode_and_validate_rejects_id_mismatch() {
        let bytes = br#"{"jsonrpc":"2.0","id":4,"result":{}}"#;
        let err = decode_and_validate(bytes, 3).expect_err("must reject");
        assert!(matches!(err, DiscoveryError::Protocol(_)));
    }

    #[test]
    fn decode_and_validate_surfaces_a_json_rpc_error() {
        let bytes = br#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"nope"}}"#;
        let err = decode_and_validate(bytes, 3).expect_err("must reject");
        match err {
            DiscoveryError::ServerError { code, message } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "nope");
            }
            other => panic!("expected ServerError, got {other:?}"),
        }
    }

    #[test]
    fn decode_and_validate_rejects_malformed_json() {
        let err = decode_and_validate(b"not json", 0).expect_err("must reject");
        assert!(matches!(err, DiscoveryError::Protocol(_)));
    }

    #[test]
    fn decode_and_validate_rejects_a_response_with_neither_result_nor_error() {
        let bytes = br#"{"jsonrpc":"2.0","id":0}"#;
        let err = decode_and_validate(bytes, 0).expect_err("must reject");
        assert!(matches!(err, DiscoveryError::Protocol(_)));
    }

    /// The two real 4xx bodies from the October re-survey's Appendix A, as evidence that
    /// `decode_and_validate` genuinely cannot be the era classifier: both are rejected on
    /// their `id` before their error code is ever looked at. P0-11's classifier lives in
    /// `crate::era` precisely because this check must stay strict.
    #[test]
    fn decode_and_validate_rejects_the_real_4xx_bodies_the_era_classifier_must_read() {
        for bytes in [
            &br#"{"jsonrpc":"2.0","id":"server-error","error":{"code":-32600,"message":"Bad Request"}}"#[..],
            &br#"{"jsonrpc":"2.0","error":{"code":-32000,"message":"Bad Request"},"id":null}"#[..],
        ] {
            let err = decode_and_validate(bytes, 0).expect_err("must reject a non-integer id");
            match err {
                DiscoveryError::Protocol(msg) => {
                    assert!(msg.contains("id"), "must fail on the id, got: {msg}");
                }
                other => panic!("expected Protocol, got {other:?}"),
            }
        }
    }

    /// Exercises the real framing (`send`/`recv_line`) over a genuine bidirectional OS
    /// pipe — a faithful stand-in for a stdio pipe, since both are just byte streams under
    /// `Read`/`Write`, without needing a full subprocess for a framing-level test.
    #[test]
    fn stdio_transport_round_trips_over_a_real_pipe() {
        use std::os::unix::net::UnixStream;
        use std::thread;

        let (client_side, server_side) = UnixStream::pair().expect("socket pair");
        let server = thread::spawn(move || {
            let mut reader = BufReader::new(server_side.try_clone().expect("clone"));
            let mut writer = server_side;
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");
            let request: Value = serde_json::from_str(&line).expect("valid JSON");
            let id = request["id"].clone();
            let response = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"echo": true}});
            writeln!(writer, "{response}").expect("write response");
        });

        let mut transport =
            StdioTransport::new(client_side.try_clone().expect("clone"), client_side);
        let raw = transport.call("ping", Value::Null).expect("call");
        let value: Value = serde_json::from_slice(&raw.bytes).expect("valid JSON");
        assert_eq!(value["result"]["echo"], true);

        server.join().expect("server thread must not panic");
    }

    /// stdio has no HTTP status, so `call_raw` must report `None` rather than inventing one
    /// — the era classifier keys the `-32601` branch on `Some(404)`, and a fabricated
    /// status would silently change the stdio verdict.
    #[test]
    fn stdio_call_raw_reports_no_http_status() {
        use std::os::unix::net::UnixStream;
        use std::thread;

        let (client_side, server_side) = UnixStream::pair().expect("socket pair");
        let server = thread::spawn(move || {
            let mut reader = BufReader::new(server_side.try_clone().expect("clone"));
            let mut writer = server_side;
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");
            writeln!(writer, r#"{{"jsonrpc":"2.0","id":0,"result":{{}}}}"#).expect("write");
        });

        let mut transport =
            StdioTransport::new(client_side.try_clone().expect("clone"), client_side);
        let outcome = transport.call_raw("ping", Value::Null).expect("call_raw");
        assert_eq!(outcome.status, None);
        assert_eq!(outcome.retry_after, None);
        assert_eq!(outcome.request_id, 0);
        server.join().expect("server thread must not panic");
    }

    /// A hostile peer streaming an over-cap "line" must produce a bounded error, not an
    /// unbounded buffer. The writer thread sends MAX_LINE_BYTES + 16 bytes with no newline
    /// and then closes; the reader must reject it at the cap.
    #[test]
    fn stdio_transport_rejects_a_line_exceeding_the_byte_cap() {
        use std::os::unix::net::UnixStream;
        use std::thread;

        let (client_side, server_side) = UnixStream::pair().expect("socket pair");
        let flooder = thread::spawn(move || {
            let mut writer = &server_side;
            let mut reader = BufReader::new(server_side.try_clone().expect("clone"));
            let mut request = String::new();
            reader.read_line(&mut request).expect("read request");
            let chunk = vec![b'a'; 64 * 1024];
            let mut sent = 0usize;
            while sent < MAX_LINE_BYTES + 16 {
                // A write error is the expected end state, not a failure: the reader stops
                // consuming at the cap and closes its end, so a blocked flood write gets
                // EPIPE. `expect`ing success here would deadlock — reader done, writer
                // blocked forever on a full socket buffer, test stuck in join().
                if writer.write_all(&chunk).is_err() {
                    break;
                }
                sent += chunk.len();
            }
            // No newline, ever — the connection just closes.
        });

        let mut transport =
            StdioTransport::new(client_side.try_clone().expect("clone"), client_side);
        let err = transport.call("ping", Value::Null).expect_err("flood must be rejected");
        assert!(matches!(err, DiscoveryError::Io(_)));
        // Close both fds so a flooder still blocked in write() is woken with EPIPE.
        drop(transport);
        flooder.join().expect("flooder thread");
    }

    #[test]
    fn stdio_transport_reports_an_error_when_the_peer_closes_without_responding() {
        use std::os::unix::net::UnixStream;

        let (client_side, server_side) = UnixStream::pair().expect("socket pair");
        drop(server_side); // simulate the server process exiting immediately

        let mut transport =
            StdioTransport::new(client_side.try_clone().expect("clone"), client_side);
        let err = transport.call("ping", Value::Null).expect_err("peer is gone");
        assert!(matches!(err, DiscoveryError::Io(_)));
    }
}
