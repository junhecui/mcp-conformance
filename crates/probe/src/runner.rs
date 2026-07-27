//! Drives one full probe → invoke → probe (or probe → invoke → probe → invoke → probe) run
//! against a live server: owns the connection, calls into [`crate::snapshot`] for surface
//! discovery and state reading, [`crate::argsynth_min`] for the invocation's arguments, and
//! [`crate::protocol`] for the pure decision at the end.
//!
//! A failure to reach a decisive verdict is not necessarily a [`ProbeError`]: "no probe
//! surface" and "invocation failed" are *outcomes* ([`Outcome::Unverifiable`] with a reason
//! code), returned as `Ok`. [`ProbeError`] is reserved for genuine inability to run the
//! protocol at all — the connection dying mid-run, a malformed response to a call that
//! should have succeeded. That split matters to a caller doing a sweep over many servers:
//! `Ok(ProbeAssessment)` is always a record worth keeping, `Err` is a run that produced
//! nothing and should be logged as a failure, not silently coerced into a verdict either way.

use std::time::Duration;

use serde_json::Value;

use crate::argsynth_min::synthesize_arguments;
use crate::client::{ProbeClient, ProbeError};
use crate::protocol::{self, ProbeAssessment};
use crate::snapshot::{self, ProbeSurface};

/// Everything needed to run a probe against one tool on one live Class B server.
pub struct ProbeTarget<'a> {
    /// The server's Streamable HTTP endpoint.
    pub endpoint: &'a str,
    /// The tool under test, exactly as `tools/list` named it.
    pub tool_name: &'a str,
    /// The tool's declared `inputSchema`, for [`synthesize_arguments`].
    pub input_schema: &'a Value,
    /// Per-request timeout. Kept explicit rather than defaulted, the same posture
    /// `discovery::DiscoveryClient::http_with_timeout` takes for a sweep over many
    /// third-party hosts where a handful of slow ones must not dominate total run time.
    pub timeout: Duration,
}

/// Run the `readOnlyHint` protocol: probe, invoke once, probe again.
///
/// `declared` is the tool's effective declared value (see [`protocol::assess_read_only`]).
///
/// # Errors
///
/// [`ProbeError`] if the connection or the `initialize` handshake itself fails, or if
/// surface discovery / state reading fails for a reason other than "no surface." A failed
/// *invocation* is not an error here — it becomes `Ok(`[`protocol::invocation_failed`]`)`.
pub fn probe_read_only_hint(
    target: &ProbeTarget,
    declared: bool,
) -> Result<ProbeAssessment, ProbeError> {
    let mut client = ProbeClient::new(target.endpoint.to_string(), target.timeout);
    client.initialize()?;

    let surface = snapshot::discover_surface(&mut client)?;
    if matches!(surface, ProbeSurface::None) {
        return Ok(protocol::no_probe_surface());
    }

    let before = snapshot::snapshot_state(&mut client, &surface)?;

    let args = synthesize_arguments(target.input_schema);
    if invoke(&mut client, target.tool_name, args).is_err() {
        return Ok(protocol::invocation_failed());
    }

    let after = snapshot::snapshot_state(&mut client, &surface)?;
    Ok(protocol::assess_read_only(declared, before, after))
}

/// Run the `idempotentHint` protocol: invoke, probe, invoke again with identical
/// arguments, probe.
///
/// Same error/outcome split as [`probe_read_only_hint`].
pub fn probe_idempotent_hint(
    target: &ProbeTarget,
    declared: bool,
) -> Result<ProbeAssessment, ProbeError> {
    let mut client = ProbeClient::new(target.endpoint.to_string(), target.timeout);
    client.initialize()?;

    let surface = snapshot::discover_surface(&mut client)?;
    if matches!(surface, ProbeSurface::None) {
        return Ok(protocol::no_probe_surface());
    }

    let args = synthesize_arguments(target.input_schema);

    if invoke(&mut client, target.tool_name, args.clone()).is_err() {
        return Ok(protocol::invocation_failed());
    }
    let s1 = snapshot::snapshot_state(&mut client, &surface)?;

    if invoke(&mut client, target.tool_name, args).is_err() {
        return Ok(protocol::invocation_failed());
    }
    let s2 = snapshot::snapshot_state(&mut client, &surface)?;

    Ok(protocol::assess_idempotent(declared, s1, s2))
}

fn invoke(client: &mut ProbeClient, tool_name: &str, arguments: Value) -> Result<(), ProbeError> {
    client.call("tools/call", serde_json::json!({ "name": tool_name, "arguments": arguments }))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use datamodel::{Outcome, ReasonCode};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn read_one_http_request(stream: &mut TcpStream) {
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
                lower.strip_prefix("content-length:").map(|v| v.trim().parse().expect("numeric"))
            })
            .unwrap_or(0);
        let mut body = vec![0u8; content_length];
        stream.read_exact(&mut body).expect("read body");
    }

    fn write_json_response(stream: &mut TcpStream, body: &serde_json::Value) {
        let bytes = serde_json::to_vec(body).unwrap();
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            bytes.len()
        );
        stream.write_all(header.as_bytes()).expect("write header");
        stream.write_all(&bytes).expect("write body");
        stream.flush().expect("flush");
    }

    /// Serves `responses` in order, one per accepted connection, exactly as given — unlike
    /// `snapshot`'s test helper, ids are **not** auto-derived from connection order here,
    /// because [`ProbeClient`]'s notification request (`notifications/initialized`, sent
    /// from inside [`ProbeClient::initialize`]) consumes an HTTP connection but no id,
    /// which would desynchronise a naive "id = connection index" scheme the moment a real
    /// call follows it. Callers build each response with [`with_id`] using the id the
    /// client will actually send — see the sequence comment on each test below.
    fn scripted_server(listener: TcpListener, responses: Vec<serde_json::Value>) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept");
                read_one_http_request(&mut stream);
                write_json_response(&mut stream, &response);
            }
        })
    }

    fn with_id(mut v: serde_json::Value, id: u64) -> serde_json::Value {
        v["id"] = serde_json::json!(id);
        v
    }

    fn init_response(id: u64) -> serde_json::Value {
        with_id(
            serde_json::json!({"result": {"protocolVersion": "2025-11-25", "capabilities": {}, "serverInfo": {"name": "fake", "version": "0"}}}),
            id,
        )
    }
    /// The response to `notifications/initialized`'s HTTP POST. `ProbeClient::notify`
    /// never parses this body, so its content — including whether it carries an `id` at
    /// all — is irrelevant; it exists purely to complete the HTTP exchange.
    fn ack() -> serde_json::Value {
        serde_json::json!({})
    }
    fn resources_list(id: u64, uris: &[&str]) -> serde_json::Value {
        with_id(
            serde_json::json!({"result": {"resources": uris.iter().map(|u| serde_json::json!({"uri": u})).collect::<Vec<_>>()}}),
            id,
        )
    }
    fn resource_read(id: u64, text: &str) -> serde_json::Value {
        with_id(serde_json::json!({"result": {"contents": [{"text": text}]}}), id)
    }
    fn tool_call_ok(id: u64) -> serde_json::Value {
        with_id(serde_json::json!({"result": {"content": [], "isError": false}}), id)
    }
    fn tool_call_error(id: u64) -> serde_json::Value {
        with_id(serde_json::json!({"error": {"code": -32602, "message": "Invalid params"}}), id)
    }

    fn target<'a>(endpoint: &'a str, schema: &'a Value) -> ProbeTarget<'a> {
        ProbeTarget {
            endpoint,
            tool_name: "some_tool",
            input_schema: schema,
            timeout: Duration::from_secs(5),
        }
    }

    // Id sequence shared by every `probe_read_only_hint` test below, since
    // `ProbeClient`'s id counter is shared across the whole run: 0 = `initialize`
    // (`notifications/initialized` consumes an HTTP connection but no id — see
    // `scripted_server`'s doc comment), 1 = `resources/list`, 2 = `resources/read` (before),
    // 3 = `tools/call`, 4 = `resources/read` (after).

    #[test]
    fn read_only_hint_unchanged_state_holds_for_a_true_declaration() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        let server = scripted_server(
            listener,
            vec![
                init_response(0),
                ack(),
                resources_list(1, &["state://x"]),
                resource_read(2, "same"),
                tool_call_ok(3),
                resource_read(4, "same"),
            ],
        );

        let schema = serde_json::json!({"type": "object"});
        let result = probe_read_only_hint(&target(&endpoint, &schema), true).expect("probe");
        assert_eq!(result.outcome, Outcome::Holds);
        server.join().expect("server thread");
    }

    #[test]
    fn read_only_hint_changed_state_violates_a_true_declaration() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        let server = scripted_server(
            listener,
            vec![
                init_response(0),
                ack(),
                resources_list(1, &["state://x"]),
                resource_read(2, "before"),
                tool_call_ok(3),
                resource_read(4, "after"),
            ],
        );

        let schema = serde_json::json!({"type": "object"});
        let result = probe_read_only_hint(&target(&endpoint, &schema), true).expect("probe");
        assert_eq!(result.outcome, Outcome::Violated);
        server.join().expect("server thread");
    }

    #[test]
    fn read_only_hint_with_no_resources_is_unverifiable_no_probe_surface() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        // Only initialize + the notification ack + an empty resources/list: the run must
        // stop there — no tool invocation follows a surface that doesn't exist. If it did,
        // the listener would only have three scripted responses and the fourth accept
        // would hang, so this also proves invocation was skipped.
        let server = scripted_server(listener, vec![init_response(0), ack(), resources_list(1, &[])]);

        let schema = serde_json::json!({"type": "object"});
        let result = probe_read_only_hint(&target(&endpoint, &schema), true).expect("probe");
        assert_eq!(result.outcome, Outcome::Unverifiable);
        assert_eq!(result.reason, Some(ReasonCode("no_probe_surface".to_string())));
        server.join().expect("server thread");
    }

    #[test]
    fn read_only_hint_records_invocation_failure_as_unverifiable_not_a_hard_error() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        let server = scripted_server(
            listener,
            vec![
                init_response(0),
                ack(),
                resources_list(1, &["state://x"]),
                resource_read(2, "before"),
                tool_call_error(3),
            ],
        );

        let schema = serde_json::json!({"type": "object"});
        let result = probe_read_only_hint(&target(&endpoint, &schema), true).expect("probe must still return Ok");
        assert_eq!(result.outcome, Outcome::Unverifiable);
        assert_eq!(result.reason, Some(ReasonCode("invocation_failed".to_string())));
        server.join().expect("server thread");
    }

    // Id sequence for `probe_idempotent_hint`: 0 = `initialize`, 1 = `resources/list`,
    // 2 = `tools/call` (first), 3 = `resources/read` (s1), 4 = `tools/call` (second),
    // 5 = `resources/read` (s2).

    #[test]
    fn idempotent_hint_second_call_adds_nothing_holds_for_a_true_declaration() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        let server = scripted_server(
            listener,
            vec![
                init_response(0),
                ack(),
                resources_list(1, &["state://x"]),
                tool_call_ok(2),
                resource_read(3, "settled"),
                tool_call_ok(4),
                resource_read(5, "settled"),
            ],
        );

        let schema = serde_json::json!({"type": "object"});
        let result = probe_idempotent_hint(&target(&endpoint, &schema), true).expect("probe");
        assert_eq!(result.outcome, Outcome::Holds);
        server.join().expect("server thread");
    }

    #[test]
    fn idempotent_hint_second_call_changes_state_violates_a_true_declaration() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        let server = scripted_server(
            listener,
            vec![
                init_response(0),
                ack(),
                resources_list(1, &["state://x"]),
                tool_call_ok(2),
                resource_read(3, "after-first"),
                tool_call_ok(4),
                resource_read(5, "after-second"),
            ],
        );

        let schema = serde_json::json!({"type": "object"});
        let result = probe_idempotent_hint(&target(&endpoint, &schema), true).expect("probe");
        assert_eq!(result.outcome, Outcome::Violated);
        server.join().expect("server thread");
    }
}
