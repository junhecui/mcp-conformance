//! Metadata DB (F-06): `SERVER`, `TOOL_SNAPSHOT`, `RUN`, `INTEGRITY`, `EVIDENCE`,
//! `VERDICT`, `RULESET`, `FIXTURE` — architecture.md §6.
//!
//! SQLite via `rusqlite`'s `bundled` feature, so the schema is reproducible across hosts
//! regardless of the system's installed SQLite version — the same posture ADR-007 already
//! applies to the Rust toolchain, and F-05 applies to the evidence blob store.
//!
//! Migrations are `include_str!`'d rather than read from disk at runtime: the harness must
//! apply the same schema regardless of where it's deployed from, and compiling them in
//! makes "the migration that ran" and "the migration in this build" the same file by
//! construction. A hand-rolled runner rather than a migration-framework dependency: with
//! one migration file today, a framework would be pure overhead, and the runner itself is
//! about 20 lines (ADR-007's general preference for hand-rolled over abstracted, applied at
//! a scale where it's actually cheap).

use rusqlite::Connection;

/// Migrations in application order. Each name is also the row recorded in
/// `schema_migrations` once applied, so re-ordering this array without renaming a file
/// would silently change what "already applied" means — don't.
const MIGRATIONS: &[(&str, &str)] = &[(
    "0001_initial_schema",
    include_str!("../migrations/0001_initial_schema.sql"),
)];

/// Open a metadata DB at `path` (or `":memory:"`) and apply any pending migrations.
///
/// Foreign keys are off by default in SQLite for backward-compatibility reasons that don't
/// apply here; this turns them on for every connection this function returns, since half
/// the point of this schema is the FK graph in architecture.md §6.
pub fn open_and_migrate(path: &str) -> rusqlite::Result<Connection> {
    let mut conn = Connection::open(path)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            name        TEXT PRIMARY KEY,
            applied_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
        );",
    )?;

    for (name, sql) in MIGRATIONS {
        let already_applied: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE name = ?1)",
            [name],
            |row| row.get(0),
        )?;
        if already_applied {
            continue;
        }

        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute("INSERT INTO schema_migrations (name) VALUES (?1)", [name])?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENTITIES: &[&str] = &[
        "server",
        "tool_snapshot",
        "run",
        "integrity",
        "evidence",
        "verdict",
        "ruleset",
        "fixture",
    ];

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .expect("prepare");
        stmt.query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("collect")
    }

    #[test]
    fn migration_creates_all_eight_entities() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        let names = table_names(&conn);
        for entity in ENTITIES {
            assert!(names.contains(&(*entity).to_string()), "missing table `{entity}`");
        }
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        let fk_on: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("read pragma");
        assert_eq!(fk_on, 1);

        // tool_snapshot.server_id must reference an existing server.
        let err = conn
            .execute(
                "INSERT INTO tool_snapshot
                 (snapshot_id, server_id, tool_name, metadata_pin, annotations_raw,
                  readonly_explicit, destructive_explicit, idempotent_explicit,
                  openworld_explicit, observed_at)
                 VALUES ('snap-1', 'no-such-server', 'tool', 'pin', '{}', 0, 0, 0, 0, 'now')",
                [],
            )
            .expect_err("FK violation must be rejected");
        assert!(format!("{err}").to_lowercase().contains("foreign key"));
    }

    /// Re-running migrations against the same on-disk DB must not error and must not
    /// re-apply the migration (proven via the schema_migrations row count, not just "no
    /// error" — an idempotent no-op and a silently-ignored duplicate-key error look the
    /// same from "it didn't crash" alone).
    #[test]
    fn reapplying_migrations_is_a_true_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("meta.sqlite3");
        let path_str = path.to_str().expect("utf8 path");

        open_and_migrate(path_str).expect("first open");
        let conn = open_and_migrate(path_str).expect("second open");

        let applied: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| row.get(0))
            .expect("count migrations");
        assert_eq!(applied, 1, "migration must be recorded exactly once");
    }

    /// Seeds a minimal, valid server -> tool_snapshot -> run chain so FK-dependent tests
    /// don't each have to repeat the setup.
    fn seed_run(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO server (server_id, source_uri, containability_class, spec_revision)
             VALUES ('srv-1', 'stdio://tool', 'A', '2026-06-18');

             INSERT INTO tool_snapshot
             (snapshot_id, server_id, tool_name, metadata_pin, annotations_raw,
              readonly_explicit, destructive_explicit, idempotent_explicit,
              openworld_explicit, observed_at)
             VALUES ('snap-1', 'srv-1', 'read_file', 'pin-abc', '{}', 1, 0, 1, 0, 'now');

             INSERT INTO run
             (run_id, snapshot_id, arm, arguments, harness_version, started_at)
             VALUES ('run-1', 'snap-1', 'arm0', '{}', '0.1.0', 'now');",
        )
        .expect("seed server/tool_snapshot/run");
    }

    #[test]
    fn verdict_reason_code_required_when_unverifiable() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        seed_run(&conn);

        let insert_unverifiable_without_reason = conn.execute(
            "INSERT INTO verdict
             (verdict_id, snapshot_id, annotation, declared, outcome, reason_code, oracle,
              protocol_version, derived_at)
             VALUES ('v-1', 'snap-1', 'readOnlyHint', 'true', 'unverifiable', NULL,
                     'kernel_changeset', '2026-06-18', 'now')",
            [],
        );
        assert!(
            insert_unverifiable_without_reason.is_err(),
            "unverifiable outcome without reason_code must violate the CHECK constraint"
        );

        conn.execute(
            "INSERT INTO verdict
             (verdict_id, snapshot_id, annotation, declared, outcome, reason_code, oracle,
              protocol_version, derived_at)
             VALUES ('v-2', 'snap-1', 'readOnlyHint', 'true', 'unverifiable', 'timeout',
                     'kernel_changeset', '2026-06-18', 'now')",
            [],
        )
        .expect("unverifiable with a reason_code must be accepted");

        conn.execute(
            "INSERT INTO verdict
             (verdict_id, snapshot_id, annotation, declared, outcome, reason_code, oracle,
              protocol_version, derived_at)
             VALUES ('v-3', 'snap-1', 'readOnlyHint', 'true', 'holds', NULL,
                     'kernel_changeset', '2026-06-18', 'now')",
            [],
        )
        .expect("holds without a reason_code must be accepted");
    }

    #[test]
    fn evidence_rows_reject_update_and_delete() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        seed_run(&conn);
        conn.execute(
            "INSERT INTO evidence (digest, run_id, kind, blob_ref)
             VALUES (?1, 'run-1', 'overlay_upper', 'blob-ref')",
            [&"a".repeat(64)],
        )
        .expect("insert evidence");

        let update_err = conn
            .execute(
                "UPDATE evidence SET kind = 'tampered' WHERE digest = ?1",
                [&"a".repeat(64)],
            )
            .expect_err("UPDATE on evidence must be rejected");
        assert!(format!("{update_err}").contains("immutable"));

        let delete_err = conn
            .execute("DELETE FROM evidence WHERE digest = ?1", [&"a".repeat(64)])
            .expect_err("DELETE on evidence must be rejected");
        assert!(format!("{delete_err}").contains("immutable"));
    }

    #[test]
    fn evidence_digest_must_look_like_a_blobstore_digest() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        seed_run(&conn);

        let wrong_length = conn.execute(
            "INSERT INTO evidence (digest, run_id, kind, blob_ref)
             VALUES ('too-short', 'run-1', 'overlay_upper', 'blob-ref')",
            [],
        );
        assert!(wrong_length.is_err(), "a non-64-character digest must be rejected");

        let uppercase = conn.execute(
            "INSERT INTO evidence (digest, run_id, kind, blob_ref)
             VALUES (?1, 'run-1', 'overlay_upper', 'blob-ref')",
            [&"A".repeat(64)],
        );
        assert!(uppercase.is_err(), "an uppercase digest must be rejected");
    }

    #[test]
    fn fixture_server_id_matches_its_kind() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        conn.execute(
            "INSERT INTO server (server_id, source_uri, containability_class, spec_revision)
             VALUES ('srv-1', 'stdio://tool', 'A', '2026-06-18')",
            [],
        )
        .expect("seed server");

        conn.execute(
            "INSERT INTO fixture (fixture_id, server_id, kind, content_digest, created_at)
             VALUES ('fx-generic', NULL, 'generic', ?1, 'now')",
            [&"b".repeat(64)],
        )
        .expect("generic fixture with no server_id must be accepted");

        conn.execute(
            "INSERT INTO fixture (fixture_id, server_id, kind, content_digest, created_at)
             VALUES ('fx-per-server', 'srv-1', 'per_server', ?1, 'now')",
            [&"c".repeat(64)],
        )
        .expect("per_server fixture with a server_id must be accepted");

        let generic_with_server = conn.execute(
            "INSERT INTO fixture (fixture_id, server_id, kind, content_digest, created_at)
             VALUES ('fx-bad-1', 'srv-1', 'generic', ?1, 'now')",
            [&"d".repeat(64)],
        );
        assert!(generic_with_server.is_err(), "generic fixture must not carry a server_id");

        let per_server_without_server = conn.execute(
            "INSERT INTO fixture (fixture_id, server_id, kind, content_digest, created_at)
             VALUES ('fx-bad-2', NULL, 'per_server', ?1, 'now')",
            [&"e".repeat(64)],
        );
        assert!(
            per_server_without_server.is_err(),
            "per_server fixture must carry a server_id"
        );
    }
}
