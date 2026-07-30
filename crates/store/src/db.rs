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

use rusqlite::{Connection, OptionalExtension};

/// Migrations in application order. Each name is also the row recorded in
/// `schema_migrations` once applied, so re-ordering this array without renaming a file
/// would silently change what "already applied" means — don't.
const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_initial_schema", include_str!("../migrations/0001_initial_schema.sql")),
    ("0002_run_queue", include_str!("../migrations/0002_run_queue.sql")),
    ("0003_evidence_synthetic_key", include_str!("../migrations/0003_evidence_synthetic_key.sql")),
];

/// Open a metadata DB at `path` (or `":memory:"`) and apply any pending migrations.
///
/// Foreign keys are off by default in SQLite for backward-compatibility reasons that don't
/// apply here; this turns them on for every connection this function returns, since half
/// the point of this schema is the FK graph in architecture.md §6.
///
/// Also sets a 5-second busy timeout: P5-01's `run_queue` is the first table in this schema
/// meant to be opened from several independent connections (one per worker-pool slot)
/// against the same on-disk file at once. SQLite's default is to fail a write immediately
/// with `SQLITE_BUSY` when another connection holds the write lock; a busy timeout makes a
/// second writer retry internally instead, which is what turns "two workers raced to lease
/// a job" into "one waits a few milliseconds," not a spurious error.
pub fn open_and_migrate(path: &str) -> rusqlite::Result<Connection> {
    let mut conn = Connection::open(path)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
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

// ---- Typed row insertion (B-02) ----
//
// Until this addition, every row this schema has ever received came from a `#[cfg(test)]`
// module writing raw SQL directly (see below). That was adequate while F-06's only job was
// proving the schema itself — foreign keys enforced, `CHECK`s firing, immutability holding.
// B-02's job is different: get a *real* verdict, produced by a real oracle (Track B's
// `probe` crate today; the Class A kernel-changeset path once P1-07 lands), into this
// table correctly. Hand-building `INSERT` strings at every call site that needs one is how
// a column silently drifts from what the `CHECK` constraints actually accept; these
// functions are the one place that mapping is allowed to live, using
// `datamodel::{Annotation, Oracle, Outcome}`'s `Display` impls so the TEXT written here can
// never fall out of sync with the enum it came from.

/// A `SERVER` row (architecture.md §6).
pub struct ServerRecord<'a> {
    /// Primary key.
    pub server_id: &'a str,
    /// Where this server was reached (a URL for Class B; an install/launch descriptor for
    /// Class A).
    pub source_uri: &'a str,
    /// `A`, `B`, or `Unclassifiable` (P0-04).
    pub containability_class: datamodel::ContainabilityClass,
    /// The MCP protocol revision negotiated during discovery.
    pub spec_revision: &'a str,
}

fn containability_class_db_str(class: datamodel::ContainabilityClass) -> &'static str {
    match class {
        datamodel::ContainabilityClass::A => "A",
        datamodel::ContainabilityClass::B => "B",
        datamodel::ContainabilityClass::Unclassifiable => "unclassifiable",
    }
}

/// Insert one `SERVER` row.
///
/// # Errors
///
/// Propagates any `rusqlite` error, including a `CHECK` violation on
/// `containability_class` (which cannot actually happen here, since
/// [`containability_class_db_str`] only ever emits one of the three values the constraint
/// accepts) or a duplicate `server_id`.
pub fn insert_server(conn: &Connection, record: &ServerRecord<'_>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO server (server_id, source_uri, containability_class, spec_revision)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            record.server_id,
            record.source_uri,
            containability_class_db_str(record.containability_class),
            record.spec_revision,
        ],
    )?;
    Ok(())
}

/// A `TOOL_SNAPSHOT` row (architecture.md §6) — one discovered-and-pinned tool.
pub struct ToolSnapshotRecord<'a> {
    /// Primary key.
    pub snapshot_id: &'a str,
    /// FK to `SERVER`.
    pub server_id: &'a str,
    /// The tool's name, as `tools/list` returned it.
    pub tool_name: &'a str,
    /// P0-02's metadata pin, rendered as its hex `Display` form.
    pub metadata_pin: &'a str,
    /// The tool's raw `annotations` object, or `"null"` if it had none — must be valid
    /// JSON per the schema's `json_valid` `CHECK`.
    pub annotations_raw: &'a str,
    /// Whether each annotation was explicitly declared (`true`) or defaulted/absent
    /// (`false`) — P0-05's coverage distinction collapsed to the single boolean this
    /// column shape wants; `Coverage::Defaulted` and `Coverage::Absent` are both `false`
    /// here; the two-way split lives in `census::coverage`, not in this table.
    pub readonly_explicit: bool,
    /// See `readonly_explicit`.
    pub destructive_explicit: bool,
    /// See `readonly_explicit`.
    pub idempotent_explicit: bool,
    /// See `readonly_explicit`.
    pub openworld_explicit: bool,
    /// When this snapshot was observed, as an ISO-8601-ish string — this schema stores
    /// timestamps as `TEXT`, matching every other `_at` column.
    pub observed_at: &'a str,
}

/// Insert one `TOOL_SNAPSHOT` row.
///
/// # Errors
///
/// Propagates any `rusqlite` error, including a foreign-key violation if `server_id` does
/// not reference an existing `SERVER` row, or a `CHECK` violation if `annotations_raw` is
/// not valid JSON.
pub fn insert_tool_snapshot(conn: &Connection, record: &ToolSnapshotRecord<'_>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO tool_snapshot
         (snapshot_id, server_id, tool_name, metadata_pin, annotations_raw,
          readonly_explicit, destructive_explicit, idempotent_explicit, openworld_explicit,
          observed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            record.snapshot_id,
            record.server_id,
            record.tool_name,
            record.metadata_pin,
            record.annotations_raw,
            record.readonly_explicit,
            record.destructive_explicit,
            record.idempotent_explicit,
            record.openworld_explicit,
            record.observed_at,
        ],
    )?;
    Ok(())
}

/// A `RULESET` row (architecture.md §6) — found missing, not merely unused, while wiring
/// P5-02's offline derivation batch job: `VERDICT.ruleset_version` is a `REFERENCES ruleset
/// (ruleset_version)` foreign key, and nothing before this task had ever written a `RULESET`
/// row, so no verdict naming a ruleset version could ever have been inserted at all. The
/// same shape of gap P4-03 closed for `RUN`/`INTEGRITY` and this same task closed for
/// `EVIDENCE`.
pub struct RulesetRecord<'a> {
    /// Primary key — e.g. `"v1"`.
    pub ruleset_version: &'a str,
    /// The ruleset's rules, as JSON text (the schema's own `CHECK` enforces validity). This
    /// crate has no opinion on the rules' shape; `orchestrator::load_ruleset`/`normalise`
    /// own that.
    pub rules: &'a str,
    /// When this ruleset version was published.
    pub published_at: &'a str,
}

/// Insert one `RULESET` row — a no-op if `ruleset_version` already exists (`INSERT OR
/// IGNORE`), since a ruleset version's rules are conceptually fixed once published: the
/// same version tag naming different rules would be the actual error, not something this
/// function should overwrite silently. A batch job re-deriving verdicts against a ruleset
/// it has already registered can call this unconditionally, every run, without erroring on
/// the second and subsequent calls.
///
/// # Errors
///
/// Propagates any `rusqlite` error, including the `CHECK` violation if `rules` is not valid
/// JSON.
pub fn insert_ruleset(conn: &Connection, record: &RulesetRecord<'_>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO ruleset (ruleset_version, rules, published_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![record.ruleset_version, record.rules, record.published_at],
    )?;
    Ok(())
}

/// A `VERDICT` row (architecture.md §6). `oracle` is mandatory and typed
/// ([`datamodel::Oracle`]) rather than a caller-supplied string — the one property B-02
/// exists to guarantee: every verdict this function can write carries a real oracle value,
/// never an empty or forgotten one.
pub struct VerdictRecord<'a> {
    /// Primary key.
    pub verdict_id: &'a str,
    /// FK to `TOOL_SNAPSHOT` — never `(server_id, tool_name)` (architecture.md §6
    /// invariant 1).
    pub snapshot_id: &'a str,
    /// Which of the four annotations this verdict assesses.
    pub annotation: datamodel::Annotation,
    /// The declared value this verdict is checking, as text (`"true"`/`"false"` for the
    /// boolean annotations) — kept as the caller's own rendering rather than re-deriving
    /// it, since "declared" already means different things across the deterministic and
    /// protocol-probe paths (explicit vs. effective-with-default).
    pub declared: &'a str,
    /// Holds, violated, or unverifiable.
    pub outcome: datamodel::Outcome,
    /// Mandatory whenever `outcome` is [`datamodel::Outcome::Unverifiable`] — the schema's
    /// own `CHECK` enforces this too (F-06), so a caller that gets it wrong fails loudly
    /// rather than silently.
    pub reason_code: Option<&'a str>,
    /// Which observation surface produced this verdict. B-02's whole point.
    pub oracle: datamodel::Oracle,
    /// FK to `RULESET`, when a normalisation ruleset was involved. `None` for the
    /// protocol-probe oracle, which has no changeset to normalise.
    pub ruleset_version: Option<&'a str>,
    /// The MCP protocol revision this verdict was derived under.
    pub protocol_version: &'a str,
    /// When this verdict was derived.
    pub derived_at: &'a str,
}

/// Insert one `VERDICT` row.
///
/// # Errors
///
/// Propagates any `rusqlite` error, including the `CHECK` that rejects an `unverifiable`
/// outcome with no `reason_code` (architecture.md §6 invariant 3) or a foreign-key
/// violation on `snapshot_id`.
pub fn insert_verdict(conn: &Connection, record: &VerdictRecord<'_>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO verdict
         (verdict_id, snapshot_id, annotation, declared, outcome, reason_code, oracle,
          ruleset_version, protocol_version, derived_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            record.verdict_id,
            record.snapshot_id,
            record.annotation.as_db_str(),
            record.declared,
            record.outcome.as_db_str(),
            record.reason_code,
            record.oracle.as_db_str(),
            record.ruleset_version,
            record.protocol_version,
            record.derived_at,
        ],
    )?;
    Ok(())
}

/// One verdict row read back out, with `annotation`/`outcome`/`oracle` parsed back into
/// their typed form via each enum's `from_db_str` — the inverse of [`insert_verdict`]'s
/// `as_db_str` writes. Feeds [`crate::aggregate`] (B-03).
pub struct VerdictRow {
    /// Which annotation this verdict assesses.
    pub annotation: datamodel::Annotation,
    /// Which oracle produced it.
    pub oracle: datamodel::Oracle,
    /// The outcome.
    pub outcome: datamodel::Outcome,
}

/// Read back every `VERDICT` row currently stored, for reporting (B-03).
///
/// # Errors
///
/// Propagates any `rusqlite` error. A stored value that doesn't round-trip through the
/// corresponding `from_db_str` (which should be impossible — [`insert_verdict`] is the only
/// writer, and it only ever writes `as_db_str` output) surfaces as
/// [`rusqlite::Error::InvalidColumnType`] rather than a panic or a silently-dropped row.
pub fn list_verdicts(conn: &Connection) -> rusqlite::Result<Vec<VerdictRow>> {
    let mut stmt = conn.prepare("SELECT annotation, oracle, outcome FROM verdict")?;
    stmt.query_map([], |row| {
        let annotation_str: String = row.get(0)?;
        let oracle_str: String = row.get(1)?;
        let outcome_str: String = row.get(2)?;
        let annotation = datamodel::Annotation::from_db_str(&annotation_str).ok_or_else(|| {
            rusqlite::Error::InvalidColumnType(0, "annotation".into(), rusqlite::types::Type::Text)
        })?;
        let oracle = datamodel::Oracle::from_db_str(&oracle_str).ok_or_else(|| {
            rusqlite::Error::InvalidColumnType(1, "oracle".into(), rusqlite::types::Type::Text)
        })?;
        let outcome = datamodel::Outcome::from_db_str(&outcome_str).ok_or_else(|| {
            rusqlite::Error::InvalidColumnType(2, "outcome".into(), rusqlite::types::Type::Text)
        })?;
        Ok(VerdictRow { annotation, oracle, outcome })
    })?
    .collect()
}

/// The two `TOOL_SNAPSHOT`/`SERVER` fields the offline derivation batch job (P5-02) needs
/// to re-derive a verdict for a snapshot: its raw annotations (to read the declared value
/// out of) and the spec revision its server negotiated (`VERDICT.protocol_version`).
pub struct SnapshotForVerdict {
    /// `TOOL_SNAPSHOT.annotations_raw`, exactly as stored — a caller parses the specific
    /// annotation it means to check out of this JSON itself; this crate has no opinion on
    /// which one.
    pub annotations_raw: String,
    /// `SERVER.spec_revision`, joined in because `TOOL_SNAPSHOT` itself carries no protocol
    /// version column of its own.
    pub spec_revision: String,
}

/// Read the fields [`SnapshotForVerdict`] needs, joined from `TOOL_SNAPSHOT` and its
/// `SERVER`, for `snapshot_id`. `None` if no such snapshot exists.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn get_snapshot_for_verdict(
    conn: &Connection,
    snapshot_id: &str,
) -> rusqlite::Result<Option<SnapshotForVerdict>> {
    conn.query_row(
        "SELECT tool_snapshot.annotations_raw, server.spec_revision
         FROM tool_snapshot
         JOIN server ON server.server_id = tool_snapshot.server_id
         WHERE tool_snapshot.snapshot_id = ?1",
        [snapshot_id],
        |row| Ok(SnapshotForVerdict { annotations_raw: row.get(0)?, spec_revision: row.get(1)? }),
    )
    .optional()
}

/// Delete every `VERDICT` row for exactly one `(annotation, oracle)` pair — the "safe to
/// truncate" half of architecture.md §6 invariant 2, scoped precisely rather than a blanket
/// wipe, so a batch job regenerating one annotation/oracle pair's verdicts can never
/// silently discard a different oracle's results (ADR-002, B-03's own cross-oracle guard).
/// Returns how many rows were deleted.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn delete_verdicts_by_annotation_and_oracle(
    conn: &Connection,
    annotation: datamodel::Annotation,
    oracle: datamodel::Oracle,
) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM verdict WHERE annotation = ?1 AND oracle = ?2",
        rusqlite::params![annotation.as_db_str(), oracle.as_db_str()],
    )
}

/// A `RUN` row (architecture.md §6) — one sandboxed execution. Exists as a typed helper
/// specifically because [`IntegrityRecord::run_id`] (P4-03) is a `NOT NULL REFERENCES
/// run (run_id)` foreign key: an `INTEGRITY` row cannot be written at all without a real
/// `RUN` row to point at, so this is the minimal prerequisite P4-03 needs, not scope creep
/// toward the rest of Phase 5's own persistence wiring.
pub struct RunRecord<'a> {
    /// Primary key.
    pub run_id: &'a str,
    /// FK to `TOOL_SNAPSHOT`.
    pub snapshot_id: &'a str,
    /// Which arm produced this run (`"1'"`, `"2"`, `"2R"`, ...).
    pub arm: &'a str,
    /// FK to `FIXTURE`, when one seeded this run. `None` for a run needing no fixture.
    pub fixture_id: Option<&'a str>,
    /// The arguments this run was invoked with, as a JSON text — must be valid JSON per the
    /// schema's own `json_valid` `CHECK`.
    pub arguments: &'a str,
    /// This harness's own version string.
    pub harness_version: &'a str,
    /// When this run started, as an ISO-8601-ish string.
    pub started_at: &'a str,
}

/// Insert one `RUN` row.
///
/// # Errors
///
/// Propagates any `rusqlite` error, including a foreign-key violation if `snapshot_id` or
/// `fixture_id` does not reference an existing row, or a `CHECK` violation if `arguments` is
/// not valid JSON.
pub fn insert_run(conn: &Connection, record: &RunRecord<'_>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO run (run_id, snapshot_id, arm, fixture_id, arguments, harness_version, started_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            record.run_id,
            record.snapshot_id,
            record.arm,
            record.fixture_id,
            record.arguments,
            record.harness_version,
            record.started_at,
        ],
    )?;
    Ok(())
}

/// An `INTEGRITY` row (architecture.md §6) — P4-03's own literal subject.
/// `adversarial_flag` is `sandbox`'s seccomp filter (P4-01) plus `observe::seccomp_audit`
/// (P4-02) made durable: whether an escape-class syscall was denied during this run, exactly
/// the value `integrity::GateOutcome::Accept::adversarial_flag` already computes in memory —
/// this is what makes that value survive past the process that computed it, so a
/// "published record" reading it back later reads the same thing the gate actually decided,
/// not a second, decoupled copy.
pub struct IntegrityRecord<'a> {
    /// Primary key and FK to `RUN`.
    pub run_id: &'a str,
    /// `G1`: whether the sandbox reported a clean teardown.
    pub clean_teardown: bool,
    /// `G2`/caps: whether the resource caps this run was subject to were respected (i.e.
    /// *not* hit — a cap hit is what `G2` gates on).
    pub caps_respected: bool,
    /// `G3`: whether the hard timeout fired.
    pub timed_out: bool,
    /// Every syscall number `observe::seccomp_audit` harvested for this run, as a JSON
    /// array — `"[]"` for a clean run, never omitted (the schema's own `NOT NULL` already
    /// enforces this, but the type here makes "no denials" a real empty list, not an absent
    /// column).
    pub denied_syscalls: &'a str,
    /// `G4`: whether an escape-class syscall was denied — `architecture.md §5.1`'s
    /// "accepted evidence, flagged" case when `true`.
    pub adversarial_flag: bool,
}

/// Insert one `INTEGRITY` row.
///
/// # Errors
///
/// Propagates any `rusqlite` error, including a foreign-key violation if `run_id` does not
/// reference an existing `RUN` row, or a `CHECK` violation if `denied_syscalls` is not valid
/// JSON.
pub fn insert_integrity(conn: &Connection, record: &IntegrityRecord<'_>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO integrity
         (run_id, clean_teardown, caps_respected, timed_out, denied_syscalls, adversarial_flag)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            record.run_id,
            record.clean_teardown,
            record.caps_respected,
            record.timed_out,
            record.denied_syscalls,
            record.adversarial_flag,
        ],
    )?;
    Ok(())
}

/// One `INTEGRITY` row read back out — the read half of [`insert_integrity`], used to prove
/// a published record's `adversarial_flag` traces back to what this table actually stored,
/// not an independent in-memory copy that could have silently drifted from it.
pub struct IntegrityRow {
    /// `G4`'s own flag, exactly as stored.
    pub adversarial_flag: bool,
    /// The denied-syscalls JSON array, exactly as stored (raw text — this crate has no
    /// reason to know what a syscall number means, only to store and return it faithfully).
    pub denied_syscalls: String,
}

/// Read back the `INTEGRITY` row for `run_id`, if one exists.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn get_integrity(conn: &Connection, run_id: &str) -> rusqlite::Result<Option<IntegrityRow>> {
    conn.query_row(
        "SELECT adversarial_flag, denied_syscalls FROM integrity WHERE run_id = ?1",
        [run_id],
        |row| {
            let adversarial_flag: bool = row.get(0)?;
            let denied_syscalls: String = row.get(1)?;
            Ok(IntegrityRow { adversarial_flag, denied_syscalls })
        },
    )
    .optional()
}

// ---- Evidence (P5-02) ----
//
// Until this addition, nothing in production code had ever written an `EVIDENCE` row
// either (the only prior `INSERT`s were raw SQL inside this module's own test module) —
// the same kind of gap P4-03 closed for `RUN`/`INTEGRITY`. Closing it here is what exposed
// the real `digest`-as-primary-key bug `0003_evidence_synthetic_key.sql` fixes: building
// the batch job that actually needs to write more than one run's worth of evidence is what
// surfaced it, not inspection of the schema alone.

/// An `EVIDENCE` row (architecture.md §6) — one piece of durable, content-addressed proof a
/// specific run produced. `digest` addresses the blob in a [`crate::BlobStore`] or
/// [`crate::object_store::ObjectStore`]; `blob_ref` is that store's own locator for it
/// (for a local `BlobStore` this is conventionally the digest's own string form again —
/// there is nothing else to point at — but a networked store might use a full URL, so this
/// crate does not assume the two are always equal).
pub struct EvidenceRecord<'a> {
    /// FK to `RUN`.
    pub run_id: &'a str,
    /// What kind of evidence this is (e.g. `"upper_layer"`). One row per `(run_id, kind)` —
    /// enforced by the schema's own `UNIQUE` constraint, not just convention.
    pub kind: &'a str,
    /// The content digest, as lowercase hex — must match [`crate::digest_of`]'s own output
    /// shape (the schema's `CHECK` enforces this).
    pub digest: &'a str,
    /// The backing store's own locator for this digest.
    pub blob_ref: &'a str,
}

/// Insert one `EVIDENCE` row.
///
/// # Errors
///
/// Propagates any `rusqlite` error, including a foreign-key violation if `run_id` does not
/// reference an existing `RUN` row, a `CHECK` violation if `digest` is not 64 lowercase hex
/// characters, or the `UNIQUE(run_id, kind)` violation if this run already has an evidence
/// row of this kind.
pub fn insert_evidence(conn: &Connection, record: &EvidenceRecord<'_>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO evidence (run_id, kind, digest, blob_ref) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![record.run_id, record.kind, record.digest, record.blob_ref],
    )?;
    Ok(())
}

/// One `EVIDENCE` row read back out, paired with the `RUN` it came from — what the offline
/// derivation batch job (P5-02) actually iterates over: every run that has evidence of
/// `kind`, regardless of which tool or server it belongs to.
pub struct EvidenceForRun {
    /// The `RUN` this evidence belongs to.
    pub run_id: String,
    /// FK to `TOOL_SNAPSHOT`, carried along so a caller doesn't need a second query per row
    /// just to look up which tool this run tested.
    pub snapshot_id: String,
    /// The evidence's own content digest.
    pub digest: String,
    /// The backing store's own locator for `digest`, exactly as [`insert_evidence`] stored
    /// it.
    pub blob_ref: String,
}

/// Every `EVIDENCE` row of `kind`, joined to its `RUN` for the `snapshot_id` a caller needs
/// to look up the tool it belongs to — the offline derivation batch job's own entry point
/// into "what is there to (re-)derive a verdict from."
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn list_evidence_by_kind(conn: &Connection, kind: &str) -> rusqlite::Result<Vec<EvidenceForRun>> {
    let mut stmt = conn.prepare(
        "SELECT run.run_id, run.snapshot_id, evidence.digest, evidence.blob_ref
         FROM evidence
         JOIN run ON run.run_id = evidence.run_id
         WHERE evidence.kind = ?1
         ORDER BY run.run_id",
    )?;
    stmt.query_map([kind], |row| {
        Ok(EvidenceForRun {
            run_id: row.get(0)?,
            snapshot_id: row.get(1)?,
            digest: row.get(2)?,
            blob_ref: row.get(3)?,
        })
    })?
    .collect()
}

// ---- Run queue (P5-01) ----
//
// The primitive a worker pool's slots lease jobs from. Every function here takes "now" and
// "lease deadline" as caller-supplied unix-epoch-second integers rather than calling a
// clock itself — this module has no opinion on time, matching the rest of this crate's
// posture of doing exactly what it's told with the values it's given (F-04's purity rule
// technically doesn't reach this crate, `store` is already impure by design, but the habit
// of not hiding a clock read inside a function that looks pure from its signature is worth
// keeping anyway).

/// One `run_queue` row leased for execution.
pub struct QueueJobRow {
    /// Primary key.
    pub job_id: i64,
    /// The opaque payload this crate was handed at `enqueue_job` time — this crate has no
    /// opinion on what it means; that is entirely the enqueuer's and the leaser's business.
    pub payload: String,
    /// How many times this job has now been leased, including this lease (starts at 1).
    pub attempts: i64,
}

/// Enqueue one job. `payload` must be valid JSON (the schema's own `CHECK` enforces this);
/// what it contains is opaque to this crate.
///
/// # Errors
///
/// Propagates any `rusqlite` error, including the `CHECK` violation if `payload` is not
/// valid JSON.
pub fn enqueue_job(conn: &Connection, payload: &str, created_at: &str) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO run_queue (payload, created_at) VALUES (?1, ?2)",
        rusqlite::params![payload, created_at],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Atomically lease one available job — `pending`, or `leased` with an expired
/// `leased_until` (a worker that took a job and never came back, whether crashed or merely
/// slow) — for `worker_id`, marking it `leased` with a new deadline of `lease_until_unix`.
///
/// Uses [`rusqlite::Transaction::new_unchecked`] with [`rusqlite::TransactionBehavior::
/// Immediate`] — deliberately not the default `DEFERRED` behavior
/// [`Connection::unchecked_transaction`] would give — so this function can still take
/// `&Connection` like every other function in this module (safe because each worker-pool
/// slot, this function's only caller, owns its connection exclusively; nothing in this
/// crate nests a second transaction inside this one), while avoiding a real, empirically
/// reproduced bug `IMMEDIATE` exists specifically to prevent: two `DEFERRED` transactions
/// that both read (acquiring a `SHARED` lock) before attempting to write race to *upgrade*
/// to a write lock, and SQLite fails that upgrade with `SQLITE_BUSY` outright rather than
/// retrying it — `busy_timeout` never gets a chance to help, because the race is over the
/// upgrade itself, not over acquiring an already-`RESERVED` lock. `IMMEDIATE` takes the
/// write lock up front, before the `SELECT`, so there is no upgrade left to race.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn lease_job(
    conn: &Connection,
    worker_id: &str,
    now_unix: i64,
    lease_until_unix: i64,
) -> rusqlite::Result<Option<QueueJobRow>> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let candidate: Option<(i64, String, i64)> = tx
        .query_row(
            "SELECT job_id, payload, attempts FROM run_queue
             WHERE status = 'pending' OR (status = 'leased' AND leased_until < ?1)
             ORDER BY job_id LIMIT 1",
            [now_unix],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;

    let Some((job_id, payload, attempts_before)) = candidate else {
        return Ok(None);
    };
    let attempts = attempts_before + 1;

    tx.execute(
        "UPDATE run_queue
         SET status = 'leased', leased_by = ?1, leased_until = ?2, attempts = ?3
         WHERE job_id = ?4",
        rusqlite::params![worker_id, lease_until_unix, attempts, job_id],
    )?;
    tx.commit()?;
    Ok(Some(QueueJobRow { job_id, payload, attempts }))
}

/// Mark a leased job `done`.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn complete_job(conn: &Connection, job_id: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE run_queue SET status = 'done' WHERE job_id = ?1", [job_id])?;
    Ok(())
}

/// Mark a leased job `failed` — terminal, not retried. A caller that wants retry-on-failure
/// semantics gets them for free by simply *not* calling this (an unmarked job's lease
/// expires and [`lease_job`] picks it up again); this function is for the case a job must
/// never be retried.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn fail_job(conn: &Connection, job_id: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE run_queue SET status = 'failed' WHERE job_id = ?1", [job_id])?;
    Ok(())
}

/// Count jobs that still need a worker slot's attention — `pending`, plus `leased` (even a
/// currently-live lease counts: it is still outstanding work, just not available to lease
/// again yet). A worker pool computes this once at start-up to know how many completions to
/// wait for.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn count_outstanding_jobs(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM run_queue WHERE status IN ('pending', 'leased')",
        [],
        |row| row.get(0),
    )
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
        assert_eq!(
            applied,
            i64::try_from(MIGRATIONS.len()).expect("migration count fits in i64"),
            "every migration in MIGRATIONS must be recorded exactly once, not zero and not twice"
        );
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

    /// B-02's exit criterion, taken literally: build a real chain through the typed
    /// helpers (not raw SQL) and confirm a verdict written with `Oracle::ProtocolProbe`
    /// round-trips as `protocol_probe`, distinct from a `KernelChangeset` verdict on a
    /// sibling snapshot.
    #[test]
    fn typed_insert_helpers_round_trip_the_oracle_correctly() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");

        insert_server(
            &conn,
            &ServerRecord {
                server_id: "srv-b",
                source_uri: "https://example.com/mcp",
                containability_class: datamodel::ContainabilityClass::B,
                spec_revision: "2025-11-25",
            },
        )
        .expect("insert Class B server");

        insert_tool_snapshot(
            &conn,
            &ToolSnapshotRecord {
                snapshot_id: "snap-b",
                server_id: "srv-b",
                tool_name: "get_status",
                metadata_pin: "deadbeef",
                annotations_raw: r#"{"readOnlyHint":true}"#,
                readonly_explicit: true,
                destructive_explicit: false,
                idempotent_explicit: false,
                openworld_explicit: false,
                observed_at: "2026-07-27T00:00:00Z",
            },
        )
        .expect("insert tool snapshot");

        insert_verdict(
            &conn,
            &VerdictRecord {
                verdict_id: "v-probe",
                snapshot_id: "snap-b",
                annotation: datamodel::Annotation::ReadOnlyHint,
                declared: "true",
                outcome: datamodel::Outcome::Holds,
                reason_code: None,
                oracle: datamodel::Oracle::ProtocolProbe,
                ruleset_version: None,
                protocol_version: "2025-11-25",
                derived_at: "2026-07-27T00:00:01Z",
            },
        )
        .expect("insert protocol-probe verdict");

        // A sibling Class A verdict on its own snapshot, tagged with the other oracle —
        // proves the two never collapse into one value on read-back.
        insert_server(
            &conn,
            &ServerRecord {
                server_id: "srv-a",
                source_uri: "stdio://local-tool",
                containability_class: datamodel::ContainabilityClass::A,
                spec_revision: "2025-11-25",
            },
        )
        .expect("insert Class A server");
        insert_tool_snapshot(
            &conn,
            &ToolSnapshotRecord {
                snapshot_id: "snap-a",
                server_id: "srv-a",
                tool_name: "read_file",
                metadata_pin: "cafebabe",
                annotations_raw: "null",
                readonly_explicit: false,
                destructive_explicit: false,
                idempotent_explicit: false,
                openworld_explicit: false,
                observed_at: "2026-07-27T00:00:00Z",
            },
        )
        .expect("insert tool snapshot");
        insert_verdict(
            &conn,
            &VerdictRecord {
                verdict_id: "v-kernel",
                snapshot_id: "snap-a",
                annotation: datamodel::Annotation::ReadOnlyHint,
                declared: "false",
                outcome: datamodel::Outcome::Violated,
                reason_code: None,
                oracle: datamodel::Oracle::KernelChangeset,
                // `ruleset_version` is an FK to `RULESET`; `None` here since this test's
                // focus is the oracle round-trip, not exercising the ruleset table too.
                ruleset_version: None,
                protocol_version: "2025-11-25",
                derived_at: "2026-07-27T00:00:01Z",
            },
        )
        .expect("insert kernel-changeset verdict");

        let rows = list_verdicts(&conn).expect("list_verdicts");
        assert_eq!(rows.len(), 2);
        let probe_row = rows.iter().find(|r| r.oracle == datamodel::Oracle::ProtocolProbe).expect("probe row present");
        assert_eq!(probe_row.annotation, datamodel::Annotation::ReadOnlyHint);
        assert_eq!(probe_row.outcome, datamodel::Outcome::Holds);
        let kernel_row = rows.iter().find(|r| r.oracle == datamodel::Oracle::KernelChangeset).expect("kernel row present");
        assert_eq!(kernel_row.outcome, datamodel::Outcome::Violated);

        // And directly against the raw column, since the whole point is the TEXT written
        // to disk, not just what comes back through the typed reader.
        let raw_oracle: String = conn
            .query_row("SELECT oracle FROM verdict WHERE verdict_id = 'v-probe'", [], |r| r.get(0))
            .expect("read raw oracle column");
        assert_eq!(raw_oracle, "protocol_probe");
    }

    /// [`insert_verdict`] must not bypass the schema's own invariant-3 `CHECK`
    /// (`unverifiable` requires a `reason_code`) just because it goes through a typed
    /// helper instead of raw SQL.
    #[test]
    fn insert_verdict_still_enforces_the_unverifiable_reason_code_check() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        seed_run(&conn);

        let err = insert_verdict(
            &conn,
            &VerdictRecord {
                verdict_id: "v-bad",
                snapshot_id: "snap-1",
                annotation: datamodel::Annotation::IdempotentHint,
                declared: "false",
                outcome: datamodel::Outcome::Unverifiable,
                reason_code: None,
                oracle: datamodel::Oracle::ProtocolProbe,
                ruleset_version: None,
                protocol_version: "2025-11-25",
                derived_at: "now",
            },
        )
        .expect_err("unverifiable without a reason_code must still violate the CHECK");
        assert!(format!("{err}").to_lowercase().contains("check"));
    }

    /// [`insert_run`]'s own round trip, through the typed helper rather than `seed_run`'s
    /// raw SQL — proves the helper itself writes a row the schema actually accepts.
    #[test]
    fn insert_run_writes_a_row_the_schema_accepts() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        conn.execute_batch(
            "INSERT INTO server (server_id, source_uri, containability_class, spec_revision)
             VALUES ('srv-1', 'stdio://tool', 'A', '2026-06-18');
             INSERT INTO tool_snapshot
             (snapshot_id, server_id, tool_name, metadata_pin, annotations_raw,
              readonly_explicit, destructive_explicit, idempotent_explicit,
              openworld_explicit, observed_at)
             VALUES ('snap-1', 'srv-1', 'read_file', 'pin-abc', '{}', 1, 0, 1, 0, 'now');",
        )
        .expect("seed server/tool_snapshot");

        insert_run(
            &conn,
            &RunRecord {
                run_id: "run-typed",
                snapshot_id: "snap-1",
                arm: "1'",
                fixture_id: None,
                arguments: r#"{"path":"/tmp/x"}"#,
                harness_version: "0.1.0",
                started_at: "2026-07-28T00:00:00Z",
            },
        )
        .expect("insert_run");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM run WHERE run_id = 'run-typed'", [], |r| r.get(0))
            .expect("count run row");
        assert_eq!(count, 1);
    }

    /// P4-03's own exit criterion, taken literally: `adversarial_flag` genuinely reaches the
    /// database (not just the schema definition — nothing wrote to `integrity` before this
    /// task), and reading it back via [`get_integrity`] returns exactly what
    /// [`insert_integrity`] wrote, not a value that quietly drifted in either direction.
    #[test]
    fn adversarial_flag_round_trips_through_the_integrity_table() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        seed_run(&conn);

        insert_integrity(
            &conn,
            &IntegrityRecord {
                run_id: "run-1",
                clean_teardown: true,
                caps_respected: true,
                timed_out: false,
                denied_syscalls: "[101, 165]",
                adversarial_flag: true,
            },
        )
        .expect("insert_integrity");

        let row = get_integrity(&conn, "run-1")
            .expect("get_integrity")
            .expect("a row was just inserted for run-1");
        assert!(row.adversarial_flag, "the flagged run must read back as flagged");
        assert_eq!(row.denied_syscalls, "[101, 165]");

        // And directly against the raw column, matching `typed_insert_helpers_round_trip_
        // the_oracle_correctly`'s own discipline: the whole point is the value written to
        // disk, not just what the typed reader happens to report.
        let raw_flag: i64 = conn
            .query_row("SELECT adversarial_flag FROM integrity WHERE run_id = 'run-1'", [], |r| r.get(0))
            .expect("read raw adversarial_flag column");
        assert_eq!(raw_flag, 1);
    }

    /// A clean run (no escape-class denial) must read back unflagged, with an empty denied-
    /// syscalls list — the common case, not just the flagged one.
    #[test]
    fn a_clean_runs_adversarial_flag_reads_back_false() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        seed_run(&conn);

        insert_integrity(
            &conn,
            &IntegrityRecord {
                run_id: "run-1",
                clean_teardown: true,
                caps_respected: true,
                timed_out: false,
                denied_syscalls: "[]",
                adversarial_flag: false,
            },
        )
        .expect("insert_integrity");

        let row = get_integrity(&conn, "run-1").expect("get_integrity").expect("row present");
        assert!(!row.adversarial_flag);
        assert_eq!(row.denied_syscalls, "[]");
    }

    /// [`get_integrity`] on a `run_id` with no `INTEGRITY` row must return `None`, never a
    /// default-valued row that could be mistaken for a genuinely clean, recorded run.
    #[test]
    fn get_integrity_for_a_run_with_no_row_is_none() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        seed_run(&conn);
        assert!(get_integrity(&conn, "run-1").expect("get_integrity").is_none());
    }

    /// [`insert_integrity`] must reject a `run_id` with no matching `RUN` row — the same
    /// foreign-key enforcement every other typed helper in this module already respects.
    #[test]
    fn insert_integrity_rejects_an_unknown_run_id() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        let err = insert_integrity(
            &conn,
            &IntegrityRecord {
                run_id: "no-such-run",
                clean_teardown: true,
                caps_respected: true,
                timed_out: false,
                denied_syscalls: "[]",
                adversarial_flag: false,
            },
        )
        .expect_err("a run_id with no RUN row must violate the foreign key");
        assert!(format!("{err}").to_lowercase().contains("foreign key"));
    }

    /// P5-01's own primitive, proven single-threaded first: enqueue, lease, complete —
    /// each transition reads back exactly as expected, and a completed job is no longer
    /// available to lease.
    #[test]
    fn a_job_can_be_enqueued_leased_and_completed() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        let job_id = enqueue_job(&conn, r#"{"tool":"read_file"}"#, "unix:0").expect("enqueue");

        let leased = lease_job(&conn, "worker-a", 100, 200)
            .expect("lease_job")
            .expect("a pending job must be leasable");
        assert_eq!(leased.job_id, job_id);
        assert_eq!(leased.payload, r#"{"tool":"read_file"}"#);
        assert_eq!(leased.attempts, 1);

        // Immediately re-leasing (lease still valid) must find nothing else to hand out.
        assert!(lease_job(&conn, "worker-b", 101, 201).expect("lease_job").is_none());

        complete_job(&conn, job_id).expect("complete_job");
        let status: String = conn
            .query_row("SELECT status FROM run_queue WHERE job_id = ?1", [job_id], |r| r.get(0))
            .expect("read status");
        assert_eq!(status, "done");

        // A completed job must never be leasable again, even long after any lease would
        // have expired.
        assert!(lease_job(&conn, "worker-c", 999_999, 1_000_000).expect("lease_job").is_none());
    }

    /// A job whose lease expired (the worker that held it crashed or hung) must become
    /// leasable again — the crash-recovery half of this primitive, not just the happy path.
    #[test]
    fn an_expired_lease_is_reclaimed() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        let job_id = enqueue_job(&conn, "{}", "unix:0").expect("enqueue");

        let first = lease_job(&conn, "worker-a", 100, 150)
            .expect("lease_job")
            .expect("must lease the fresh job");
        assert_eq!(first.attempts, 1);

        // `now` = 151 is past `leased_until` = 150: the lease has expired.
        let second = lease_job(&conn, "worker-b", 151, 300)
            .expect("lease_job")
            .expect("an expired lease must be reclaimable");
        assert_eq!(second.job_id, job_id);
        assert_eq!(second.attempts, 2, "reclaiming a lease must count as another attempt");
    }

    /// A job marked `failed` is terminal — never leasable again, unlike a merely-expired
    /// lease.
    #[test]
    fn a_failed_job_is_never_released() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        let job_id = enqueue_job(&conn, "{}", "unix:0").expect("enqueue");
        lease_job(&conn, "worker-a", 100, 150).expect("lease_job").expect("lease");
        fail_job(&conn, job_id).expect("fail_job");

        assert!(lease_job(&conn, "worker-b", 999_999, 1_000_000).expect("lease_job").is_none());
        let status: String = conn
            .query_row("SELECT status FROM run_queue WHERE job_id = ?1", [job_id], |r| r.get(0))
            .expect("read status");
        assert_eq!(status, "failed");
    }

    /// `count_outstanding_jobs` counts `pending` and `leased`, never `done` or `failed` —
    /// proven across all four states at once rather than one at a time, so a bug that
    /// happens to pass an individual-state check can't hide.
    #[test]
    fn outstanding_count_covers_pending_and_leased_only() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        let pending = enqueue_job(&conn, "{}", "unix:0").expect("enqueue pending");
        let leased = enqueue_job(&conn, "{}", "unix:0").expect("enqueue leased");
        let done = enqueue_job(&conn, "{}", "unix:0").expect("enqueue done");
        let failed = enqueue_job(&conn, "{}", "unix:0").expect("enqueue failed");

        lease_job(&conn, "worker-a", 0, 1000).expect("lease_job"); // leases `pending`
        lease_job(&conn, "worker-b", 0, 1000).expect("lease_job"); // leases `leased`
        complete_job(&conn, done).expect("complete_job");
        // `failed` was never leased before being failed — proves `fail_job` doesn't require
        // a prior lease to take effect.
        fail_job(&conn, failed).expect("fail_job");

        assert_eq!(count_outstanding_jobs(&conn).expect("count"), 2, "pending job {pending} and \
                    leased job {leased} outstanding; done job {done} and failed job {failed} not");
    }

    /// The mutual-exclusion property the whole worker pool depends on, proven against real
    /// concurrent SQLite connections rather than assumed from reading the SQL: many real OS
    /// threads, each with its own connection to the same on-disk database file (not
    /// `:memory:` — separate connections to `:memory:` are separate, isolated databases, so
    /// this specific race could only ever be observed against a real shared file), race to
    /// lease a fixed pool of jobs. Every job must be leased by exactly one thread; none may
    /// be leased twice, and none may be missed.
    #[test]
    fn concurrent_connections_never_double_lease_the_same_job() {
        use std::sync::{Arc, Mutex};
        use std::thread;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("queue.db");
        let db_path_str = db_path.to_str().expect("utf8 path").to_string();

        let job_count = 40;
        {
            let conn = open_and_migrate(&db_path_str).expect("open_and_migrate");
            for _ in 0..job_count {
                enqueue_job(&conn, "{}", "unix:0").expect("enqueue");
            }
        }

        let winners: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
        let threads: Vec<_> = (0..8)
            .map(|worker_index| {
                let db_path_str = db_path_str.clone();
                let winners = Arc::clone(&winners);
                thread::spawn(move || {
                    let conn = open_and_migrate(&db_path_str).expect("open_and_migrate");
                    let worker_id = format!("worker-{worker_index}");
                    let mut leased_here = Vec::new();
                    while let Some(job) =
                        lease_job(&conn, &worker_id, 0, 1_000_000_000).expect("lease_job")
                    {
                        leased_here.push(job.job_id);
                    }
                    winners.lock().expect("lock").extend(leased_here);
                })
            })
            .collect();

        for t in threads {
            t.join().expect("worker thread must not panic");
        }

        let mut leased_ids = Arc::try_unwrap(winners).expect("all threads joined").into_inner().expect("lock");
        leased_ids.sort_unstable();
        let mut expected: Vec<i64> = (1..=job_count).collect();
        expected.sort_unstable();
        assert_eq!(
            leased_ids, expected,
            "every job must be leased exactly once across all threads, no duplicates and none missed"
        );
    }

    /// [`insert_evidence`] and [`list_evidence_by_kind`] round trip — the ordinary path.
    #[test]
    fn evidence_can_be_inserted_and_listed_by_kind() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        seed_run(&conn);

        let digest = "b".repeat(64);
        insert_evidence(
            &conn,
            &EvidenceRecord { run_id: "run-1", kind: "upper_layer", digest: &digest, blob_ref: &digest },
        )
        .expect("insert_evidence");

        let rows = list_evidence_by_kind(&conn, "upper_layer").expect("list_evidence_by_kind");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].run_id, "run-1");
        assert_eq!(rows[0].snapshot_id, "snap-1");
        assert_eq!(rows[0].digest, digest);

        assert!(list_evidence_by_kind(&conn, "connection_log").expect("list other kind").is_empty());
    }

    /// The real bug `0003_evidence_synthetic_key.sql` fixes, proven directly rather than
    /// just trusting the migration comment: two *different* runs producing byte-identical
    /// evidence (the common case for a clean read-only tool — F-05's `BlobStore` dedupes the
    /// underlying blob, but each run still needs its own `EVIDENCE` row) must both be
    /// insertable at the same digest. Before this migration, the second insert failed with
    /// `UNIQUE constraint failed: evidence.digest`.
    #[test]
    fn two_different_runs_can_share_the_same_evidence_digest() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        seed_run(&conn);
        conn.execute(
            "INSERT INTO run (run_id, snapshot_id, arm, arguments, harness_version, started_at)
             VALUES ('run-2', 'snap-1', 'arm0', '{}', '0.1.0', 'now')",
            [],
        )
        .expect("seed second run");

        let shared_digest = "c".repeat(64);
        insert_evidence(
            &conn,
            &EvidenceRecord {
                run_id: "run-1",
                kind: "upper_layer",
                digest: &shared_digest,
                blob_ref: &shared_digest,
            },
        )
        .expect("insert evidence for run-1");
        insert_evidence(
            &conn,
            &EvidenceRecord {
                run_id: "run-2",
                kind: "upper_layer",
                digest: &shared_digest,
                blob_ref: &shared_digest,
            },
        )
        .expect("insert evidence for run-2 at the same digest must succeed");

        let rows = list_evidence_by_kind(&conn, "upper_layer").expect("list_evidence_by_kind");
        assert_eq!(rows.len(), 2, "both runs' evidence rows must be present");
        let run_ids: Vec<&str> = rows.iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(run_ids, vec!["run-1", "run-2"]);
        assert!(rows.iter().all(|r| r.digest == shared_digest));
    }

    /// A run may have at most one evidence row per kind — the schema's `UNIQUE(run_id,
    /// kind)` constraint, proven directly (a second `upper_layer` row for the same run must
    /// be rejected, while a different `kind` for that same run must still be accepted).
    #[test]
    fn a_run_may_have_only_one_evidence_row_per_kind() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        seed_run(&conn);
        let digest_a = "d".repeat(64);
        insert_evidence(
            &conn,
            &EvidenceRecord { run_id: "run-1", kind: "upper_layer", digest: &digest_a, blob_ref: &digest_a },
        )
        .expect("first upper_layer row");

        let digest_b = "e".repeat(64);
        let err = insert_evidence(
            &conn,
            &EvidenceRecord { run_id: "run-1", kind: "upper_layer", digest: &digest_b, blob_ref: &digest_b },
        )
        .expect_err("a second upper_layer row for the same run must be rejected");
        assert!(format!("{err}").to_lowercase().contains("unique"));

        insert_evidence(
            &conn,
            &EvidenceRecord {
                run_id: "run-1",
                kind: "connection_log",
                digest: &digest_b,
                blob_ref: &digest_b,
            },
        )
        .expect("a different kind for the same run must still be accepted");
    }

    /// The real gap `insert_ruleset` closes, reproduced directly before the fix existed:
    /// inserting a `VERDICT` naming a `ruleset_version` that has no `RULESET` row fails the
    /// foreign key, exactly as it should — proving the constraint is real, not merely
    /// declared.
    #[test]
    fn a_verdict_naming_an_unregistered_ruleset_version_violates_the_foreign_key() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        seed_run(&conn);

        let err = conn
            .execute(
                "INSERT INTO verdict
                 (verdict_id, snapshot_id, annotation, declared, outcome, reason_code, oracle,
                  ruleset_version, protocol_version, derived_at)
                 VALUES ('v-1', 'snap-1', 'readOnlyHint', 'true', 'holds', NULL,
                         'kernel_changeset', 'v1', '2026-06-18', 'now')",
                [],
            )
            .expect_err("an unregistered ruleset_version must violate the foreign key");
        assert!(format!("{err}").to_lowercase().contains("foreign key"));
    }

    /// `insert_ruleset` closes that gap, and a verdict naming the now-registered version
    /// succeeds.
    #[test]
    fn insert_ruleset_registers_the_version_a_verdict_can_then_reference() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        seed_run(&conn);

        insert_ruleset(
            &conn,
            &RulesetRecord { ruleset_version: "v1", rules: r#"{"ephemeral":[]}"#, published_at: "now" },
        )
        .expect("insert_ruleset");

        conn.execute(
            "INSERT INTO verdict
             (verdict_id, snapshot_id, annotation, declared, outcome, reason_code, oracle,
              ruleset_version, protocol_version, derived_at)
             VALUES ('v-1', 'snap-1', 'readOnlyHint', 'true', 'holds', NULL,
                     'kernel_changeset', 'v1', '2026-06-18', 'now')",
            [],
        )
        .expect("a verdict naming a registered ruleset_version must be accepted");
    }

    /// Registering the same ruleset version twice must be a no-op, not an error — a batch
    /// job that re-derives verdicts against the same ruleset on every run must be able to
    /// call this unconditionally.
    #[test]
    fn insert_ruleset_is_idempotent_for_the_same_version() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        let record =
            RulesetRecord { ruleset_version: "v1", rules: r#"{"ephemeral":[]}"#, published_at: "now" };
        insert_ruleset(&conn, &record).expect("first insert_ruleset");
        insert_ruleset(&conn, &record).expect("second insert_ruleset must not error");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM ruleset WHERE ruleset_version = 'v1'", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 1, "re-registering the same version must not create a duplicate row");
    }
}
