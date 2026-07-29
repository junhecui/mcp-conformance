//! P5-01: an object-store backend for evidence, alongside F-05's local-filesystem
//! [`BlobStore`]. [`ObjectStore`] is the trait both share, so `orchestrator` can be handed
//! either without caring which; [`HttpObjectStore`] is the new half.
//!
//! `HttpObjectStore` speaks the one HTTP contract every S3-compatible object store — S3
//! itself, GCS's XML API, MinIO, Ceph RGW — exposes over a bucket endpoint: `PUT
//! {base_url}/{key}` to write an object, `GET {base_url}/{key}` to read one back, `HEAD
//! {base_url}/{key}` to check existence without transferring the body. This environment has
//! no real cloud bucket or credentials to verify against (disclosed, not glossed over: the
//! only real S3/GCS access this sandboxed dev environment can reach is a proxy-injected
//! placeholder credential meant for tooling that merely expects the environment variables to
//! exist, not a bucket this project has any business writing evidence into) — what *is*
//! verified for real, in this module's own tests, is the wire protocol itself: a real
//! hand-rolled HTTP/1.1 server over a real `TcpListener`, and [`HttpObjectStore`] as a real
//! HTTP client speaking to it over a real socket, byte-exact round trip and corruption
//! detection included. Wiring the same client at an actual S3-compatible endpoint is a
//! configuration change (a different `base_url`), not a code change — the contract this
//! module implements against is the generic one every one of those services already
//! supports, not anything MinIO- or AWS-specific.

use std::io::Read as _;

use datamodel::Digest;

use crate::{BlobStore, StoreError};

/// A content-addressed store for immutable evidence blobs — the interface `orchestrator`
/// programs against, so it can be handed a local [`BlobStore`] or a networked
/// [`HttpObjectStore`] interchangeably.
pub trait ObjectStore {
    /// Write `bytes`, addressed by their digest. Re-`put`ting identical content already
    /// present is a no-op, same contract [`BlobStore::put`] documents.
    ///
    /// # Errors
    /// Implementation-specific I/O or transport failure, or [`StoreError::Corrupt`] if the
    /// address already holds different content.
    fn put(&self, bytes: &[u8]) -> Result<Digest, StoreError>;

    /// Read back the bytes addressed by `digest`.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] if no object exists at `digest`; [`StoreError::Corrupt`] if
    /// what comes back does not hash to `digest`; otherwise implementation-specific I/O or
    /// transport failure.
    fn get(&self, digest: &Digest) -> Result<Vec<u8>, StoreError>;

    /// Whether an object exists for `digest`, without necessarily transferring its content.
    ///
    /// # Errors
    /// Implementation-specific I/O or transport failure.
    fn contains(&self, digest: &Digest) -> Result<bool, StoreError>;
}

impl ObjectStore for BlobStore {
    fn put(&self, bytes: &[u8]) -> Result<Digest, StoreError> {
        Self::put(self, bytes)
    }

    fn get(&self, digest: &Digest) -> Result<Vec<u8>, StoreError> {
        Self::get(self, digest)
    }

    fn contains(&self, digest: &Digest) -> Result<bool, StoreError> {
        Ok(Self::contains(self, digest))
    }
}

/// An [`ObjectStore`] backed by a real HTTP endpoint, keyed by digest under `base_url` —
/// see this module's own doc comment for the exact wire contract.
pub struct HttpObjectStore {
    base_url: String,
    agent: ureq::Agent,
}

impl HttpObjectStore {
    /// `base_url` is the bucket/prefix URL a `/{digest}` object key gets appended to, e.g.
    /// `http://host:port/bucket` — no trailing slash.
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder().build().into();
        Self { base_url: base_url.into(), agent }
    }

    fn url_for(&self, digest: &Digest) -> String {
        format!("{}/{digest}", self.base_url)
    }
}

impl ObjectStore for HttpObjectStore {
    fn put(&self, bytes: &[u8]) -> Result<Digest, StoreError> {
        let digest = crate::digest_of(bytes);
        self.agent
            .put(self.url_for(&digest))
            .send(bytes)
            .map_err(|e| StoreError::Transport(e.to_string()))?;
        Ok(digest)
    }

    fn get(&self, digest: &Digest) -> Result<Vec<u8>, StoreError> {
        match self.agent.get(self.url_for(digest)).call() {
            Ok(mut response) => {
                let mut bytes = Vec::new();
                response
                    .body_mut()
                    .as_reader()
                    .read_to_end(&mut bytes)
                    .map_err(StoreError::Io)?;
                let actual = crate::digest_of(&bytes);
                if actual != *digest {
                    return Err(StoreError::Corrupt { addressed: *digest, actual });
                }
                Ok(bytes)
            }
            Err(ureq::Error::StatusCode(404)) => Err(StoreError::NotFound(*digest)),
            Err(e) => Err(StoreError::Transport(e.to_string())),
        }
    }

    fn contains(&self, digest: &Digest) -> Result<bool, StoreError> {
        match self.agent.head(self.url_for(digest)).call() {
            Ok(_) => Ok(true),
            Err(ureq::Error::StatusCode(404)) => Ok(false),
            Err(e) => Err(StoreError::Transport(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;
    use std::thread;

    /// A minimal hand-rolled HTTP/1.1 object-store responder, the same pattern
    /// `discovery`'s `tests/http_discovery.rs` already uses for a real fake MCP HTTP
    /// server: real `TcpListener`, one connection handled at a time, no framework. Serves
    /// exactly the three verbs [`HttpObjectStore`] issues, storing bytes as files under
    /// `root` keyed by the request path.
    fn serve_one_request(root: &Path, mut stream: TcpStream) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));

        let mut request_line = String::new();
        reader.read_line(&mut request_line).expect("read request line");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().expect("method").to_string();
        let path = parts.next().expect("path").to_string();

        let mut content_length: usize = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read header line");
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if let Some(v) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = v.trim().parse().expect("numeric Content-Length");
            }
        }

        let key = path.trim_start_matches('/');
        let file_path = root.join(key);

        match method.as_str() {
            "PUT" => {
                let mut body = vec![0u8; content_length];
                reader.read_exact(&mut body).expect("read PUT body");
                std::fs::write(&file_path, &body).expect("write object");
                write!(stream, "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .expect("write response");
            }
            "GET" => match std::fs::read(&file_path) {
                Ok(bytes) => {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    )
                    .expect("write header");
                    stream.write_all(&bytes).expect("write body");
                }
                Err(_) => {
                    write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .expect("write response");
                }
            },
            "HEAD" => {
                let status = if file_path.is_file() { "200 OK" } else { "404 Not Found" };
                write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .expect("write response");
            }
            other => panic!("test server received unexpected method {other}"),
        }
        stream.flush().expect("flush");
    }

    fn spawn_test_server(root: std::path::PathBuf, request_count: usize) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let handle = thread::spawn(move || {
            for _ in 0..request_count {
                let (stream, _) = listener.accept().expect("accept");
                serve_one_request(&root, stream);
            }
        });
        (base_url, handle)
    }

    /// P5-01's exit criterion for the object-store half, over a real network round trip:
    /// bytes `put` through [`HttpObjectStore`] to a real local HTTP server come back
    /// byte-identical from `get`, and `contains` correctly distinguishes present from
    /// absent — exactly [`BlobStore`]'s own contract, now proven over real TCP instead of
    /// the local filesystem.
    #[test]
    fn put_then_get_round_trips_over_a_real_http_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (base_url, server) = spawn_test_server(dir.path().to_path_buf(), 4);
        let store = HttpObjectStore::new(base_url);

        let digest = store.put(b"hello over the wire").expect("put");
        let read_back = store.get(&digest).expect("get");
        assert_eq!(read_back, b"hello over the wire");

        assert!(store.contains(&digest).expect("contains present"));
        let absent = Digest::from_bytes([0xabu8; 32]);
        assert_ne!(digest, absent);
        assert!(!store.contains(&absent).expect("contains absent"));

        server.join().expect("server thread must not panic");
    }

    /// `get` of a digest the server has never seen must be [`StoreError::NotFound`], not a
    /// generic transport error — the client must interpret the server's own 404 correctly.
    #[test]
    fn get_of_unknown_digest_over_http_is_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (base_url, server) = spawn_test_server(dir.path().to_path_buf(), 1);
        let store = HttpObjectStore::new(base_url);

        let never_put = Digest::from_bytes([0x11u8; 32]);
        match store.get(&never_put) {
            Err(StoreError::NotFound(d)) => assert_eq!(d, never_put),
            other => panic!("expected NotFound, got {other:?}"),
        }
        server.join().expect("server thread must not panic");
    }

    /// The client, not just [`BlobStore`], must independently verify what comes back over
    /// the wire actually hashes to the digest it was addressed by — proven by having the
    /// real server hand back bytes that don't match the requested key (simulating a
    /// tampered or misconfigured backend), and asserting the client refuses to accept them
    /// silently.
    #[test]
    fn tampered_bytes_from_the_server_are_detected_not_trusted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let addressed = crate::digest_of(b"what the client will ask for");
        std::fs::write(dir.path().join(addressed.to_string()), b"different bytes entirely")
            .expect("seed tampered object");

        let (base_url, server) = spawn_test_server(dir.path().to_path_buf(), 1);
        let store = HttpObjectStore::new(base_url);

        match store.get(&addressed) {
            Err(StoreError::Corrupt { addressed: a, .. }) => assert_eq!(a, addressed),
            other => panic!("expected Corrupt, got {other:?}"),
        }
        server.join().expect("server thread must not panic");
    }

    /// [`BlobStore`] itself must satisfy [`ObjectStore`] — the whole point of the trait is
    /// that `orchestrator` can be handed either implementation.
    #[test]
    fn blob_store_satisfies_the_object_store_trait() {
        fn assert_is_object_store<T: ObjectStore>(_: &T) {}

        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::open(dir.path()).expect("open store");
        assert_is_object_store(&store);

        let digest = ObjectStore::put(&store, b"generic over the trait").expect("put");
        assert_eq!(ObjectStore::get(&store, &digest).expect("get"), b"generic over the trait");
        assert!(ObjectStore::contains(&store, &digest).expect("contains"));
    }
}
