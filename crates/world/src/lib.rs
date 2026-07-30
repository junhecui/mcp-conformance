//! Build a byte-reproducible base layer: seeded filesystem, seeded database, mock backends.
//!
//! **Must not:** Produce nondeterministic bases. This silently poisons every diff and the failure is invisible in the output.
//!
//! Contract: [architecture.md §3.1].
//!
//! # P2-05 scope: the generic fixture only
//!
//! Phase 2 lands exactly the `fixtures/generic` half of architecture.md §8's fixture split:
//! a seeded filesystem and a seeded database, both byte-reproducible across independent
//! constructions, with `server_id: NULL` per architecture.md §6's `FIXTURE` schema (a generic
//! fixture belongs to no particular server). `fixtures/per-server` bespoke fixtures remain
//! out of scope; mock-backend network redirection is [`mock_backend`], landed in P3-03.
//!
//! Reuses `sandbox::base_layer` rather than re-deriving base-layer construction and its
//! reproducibility proof a second time: a fixture *is* a base layer — a set of `EntrySpec`s —
//! and `sandbox::base_layer::build`/`capture`/`digest_of_capture` already prove exactly the
//! byte-reproducibility property this task's exit criterion asks for again, this time over
//! content that actually varies (real seed text, a real embedded `SQLite` database) rather than
//! P1-02's own placeholder sample tree.
//!
//! Linux-only, same as `sandbox` and `observe`: this crate exists only to build
//! `sandbox::base_layer` entries and would have nothing to offer on a target where `sandbox`
//! itself compiles to an empty crate.

#![cfg(target_os = "linux")]

pub mod mock_backend;

use sandbox::{EntryKind, EntrySpec};
use std::path::PathBuf;

/// Why building the generic fixture failed.
#[derive(Debug)]
pub enum FixtureError {
    /// The embedded `SQLite` seed database could not be constructed.
    Sqlite(rusqlite::Error),
    /// Reading back the constructed `SQLite` file's raw bytes failed.
    Io(std::io::Error),
}

impl std::fmt::Display for FixtureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "seeded-database error: {e}"),
            Self::Io(e) => write!(f, "I/O error reading back seeded database: {e}"),
        }
    }
}

impl std::error::Error for FixtureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            Self::Io(e) => Some(e),
        }
    }
}

impl From<rusqlite::Error> for FixtureError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

impl From<std::io::Error> for FixtureError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// The generic fixture's seeded-database schema and seed rows — fixed, not generated from
/// anything ambient (no timestamps, no random ids), so two independent constructions apply
/// the exact same statements in the exact same order.
const SEED_SQL: &str = "\
CREATE TABLE items (
    id    INTEGER PRIMARY KEY,
    name  TEXT NOT NULL,
    value TEXT NOT NULL
);
INSERT INTO items (id, name, value) VALUES
    (1, 'alpha', 'seed-value-1'),
    (2, 'beta',  'seed-value-2'),
    (3, 'gamma', 'seed-value-3');
";

/// Construct a fresh `SQLite` database applying [`SEED_SQL`] and return its raw file bytes.
///
/// Writes to a real temporary file rather than `:memory:` — `SQLite`'s raw page bytes are only
/// observable from an on-disk file, and this function's whole purpose is to hand those bytes
/// to [`sandbox::base_layer::EntryKind::File`] as the fixture's seeded-database content.
/// The temp file is created and removed by this function alone; nothing it does depends on
/// the path surviving past return.
fn seeded_database_bytes() -> Result<Vec<u8>, FixtureError> {
    let file = tempfile::NamedTempFile::new()?;
    let path = file.path().to_path_buf();
    // Drop the handle before rusqlite opens the same path — some SQLite operations are
    // pickier about pre-existing empty files than others, and this function wants SQLite's
    // own `Connection::open` to be the one thing that ever creates and writes this file.
    drop(file);
    let _ = std::fs::remove_file(&path);

    let conn = rusqlite::Connection::open(&path)?;
    conn.execute_batch(SEED_SQL)?;
    // Force a checkpoint of anything still buffered in SQLite's own connection-level cache
    // before this function's caller reads the file back from disk.
    conn.pragma_update(None, "journal_mode", "DELETE")?;
    drop(conn);

    let bytes = std::fs::read(&path)?;
    std::fs::remove_file(&path)?;
    Ok(bytes)
}

/// Where the generic fixture's seeded database lives inside the base layer, relative to its
/// root.
pub const SEEDED_DATABASE_PATH: &str = "data/fixtures.db";

/// Build the `fixtures/generic` entry set: a small seeded filesystem plus the seeded
/// database at [`SEEDED_DATABASE_PATH`], ready to pass straight to
/// [`sandbox::base_layer::build`].
///
/// Entries are returned pre-sorted ascending by raw path bytes, exactly as `build` requires
/// — this is the one property a caller must not have to re-derive by re-sorting output this
/// function already produced in the right order.
pub fn generic_fixture_entries() -> Result<Vec<EntrySpec>, FixtureError> {
    let database = seeded_database_bytes()?;
    Ok(vec![
        EntrySpec {
            path: PathBuf::from("README.txt"),
            kind: EntryKind::File(
                b"mcp-conformance generic fixture (P2-05).\n\
                  Seeded filesystem and database for tools with no bespoke fixture.\n"
                    .to_vec(),
            ),
            mode: 0o644,
        },
        EntrySpec { path: PathBuf::from("data"), kind: EntryKind::Directory, mode: 0o755 },
        EntrySpec {
            path: PathBuf::from(SEEDED_DATABASE_PATH),
            kind: EntryKind::File(database),
            mode: 0o644,
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This task's literal exit criterion: two independent constructions of the generic
    /// fixture (seeded FS *and* seeded database together) produce byte-identical base
    /// layers — proven the same way P1-02 proved it for a placeholder tree, this time over
    /// content that genuinely could vary (an embedded `SQLite` file) if anything ambient
    /// leaked into either the filesystem or the database construction.
    #[test]
    fn two_independent_constructions_of_the_generic_fixture_are_byte_identical() {
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let dir_b = tempfile::tempdir().expect("tempdir b");

        let entries_a = generic_fixture_entries().expect("build fixture entries a");
        let entries_b = generic_fixture_entries().expect("build fixture entries b");

        sandbox::build(dir_a.path(), &entries_a).expect("materialise a");
        sandbox::build(dir_b.path(), &entries_b).expect("materialise b");

        let capture_a =
            sandbox::capture(dir_a.path(), sandbox::InodeHandling::Zeroed).expect("capture a");
        let capture_b =
            sandbox::capture(dir_b.path(), sandbox::InodeHandling::Zeroed).expect("capture b");

        assert_eq!(
            capture_a, capture_b,
            "two independent constructions of the generic fixture must serialise identically"
        );
        assert_eq!(
            sandbox::digest_of_capture(&capture_a),
            sandbox::digest_of_capture(&capture_b)
        );
    }

    /// The seeded database specifically, isolated from the surrounding filesystem tree:
    /// two independent constructions of *just* the `SQLite` file must be byte-identical, not
    /// merely equivalent in queryable content. `SQLite`'s on-disk format has enough internal
    /// state (page layout, freelist, a file-change counter) that this is a real property to
    /// verify empirically rather than assume follows from "the same SQL ran twice."
    #[test]
    fn two_independent_seeded_databases_are_byte_identical() {
        let bytes_a = seeded_database_bytes().expect("build database a");
        let bytes_b = seeded_database_bytes().expect("build database b");
        assert_eq!(bytes_a, bytes_b, "two independently constructed seed databases must match byte-for-byte");
        assert!(!bytes_a.is_empty());
    }

    /// The seeded database is genuinely queryable, not just reproducible bytes with no
    /// content behind them — a fixture that failed to seed any rows would still pass the
    /// byte-reproducibility tests above if it failed the same way twice.
    #[test]
    fn the_seeded_database_is_queryable_and_contains_the_seed_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entries = generic_fixture_entries().expect("build fixture entries");
        sandbox::build(dir.path(), &entries).expect("materialise");

        let db_path = dir.path().join(SEEDED_DATABASE_PATH);
        let conn = rusqlite::Connection::open(&db_path).expect("open seeded database");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .expect("query seed rows");
        assert_eq!(count, 3, "the seeded database must contain exactly the three seed rows");
    }

    #[test]
    fn different_seed_sql_would_produce_a_different_capture() {
        // Not a test of `generic_fixture_entries` itself — a direct proof that this
        // module's byte-reproducibility test above is actually sensitive to database
        // content, not vacuously passing because `capture` never reads the file's bytes at
        // all.
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let dir_b = tempfile::tempdir().expect("tempdir b");

        let mut entries_a = generic_fixture_entries().expect("build fixture entries a");
        let mut entries_b = generic_fixture_entries().expect("build fixture entries b");
        if let EntryKind::File(content) = &mut entries_a[2].kind {
            content.push(0);
        }
        if let EntryKind::File(content) = &mut entries_b[2].kind {
            content.push(1);
        }

        sandbox::build(dir_a.path(), &entries_a).expect("materialise a");
        sandbox::build(dir_b.path(), &entries_b).expect("materialise b");

        let capture_a =
            sandbox::capture(dir_a.path(), sandbox::InodeHandling::Zeroed).expect("capture a");
        let capture_b =
            sandbox::capture(dir_b.path(), sandbox::InodeHandling::Zeroed).expect("capture b");
        assert_ne!(capture_a, capture_b);
    }
}
