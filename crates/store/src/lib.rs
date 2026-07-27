//! Persist evidence (content-addressed, immutable) and verdicts (derived, truncatable).
//!
//! **Must not:** Mutate or delete evidence.
//!
//! F-05 lands the evidence half: [`BlobStore`], a content-addressed local-filesystem blob
//! store. F-06 lands the metadata half: [`db`], the `SERVER`/`TOOL_SNAPSHOT`/`RUN`/...
//! `VERDICT` schema and its migration runner. The two are deliberately independent modules
//! — a `VERDICT` row and an `EVIDENCE` blob are both "storage", but they have opposite
//! mutability contracts, and nothing in this crate blurs that line.
//!
//! Contract: [architecture.md §3.1].

pub mod db;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use datamodel::Digest;
use sha2::{Digest as _, Sha256};

/// A content-addressed store for immutable evidence blobs, backed by the local filesystem.
///
/// architecture.md §12 item 4: stood up before any sandbox work exists, so Phase 1
/// evidence is replayable from day one. The object-store backend is deferred to P5-01;
/// this is deliberately just a directory.
pub struct BlobStore {
    root: PathBuf,
}

/// Why a [`BlobStore`] operation failed.
#[derive(Debug)]
pub enum StoreError {
    /// The requested digest has no blob on disk.
    NotFound(Digest),
    /// The bytes read back from disk do not hash to the digest that addresses them.
    ///
    /// This is the immutability guarantee made observable: content-addressing means the
    /// address *is* a claim about the content, and this is the store catching that claim
    /// having gone false out from under it — disk corruption, or something bypassing this
    /// API to write directly into the store root. It is never raised by correct use of
    /// [`BlobStore::put`] and [`BlobStore::get`] alone.
    Corrupt {
        /// The digest the caller addressed.
        addressed: Digest,
        /// The digest the on-disk bytes actually hash to.
        actual: Digest,
    },
    /// An underlying filesystem operation failed.
    Io(io::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(digest) => write!(f, "no blob for digest {digest}"),
            Self::Corrupt { addressed, actual } => write!(
                f,
                "blob at digest {addressed} actually hashes to {actual} — evidence store corrupt"
            ),
            Self::Io(e) => write!(f, "evidence store I/O error: {e}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::NotFound(_) | Self::Corrupt { .. } => None,
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

fn digest_of(bytes: &[u8]) -> Digest {
    let hash = Sha256::digest(bytes);
    Digest::from_bytes(hash.into())
}

impl BlobStore {
    /// Open (creating if necessary) a blob store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn path_for(&self, digest: &Digest) -> PathBuf {
        self.root.join(digest.to_string())
    }

    /// Write `bytes`, addressed by their digest.
    ///
    /// Re-`put`ting bytes already present under their digest is a no-op — verified by
    /// this method never opening the target path for writing when the existing on-disk
    /// content already matches, not merely by returning early with the same-looking
    /// result. If a digest's path already holds *different* bytes, that is the
    /// [`StoreError::Corrupt`] case: correct use of this API can never produce it, because
    /// the address is derived from the content being written, never chosen by the caller.
    pub fn put(&self, bytes: &[u8]) -> Result<Digest, StoreError> {
        let digest = digest_of(bytes);
        let path = self.path_for(&digest);

        match fs::read(&path) {
            Ok(existing) if existing == bytes => return Ok(digest),
            Ok(existing) => {
                return Err(StoreError::Corrupt {
                    addressed: digest,
                    actual: digest_of(&existing),
                });
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        // Write-to-temp-then-rename: a reader can never observe a partial blob, and a
        // crash mid-write leaves only an orphaned temp file, never a corrupt permanent one.
        let tmp_path = self.root.join(format!(".tmp-{digest}-{}", std::process::id()));
        fs::write(&tmp_path, bytes)?;
        fs::rename(&tmp_path, &path)?;
        Ok(digest)
    }

    /// Read back the bytes addressed by `digest`.
    ///
    /// Recomputes the digest of what was actually read before returning it, so a caller
    /// gets [`StoreError::Corrupt`] rather than silently-wrong bytes if the store root was
    /// tampered with outside this API.
    pub fn get(&self, digest: &Digest) -> Result<Vec<u8>, StoreError> {
        let path = self.path_for(digest);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(*digest));
            }
            Err(e) => return Err(e.into()),
        };

        let actual = digest_of(&bytes);
        if actual != *digest {
            return Err(StoreError::Corrupt {
                addressed: *digest,
                actual,
            });
        }
        Ok(bytes)
    }

    /// Whether a blob exists for `digest`, without reading or verifying its content.
    #[must_use]
    pub fn contains(&self, digest: &Digest) -> bool {
        self.path_for(digest).is_file()
    }

    /// The store's root directory, for tests and diagnostics.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (BlobStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = BlobStore::open(dir.path()).expect("open store");
        (store, dir)
    }

    #[test]
    fn put_then_get_is_byte_identical() {
        let (store, _dir) = temp_store();
        let digest = store.put(b"hello evidence").expect("put");
        let read_back = store.get(&digest).expect("get");
        assert_eq!(read_back, b"hello evidence");
    }

    #[test]
    fn digest_is_deterministic_over_content() {
        let (store, _dir) = temp_store();
        let d1 = store.put(b"same bytes").expect("put 1");
        let d2 = store.put(b"same bytes").expect("put 2");
        assert_eq!(d1, d2);
    }

    #[test]
    fn different_content_gets_different_digests() {
        let (store, _dir) = temp_store();
        let d1 = store.put(b"content a").expect("put a");
        let d2 = store.put(b"content b").expect("put b");
        assert_ne!(d1, d2);
    }

    #[test]
    fn get_of_unknown_digest_is_not_found() {
        let (store, _dir) = temp_store();
        let bogus = store.put(b"exists").expect("put");
        // A digest that was never put, but is well-formed, must read as NotFound.
        let never_put = datamodel::Digest::from_bytes([0u8; 32]);
        assert_ne!(bogus, never_put);
        match store.get(&never_put) {
            Err(StoreError::NotFound(d)) => assert_eq!(d, never_put),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// Re-`put`ting identical content must be a genuine no-op, not merely error-free.
    /// Proven by revoking write permission on the store root after the first `put`: if
    /// the second `put` tried to write anything at all, it would fail with a permission
    /// error instead of succeeding.
    #[test]
    fn reput_of_identical_content_performs_no_write() {
        use std::os::unix::fs::PermissionsExt;

        let (store, dir) = temp_store();
        let digest = store.put(b"immutable").expect("first put");

        let mut perms = fs::metadata(dir.path()).expect("stat dir").permissions();
        perms.set_mode(0o555); // read + execute, no write
        fs::set_permissions(dir.path(), perms.clone()).expect("lock dir");

        let result = store.put(b"immutable");

        // Always restore write permission before any assertion can panic and leak a
        // read-only temp dir past the test.
        perms.set_mode(0o755);
        fs::set_permissions(dir.path(), perms).expect("unlock dir");

        assert_eq!(result.expect("no-op put must still succeed"), digest);
    }

    #[test]
    fn tampered_blob_is_detected_on_get() {
        let (store, _dir) = temp_store();
        let digest = store.put(b"original").expect("put");

        fs::write(store.root().join(digest.to_string()), b"tampered").expect("tamper");

        match store.get(&digest) {
            Err(StoreError::Corrupt { addressed, .. }) => assert_eq!(addressed, digest),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn address_collision_on_put_is_rejected_not_silently_accepted() {
        let (store, _dir) = temp_store();
        let digest = digest_of(b"whatever ends up here");

        // Simulate external tampering: content sitting at a path before the API ever
        // wrote it there, under a digest that does not match that content.
        fs::write(store.root().join(digest.to_string()), b"pre-existing garbage")
            .expect("seed collision");

        match store.put(b"whatever ends up here") {
            Err(StoreError::Corrupt { addressed, .. }) => assert_eq!(addressed, digest),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn contains_reflects_presence_without_reading() {
        let (store, _dir) = temp_store();
        let digest = store.put(b"present").expect("put");
        assert!(store.contains(&digest));

        let absent = datamodel::Digest::from_bytes([0xffu8; 32]);
        assert_ne!(digest, absent);
        assert!(!store.contains(&absent));
    }
}
