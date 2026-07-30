//! The probe surface: MCP **resources**, read via `resources/list` + `resources/read`,
//! used to take a before/after snapshot of whatever state the server chooses to expose.
//!
//! This is the "weaker oracle" architecture.md §2 describes: it only sees state a server
//! decided to publish as a resource, never a server's full internal state the way a
//! kernel-level changeset (Class A) would. A `holds`/`violated` verdict built on it is real
//! evidence — correctly tagged `oracle = protocol_probe` — and must never be conflated with
//! the stronger oracle (B-03, `store::aggregate`).

use datamodel::Digest;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::client::{ProbeClient, ProbeError};

/// One resource the server listed, enough to read it back.
#[derive(Debug, Clone)]
pub struct ResourceRef {
    /// The resource's URI, as `resources/list` returned it.
    pub uri: String,
}

/// The JSON-RPC error code for "method not found" (JSON-RPC 2.0 spec §5.1) — how a server
/// says "I don't support `resources/list`" rather than returning an empty list.
const METHOD_NOT_FOUND: i64 = -32601;

/// Bounds both the number of requests this module makes against a third-party host and the
/// size of any one snapshot. A server publishing more than this many resources still gets
/// probed — just over a partial view of its exposed state, the same kind of narrowing this
/// whole oracle already accepts.
const MAX_RESOURCES: usize = 10;

/// Whether this server has anything for the probe to read.
#[derive(Debug, Clone)]
pub enum ProbeSurface {
    /// At least one resource was listed (capped at [`MAX_RESOURCES`]).
    Resources(Vec<ResourceRef>),
    /// `resources/list` is unsupported (JSON-RPC "method not found") or returned zero
    /// resources. There is nothing this module can probe.
    None,
}

/// Ask the server whether it exposes a usable probe surface at all.
///
/// A `resources/list` that errors with anything other than "method not found" is a real
/// failure ([`ProbeError`]), not evidence of an absent surface — the caller should
/// distinguish "this server has no resources" from "something went wrong talking to it."
///
/// # Errors
///
/// Returns [`ProbeError`] if `resources/list` fails with anything other than a "method not
/// found" JSON-RPC error.
pub fn discover_surface(client: &mut ProbeClient) -> Result<ProbeSurface, ProbeError> {
    match client.call("resources/list", serde_json::json!({})) {
        Ok(result) => {
            #[derive(Deserialize)]
            struct ListResult {
                #[serde(default)]
                resources: Vec<ResourceEntry>,
            }
            #[derive(Deserialize)]
            struct ResourceEntry {
                uri: String,
            }

            let parsed: ListResult = serde_json::from_value(result).map_err(|e| {
                ProbeError::Protocol(format!("resources/list result is not the expected shape: {e}"))
            })?;
            let refs: Vec<ResourceRef> = parsed
                .resources
                .into_iter()
                .take(MAX_RESOURCES)
                .map(|r| ResourceRef { uri: r.uri })
                .collect();
            if refs.is_empty() { Ok(ProbeSurface::None) } else { Ok(ProbeSurface::Resources(refs)) }
        }
        Err(ProbeError::ServerError { code, .. }) if code == METHOD_NOT_FOUND => Ok(ProbeSurface::None),
        Err(e) => Err(e),
    }
}

/// Read every resource in `surface` and fold the results into one digest — a snapshot of
/// exactly what this probe surface can see, at this moment.
///
/// Canonicalised by sorting on URI before hashing, so the *order* `resources/list` happens
/// to return is not itself part of the snapshot. That is a deliberate difference from
/// P0-02's metadata pin, which is order-sensitive on purpose because the thing being pinned
/// is one server response whose byte shape *is* the evidence; here the thing being hashed
/// is a set of independent state readings taken across several requests, and the harness's
/// own request order should not masquerade as a property of the server.
///
/// # Errors
///
/// Returns [`ProbeError`] on any transport or protocol failure reading a resource.
/// Requires `surface` to be [`ProbeSurface::Resources`] — a caller that already checked
/// [`ProbeSurface::None`] and called this anyway gets a [`ProbeError::Protocol`], not a
/// panic.
pub fn snapshot_state(client: &mut ProbeClient, surface: &ProbeSurface) -> Result<Digest, ProbeError> {
    let ProbeSurface::Resources(resources) = surface else {
        return Err(ProbeError::Protocol(
            "snapshot_state called with ProbeSurface::None — check the surface first".into(),
        ));
    };

    let mut readings: Vec<(String, Vec<u8>)> = Vec::with_capacity(resources.len());
    for r in resources {
        let result = client.call("resources/read", serde_json::json!({ "uri": r.uri }))?;
        readings.push((r.uri.clone(), canonical_bytes_of(&result)));
    }
    readings.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    for (uri, bytes) in &readings {
        hasher.update((uri.len() as u64).to_le_bytes());
        hasher.update(uri.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    Ok(Digest::from_bytes(hasher.finalize().into()))
}

/// `serde_json::Value`'s default map type is sorted (`preserve_order` is not enabled in
/// this workspace — see `Cargo.lock`), so `to_string()` over an already-parsed `Value` is
/// deterministic for a fixed value tree. That is a weaker property than P0-02's byte-exact
/// pin and is not meant to be the same one: a snapshot only needs *this reading equals that
/// reading*, not *these are the same bytes the server sent*.
fn canonical_bytes_of(result: &Value) -> Vec<u8> {
    result.to_string().into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

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

    /// Serves `responses` in order, one per accepted connection, forcing each envelope's
    /// `id` to match its position (0, 1, 2, ...) — the same sequence [`ProbeClient`]
    /// assigns its own outgoing requests. Templates only need to carry `result`/`error`;
    /// getting `id` right by construction here removes an entire class of test bugs where
    /// a hand-written literal `id` silently stops matching once a client makes more than
    /// one request.
    fn scripted_server(listener: TcpListener, responses: Vec<serde_json::Value>) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            for (i, mut response) in responses.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().expect("accept");
                read_one_http_request(&mut stream);
                response["id"] = serde_json::json!(i);
                write_json_response(&mut stream, &response);
            }
        })
    }

    #[test]
    fn discover_surface_finds_resources() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        let server = scripted_server(
            listener,
            vec![serde_json::json!({
                "jsonrpc": "2.0", "id": 0,
                "result": { "resources": [{"uri": "state://counter"}, {"uri": "state://log"}] }
            })],
        );

        let mut client = ProbeClient::new(endpoint, Duration::from_secs(5));
        let surface = discover_surface(&mut client).expect("discover_surface");
        match surface {
            ProbeSurface::Resources(refs) => assert_eq!(refs.len(), 2),
            ProbeSurface::None => panic!("expected Resources"),
        }
        server.join().expect("server thread");
    }

    #[test]
    fn discover_surface_treats_method_not_found_as_no_surface_not_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        let server = scripted_server(
            listener,
            vec![serde_json::json!({
                "jsonrpc": "2.0", "id": 0,
                "error": {"code": -32601, "message": "Method not found"}
            })],
        );

        let mut client = ProbeClient::new(endpoint, Duration::from_secs(5));
        let surface = discover_surface(&mut client).expect("discover_surface must not error");
        assert!(matches!(surface, ProbeSurface::None));
        server.join().expect("server thread");
    }

    #[test]
    fn discover_surface_treats_an_empty_list_as_no_surface() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        let server = scripted_server(
            listener,
            vec![serde_json::json!({"jsonrpc": "2.0", "id": 0, "result": {"resources": []}})],
        );

        let mut client = ProbeClient::new(endpoint, Duration::from_secs(5));
        let surface = discover_surface(&mut client).expect("discover_surface");
        assert!(matches!(surface, ProbeSurface::None));
        server.join().expect("server thread");
    }

    #[test]
    fn discover_surface_propagates_a_genuine_error_rather_than_hiding_it_as_no_surface() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        let server = scripted_server(
            listener,
            vec![serde_json::json!({
                "jsonrpc": "2.0", "id": 0,
                "error": {"code": -32603, "message": "Internal error"}
            })],
        );

        let mut client = ProbeClient::new(endpoint, Duration::from_secs(5));
        let err = discover_surface(&mut client).expect_err("a non-method-not-found error must propagate");
        assert!(matches!(err, ProbeError::ServerError { code: -32603, .. }));
        server.join().expect("server thread");
    }

    #[test]
    fn snapshot_state_is_stable_across_identical_reads() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        let reading = serde_json::json!({"jsonrpc": "2.0", "id": 0, "result": {"contents": [{"uri": "state://x", "text": "42"}]}});
        let server = scripted_server(listener, vec![reading.clone(), reading]);

        let mut client = ProbeClient::new(endpoint, Duration::from_secs(5));
        let surface = ProbeSurface::Resources(vec![ResourceRef { uri: "state://x".to_string() }]);
        let s1 = snapshot_state(&mut client, &surface).expect("snapshot 1");
        let s2 = snapshot_state(&mut client, &surface).expect("snapshot 2");
        assert_eq!(s1, s2, "identical resource content must snapshot identically");
        server.join().expect("server thread");
    }

    #[test]
    fn snapshot_state_changes_when_resource_content_changes() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        let before = serde_json::json!({"jsonrpc": "2.0", "id": 0, "result": {"contents": [{"uri": "state://x", "text": "42"}]}});
        let after = serde_json::json!({"jsonrpc": "2.0", "id": 0, "result": {"contents": [{"uri": "state://x", "text": "43"}]}});
        let server = scripted_server(listener, vec![before, after]);

        let mut client = ProbeClient::new(endpoint, Duration::from_secs(5));
        let surface = ProbeSurface::Resources(vec![ResourceRef { uri: "state://x".to_string() }]);
        let s1 = snapshot_state(&mut client, &surface).expect("snapshot 1");
        let s2 = snapshot_state(&mut client, &surface).expect("snapshot 2");
        assert_ne!(s1, s2, "different resource content must snapshot differently");
        server.join().expect("server thread");
    }

    #[test]
    fn snapshot_state_is_insensitive_to_resources_list_order() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        let a = serde_json::json!({"jsonrpc": "2.0", "id": 0, "result": {"text": "a"}});
        let b = serde_json::json!({"jsonrpc": "2.0", "id": 0, "result": {"text": "b"}});
        // Same two reads, forward and reversed order.
        let server = scripted_server(listener, vec![a.clone(), b.clone(), b, a]);

        let mut client = ProbeClient::new(endpoint, Duration::from_secs(5));
        let forward = ProbeSurface::Resources(vec![
            ResourceRef { uri: "state://a".to_string() },
            ResourceRef { uri: "state://b".to_string() },
        ]);
        let reversed = ProbeSurface::Resources(vec![
            ResourceRef { uri: "state://b".to_string() },
            ResourceRef { uri: "state://a".to_string() },
        ]);
        let s_forward = snapshot_state(&mut client, &forward).expect("forward");
        let s_reversed = snapshot_state(&mut client, &reversed).expect("reversed");
        assert_eq!(s_forward, s_reversed);
        server.join().expect("server thread");
    }
}
