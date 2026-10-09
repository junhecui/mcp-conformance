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
//! two migration files today, a framework would be pure overhead, and the runner itself is
//! about 20 lines (ADR-007's general preference for hand-rolled over abstracted, applied at
//! a scale where it's actually cheap). P1-07 added the second one and exercised the runner's
//! incremental path for the first time — see `0002_verdict_derivation_provenance.sql` and
//! `migration_0002_preserves_rows_and_keeps_the_fk`.

use rusqlite::Connection;

/// Migrations in application order. Each name is also the row recorded in
/// `schema_migrations` once applied, so re-ordering this array without renaming a file
/// would silently change what "already applied" means — don't.
const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_initial_schema", include_str!("../migrations/0001_initial_schema.sql")),
    (
        "0002_verdict_derivation_provenance",
        include_str!("../migrations/0002_verdict_derivation_provenance.sql"),
    ),
];

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

/// A `RULESET` row (architecture.md §6) — one immutable, digest-pinned ruleset.
pub struct RulesetRecord<'a> {
    /// Primary key: `Ruleset::identity()`, i.e. `"<label>+sha256:<hex>"`, never the bare
    /// label (ADR-012 decision 7).
    pub ruleset_identity: &'a str,
    /// The rules themselves, as JSON — must satisfy the schema's `json_valid` `CHECK`.
    /// Supplied by the caller rather than serialised here, the same posture
    /// `TOOL_SNAPSHOT.annotations_raw` takes: this crate has no JSON writer and does not
    /// need one.
    pub rules: &'a str,
    /// When this ruleset version was published.
    pub published_at: &'a str,
}

/// Insert one `RULESET` row.
///
/// Needed before any `kernel_changeset` verdict can be written at all, since
/// [`VerdictRecord`]'s `ruleset_identity` is a foreign key into this table.
///
/// # Errors
///
/// Propagates any `rusqlite` error, including a `CHECK` violation if `rules` is not valid
/// JSON, or a duplicate `ruleset_identity`.
pub fn insert_ruleset(conn: &Connection, record: &RulesetRecord<'_>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO ruleset (ruleset_identity, rules, published_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![record.ruleset_identity, record.rules, record.published_at],
    )?;
    Ok(())
}

/// An identifier for the build of the derivation code that produced a verdict
/// (ADR-012 decision 6).
///
/// A verdict is a pure function of `(evidence, ruleset_identity)` **plus the code that
/// derived it**: the `mtime`/`inode` exclusions, the overlay-private xattr name set, the
/// glob dialect and the structural-omission rule all live in `normalise`'s source rather
/// than in ruleset data (ADR-011's own disclosed cost). Without this, re-deriving under the
/// same ruleset at a different commit silently produces different verdicts under
/// indistinguishable provenance.
///
/// `MCP_CONFORMANCE_BUILD_ID` — a commit SHA, set by the orchestrator or CI — is read at
/// compile time. When it is unset the value carries a visible `+unpinned` suffix rather
/// than the workspace's static `0.1.0`, which would look authoritative while identifying
/// nothing: every row written from an unidentified build says so in the row.
#[must_use]
pub fn derivation_version() -> &'static str {
    match option_env!("MCP_CONFORMANCE_BUILD_ID") {
        Some(id) => id,
        None => concat!(env!("CARGO_PKG_VERSION"), "+unpinned"),
    }
}

/// Which oracle produced a verdict, what it was derived from, and what it observed.
///
/// One value rather than the independent columns F-06 started with (`oracle`,
/// `ruleset_version`, plus the `derivation_version` ADR-012 decision 6 adds and the
/// partition counts and invocation result decision 8 adds), because they are not
/// independent: a `kernel_changeset` verdict is *defined* as the output of
/// normalising evidence under a named ruleset with a particular build of the derivation
/// code, and a `protocol_probe` verdict has no changeset and therefore none of them. As
/// separate fields, "kernel changeset, ruleset unknown" and "kernel changeset, partitions
/// unreported" are representable rows that no one can reproduce or argue with; as this enum
/// neither typechecks. SQLite cannot express the same constraint — a conditional `CHECK`
/// cannot be added by `ALTER TABLE` — so the type is where it lives.
pub enum VerdictProvenance<'a> {
    /// The strong oracle (Class A): a kernel-provided overlay changeset, normalised.
    KernelChangeset {
        /// `Ruleset::identity()` as carried on the `CanonicalChangeset` the verdict was
        /// decided from — never a bare label. FK to `RULESET.ruleset_identity`.
        ruleset_identity: &'a str,
        /// Which build of the derivation code ran. [`derivation_version`] supplies it.
        derivation_version: &'a str,
        /// How many changes landed in each ADR-008 partition, from
        /// `verdict::Assessment::reported`. Required, not optional: architecture.md §4.3
        /// says the verdict is emitted against `user_state` *while the other two are
        /// reported*, and a row that drops them cannot support that claim — a tool that
        /// laundered three user-facing writes into `server_internal`/`ephemeral` would
        /// otherwise store a row identical in every column to a tool that touched nothing.
        counts: datamodel::PartitionCounts,
        /// What the `tools/call` produced, from `verdict::Assessment::call`. Required for
        /// the same kind of reason: ADR-012 decision 4 allows a `violated` resting on a
        /// failed invocation, so a row that omits this cannot be told apart from one
        /// resting on a clean successful call.
        call: datamodel::InvocationResult,
    },
    /// The strong oracle, on a run whose **derivation** failed inside the pure closure — a
    /// malformed capture or an uncompilable ruleset (ADR-012 decision 5, which makes those
    /// `unverifiable` verdicts rather than missing rows).
    ///
    /// A separate variant rather than a `KernelChangeset` with `Option` fields, because
    /// without it a driver holding a `malformed_evidence` assessment had three bad choices
    /// and no good one: write `ProtocolProbe` (a false oracle — exactly what ADR-002 and
    /// B-03 exist to prevent), restate the loader's ruleset identity (contradicting ADR-012
    /// decision 7, under which the identity is taken off the changeset, and there is no
    /// changeset here), or drop the row (defeating decision 5). This keeps the oracle
    /// truthful and makes both absences structural.
    ///
    /// It carries the derivation build and the invocation result and nothing else, which is
    /// deliberate: there is no changeset, so there is nothing to count and no identity to
    /// name, while *which build* decided the bytes were malformed is the single most useful
    /// fact for debugging such a row, and the invocation result is a fact about the run
    /// rather than about the derivation.
    KernelChangesetDerivationFailed {
        /// Which build of the derivation code reached the failure.
        derivation_version: &'a str,
        /// What the `tools/call` produced.
        call: datamodel::InvocationResult,
    },
    /// The weak oracle (Track B): probe → invoke → probe over the protocol surface. No
    /// changeset exists, so there is nothing to normalise, nothing to version and no
    /// partitions to count.
    ProtocolProbe,
}

impl VerdictProvenance<'_> {
    /// The `VERDICT.oracle` value this provenance implies.
    #[must_use]
    pub const fn oracle(&self) -> datamodel::Oracle {
        match self {
            Self::KernelChangeset { .. } | Self::KernelChangesetDerivationFailed { .. } => {
                datamodel::Oracle::KernelChangeset
            }
            Self::ProtocolProbe => datamodel::Oracle::ProtocolProbe,
        }
    }

    /// The `VERDICT.ruleset_identity` value, or `None` where no ruleset applies.
    #[must_use]
    pub const fn ruleset_identity(&self) -> Option<&str> {
        match self {
            Self::KernelChangeset { ruleset_identity, .. } => Some(ruleset_identity),
            Self::KernelChangesetDerivationFailed { .. } | Self::ProtocolProbe => None,
        }
    }

    /// The `VERDICT.derivation_version` value, or `None` where no derivation happened.
    #[must_use]
    pub const fn derivation_version(&self) -> Option<&str> {
        match self {
            Self::KernelChangeset { derivation_version, .. }
            | Self::KernelChangesetDerivationFailed { derivation_version, .. } => {
                Some(derivation_version)
            }
            Self::ProtocolProbe => None,
        }
    }

    /// The three `VERDICT.*_count` values, or `None` where there was no changeset to count.
    #[must_use]
    pub const fn counts(&self) -> Option<datamodel::PartitionCounts> {
        match self {
            Self::KernelChangeset { counts, .. } => Some(*counts),
            Self::KernelChangesetDerivationFailed { .. } | Self::ProtocolProbe => None,
        }
    }

    /// The `VERDICT.invocation_result` value, or `None` for an oracle that does not classify
    /// the call this way.
    ///
    /// `None` for [`Self::ProtocolProbe`] rather than a guess: Track B's runner has its own
    /// failure handling and never produces a [`datamodel::InvocationResult`], so inventing a
    /// mapping from it here would record a classification nothing actually made.
    #[must_use]
    pub const fn invocation_result(&self) -> Option<datamodel::InvocationResult> {
        match self {
            Self::KernelChangeset { call, .. }
            | Self::KernelChangesetDerivationFailed { call, .. } => Some(*call),
            Self::ProtocolProbe => None,
        }
    }
}

/// A partition count on its way to an `INTEGER` column.
///
/// `usize` is never wider than `i64` on any platform this harness targets, and a changeset
/// with more than 2^63 changes could not be held in memory to be counted in the first place
/// — but a fallible conversion is still cheaper than an `as` cast whose failure mode is a
/// silently negative count in a published table.
fn count_to_sql(n: usize) -> rusqlite::Result<i64> {
    i64::try_from(n).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

/// A `VERDICT` row (architecture.md §6). The oracle is mandatory and typed — reached
/// through [`VerdictProvenance`] rather than as a caller-supplied string, which is the one
/// property B-02 exists to guarantee: every verdict this function can write carries a real
/// oracle value, never an empty or forgotten one. P1-07 widened that from the oracle alone
/// to everything a kernel-changeset verdict is derived from (ADR-012 decision 8).
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
    /// Which observation surface produced this verdict, and — for the kernel-changeset
    /// oracle — what it was derived from, what it observed, and how the call went. B-02's
    /// whole point, with ADR-012 decisions 6 to 8 folded in: see [`VerdictProvenance`] for
    /// why these six columns travel as one value instead of six independently-settable
    /// fields.
    pub provenance: VerdictProvenance<'a>,
    /// The `RUN` this verdict was derived from, where one is known — the id the integrity
    /// gate attested (`verdict::GatedRun::run_id`).
    ///
    /// This is the path from a verdict to the `EVIDENCE` blobs behind it (architecture.md
    /// §6's `EVIDENCE ||--o{ VERDICT : supports`), since `EVIDENCE` is keyed by `run_id`.
    /// Without it a verdict row names no evidence at all, and a tampered row cannot be
    /// caught by re-derivation because nothing says which evidence it claimed.
    pub run_id: Option<&'a str>,
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
    let (user_state, server_internal, ephemeral) = match record.provenance.counts() {
        Some(c) => (
            Some(count_to_sql(c.user_state)?),
            Some(count_to_sql(c.server_internal)?),
            Some(count_to_sql(c.ephemeral)?),
        ),
        None => (None, None, None),
    };
    conn.execute(
        "INSERT INTO verdict
         (verdict_id, snapshot_id, run_id, annotation, declared, outcome, reason_code,
          oracle, ruleset_identity, derivation_version, invocation_result,
          user_state_count, server_internal_count, ephemeral_count,
          protocol_version, derived_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        rusqlite::params![
            record.verdict_id,
            record.snapshot_id,
            record.run_id,
            record.annotation.as_db_str(),
            record.declared,
            record.outcome.as_db_str(),
            record.reason_code,
            record.provenance.oracle().as_db_str(),
            record.provenance.ruleset_identity(),
            record.provenance.derivation_version(),
            record.provenance.invocation_result().map(datamodel::InvocationResult::as_db_str),
            user_state,
            server_internal,
            ephemeral,
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
    /// The declared value this verdict was checking, as the writer rendered it.
    ///
    /// Read back because it is an aggregation axis, not decoration: ADR-012 decision 3
    /// makes a declared-`false` tool that mutated state `holds`, which pools in a report
    /// with a genuinely verified read-only tool unless `declared` is in the grouping key.
    /// See [`crate::aggregate`].
    pub declared: String,
    /// `Ruleset::identity()` the verdict was derived under, where one applies. Read back
    /// so that ADR-012 decisions 6 and 7 are observable rather than write-only: a replay
    /// (P1-09) or an aggregate report (P5-04) can see exactly which ruleset bytes and which
    /// derivation build produced each row. `None` for the protocol-probe oracle, for a
    /// derivation-failure row, and for rows written before migration `0002`.
    pub ruleset_identity: Option<String>,
    /// Which build of the derivation code produced it. See [`derivation_version`].
    pub derivation_version: Option<String>,
    /// How many changes landed in each ADR-008 partition (architecture.md §4.3). `None` for
    /// an oracle with no changeset, for a derivation-failure row, and for pre-`0002` rows.
    ///
    /// All three columns are written together or not at all, so a row with some but not all
    /// of them is not something [`insert_verdict`] can produce; such a row surfaces as an
    /// error rather than being silently read as a partial count.
    pub counts: Option<datamodel::PartitionCounts>,
    /// What the `tools/call` behind this verdict produced. `None` for the protocol-probe
    /// oracle and for pre-`0002` rows.
    pub invocation_result: Option<datamodel::InvocationResult>,
    /// The `RUN` behind this verdict, and hence the join to its `EVIDENCE` blobs. `None`
    /// for pre-`0002` rows and for any row whose writer did not know one.
    pub run_id: Option<String>,
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
    let mut stmt = conn.prepare(
        "SELECT annotation, oracle, outcome, declared, ruleset_identity, derivation_version,
                invocation_result, user_state_count, server_internal_count, ephemeral_count,
                run_id
         FROM verdict",
    )?;
    stmt.query_map([], |row| {
        let annotation_str: String = row.get(0)?;
        let oracle_str: String = row.get(1)?;
        let outcome_str: String = row.get(2)?;
        let declared: String = row.get(3)?;
        let ruleset_identity: Option<String> = row.get(4)?;
        let derivation_version: Option<String> = row.get(5)?;
        let invocation_str: Option<String> = row.get(6)?;
        let user_state: Option<i64> = row.get(7)?;
        let server_internal: Option<i64> = row.get(8)?;
        let ephemeral: Option<i64> = row.get(9)?;
        let run_id: Option<String> = row.get(10)?;
        let annotation = datamodel::Annotation::from_db_str(&annotation_str).ok_or_else(|| {
            rusqlite::Error::InvalidColumnType(0, "annotation".into(), rusqlite::types::Type::Text)
        })?;
        let oracle = datamodel::Oracle::from_db_str(&oracle_str).ok_or_else(|| {
            rusqlite::Error::InvalidColumnType(1, "oracle".into(), rusqlite::types::Type::Text)
        })?;
        let outcome = datamodel::Outcome::from_db_str(&outcome_str).ok_or_else(|| {
            rusqlite::Error::InvalidColumnType(2, "outcome".into(), rusqlite::types::Type::Text)
        })?;
        let invocation_result = match invocation_str {
            Some(s) => Some(datamodel::InvocationResult::from_db_str(&s).ok_or_else(|| {
                rusqlite::Error::InvalidColumnType(
                    6,
                    "invocation_result".into(),
                    rusqlite::types::Type::Text,
                )
            })?),
            None => None,
        };
        // All three or none: a partial triple is not a shape `insert_verdict` can write, so
        // reading one as a count would be inventing a number for the two columns that are
        // missing.
        let counts = match (user_state, server_internal, ephemeral) {
            (Some(u), Some(s), Some(e)) => Some(datamodel::PartitionCounts {
                user_state: count_from_sql(u, 7)?,
                server_internal: count_from_sql(s, 8)?,
                ephemeral: count_from_sql(e, 9)?,
            }),
            (None, None, None) => None,
            _ => {
                return Err(rusqlite::Error::InvalidColumnType(
                    7,
                    "user_state_count/server_internal_count/ephemeral_count".into(),
                    rusqlite::types::Type::Integer,
                ));
            }
        };
        Ok(VerdictRow {
            annotation,
            oracle,
            outcome,
            declared,
            ruleset_identity,
            derivation_version,
            counts,
            invocation_result,
            run_id,
        })
    })?
    .collect()
}

/// The inverse of [`count_to_sql`]. A negative stored count is rejected rather than wrapped
/// — nothing in this crate can write one, so a row carrying one has been edited outside it.
fn count_from_sql(n: i64, column: usize) -> rusqlite::Result<usize> {
    usize::try_from(n).map_err(|_| {
        rusqlite::Error::InvalidColumnType(
            column,
            "partition count".into(),
            rusqlite::types::Type::Integer,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Ruleset::identity()` of the published ruleset v1 — label *and* the SHA-256 of
    /// `rulesets/v1.yaml`'s exact bytes, which is what the column holds after migration
    /// `0002` (ADR-012 decision 7). The digest matches `store::ruleset::PUBLISHED`.
    const RULESET_IDENTITY: &str =
        "v1+sha256:48e55850021a462d5710d72e06b5bebe256b1a8107d5a70b0202a1f0b63c1128";

    /// Ruleset v1's actual rules, as `RULESET.rules` is meant to hold them — not a pointer
    /// at the file (`{"source":"rulesets/v1.yaml"}`) and not `{}`. ADR-005's claim is that a
    /// reviewer handed the evidence and the ruleset reproduces every verdict, and a database
    /// row holding a filename does not hand them anything: the identity's digest then pins
    /// bytes that are nowhere in the bundle. `store::ruleset::to_json` is what produces this
    /// shape from a loaded ruleset in real use.
    const RULESET_RULES_JSON: &str = r#"{"version":"v1","ephemeral":["/tmp/**","/var/tmp/**","/run/**","**/*.lock","**/*.pid","**/*.sock"],"server_internal":["**/.cache/**","**/.config/**","**/.local/state/**","**/__pycache__/**","**/node_modules/.cache/**"]}"#;

    /// Partition counts as a kernel-changeset verdict carries them: one decisive user-state
    /// change plus noise that is reported and cannot decide (architecture.md §4.3).
    const SAMPLE_COUNTS: datamodel::PartitionCounts =
        datamodel::PartitionCounts { user_state: 1, server_internal: 2, ephemeral: 3 };

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
            MIGRATIONS.len() as i64,
            "each migration must be recorded exactly once"
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
                provenance: VerdictProvenance::ProtocolProbe,
                run_id: None,
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
        insert_ruleset(
            &conn,
            &RulesetRecord {
                ruleset_identity: RULESET_IDENTITY,
                rules: RULESET_RULES_JSON,
                published_at: "2026-10-07T00:00:00Z",
            },
        )
        .expect("insert ruleset");

        insert_verdict(
            &conn,
            &VerdictRecord {
                verdict_id: "v-kernel",
                snapshot_id: "snap-a",
                annotation: datamodel::Annotation::ReadOnlyHint,
                declared: "false",
                outcome: datamodel::Outcome::Violated,
                reason_code: None,
                // The kernel-changeset arm cannot be written without naming the ruleset
                // identity, the derivation build, the partition counts and the invocation
                // result (ADR-012 decisions 6 to 8) — there is no variant of
                // `VerdictProvenance` that omits any of them.
                provenance: VerdictProvenance::KernelChangeset {
                    ruleset_identity: RULESET_IDENTITY,
                    derivation_version: "abc1234",
                    counts: SAMPLE_COUNTS,
                    call: datamodel::InvocationResult::Completed,
                },
                run_id: None,
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

        // ADR-012 decisions 6 and 7, on read-back: the kernel-changeset row names the
        // ruleset *identity* (not the bare label) and the derivation build; the
        // protocol-probe row names neither, because neither exists for that oracle.
        assert_eq!(kernel_row.ruleset_identity.as_deref(), Some(RULESET_IDENTITY));
        assert!(
            kernel_row.ruleset_identity.as_deref().is_some_and(|i| i.contains("+sha256:")),
            "a label alone can be reused over edited rules; a digest cannot"
        );
        assert_eq!(kernel_row.derivation_version.as_deref(), Some("abc1234"));
        assert_eq!(probe_row.ruleset_identity, None);
        assert_eq!(probe_row.derivation_version, None);

        // B1: the partition counts architecture.md §4.3 promises to report survive into the
        // row and come back out. Without these three columns the two rows below would be
        // indistinguishable from verdicts on tools that touched nothing.
        assert_eq!(kernel_row.counts, Some(SAMPLE_COUNTS));
        assert_eq!(probe_row.counts, None, "the weak oracle has no partitions to count");

        // B3: and which `tools/call` result the verdict rests on.
        assert_eq!(
            kernel_row.invocation_result,
            Some(datamodel::InvocationResult::Completed)
        );
        assert_eq!(probe_row.invocation_result, None);

        // B2: `declared` is read back, because an aggregate report has to group on it.
        assert_eq!(kernel_row.declared, "false");
        assert_eq!(probe_row.declared, "true");

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
                provenance: VerdictProvenance::ProtocolProbe,
                run_id: None,
                protocol_version: "2025-11-25",
                derived_at: "now",
            },
        )
        .expect_err("unverifiable without a reason_code must still violate the CHECK");
        assert!(format!("{err}").to_lowercase().contains("check"));
    }

    /// Migration `0002` runs against real 0001-era data, which is what the committed Track
    /// B sweep database is. Three things must survive it: the rows, the foreign-key
    /// relationship across the renamed column, and the old rows' honest `NULL` in the new
    /// column.
    ///
    /// `ALTER TABLE ... RENAME COLUMN` is documented to rewrite references to the column in
    /// other tables' foreign-key clauses; this proves it happened on the SQLite `rusqlite`
    /// actually bundles, rather than trusting the documentation.
    #[test]
    fn migration_0002_preserves_rows_and_keeps_the_fk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("meta.sqlite3");
        let path_str = path.to_str().expect("utf8 path");

        // Stand up a database at 0001 only, exactly as it existed before this change.
        {
            let conn = Connection::open(path_str).expect("open");
            conn.pragma_update(None, "foreign_keys", "ON").expect("fk on");
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_migrations (
                    name        TEXT PRIMARY KEY,
                    applied_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
                 );",
            )
            .expect("migrations table");
            conn.execute_batch(MIGRATIONS[0].1).expect("apply 0001");
            conn.execute("INSERT INTO schema_migrations (name) VALUES (?1)", [MIGRATIONS[0].0])
                .expect("record 0001");
            seed_run(&conn);
            conn.execute(
                "INSERT INTO ruleset (ruleset_version, rules, published_at)
                 VALUES (?1, '{}', 'then')",
                [RULESET_IDENTITY],
            )
            .expect("0001-era ruleset row");
            conn.execute(
                "INSERT INTO verdict
                 (verdict_id, snapshot_id, annotation, declared, outcome, oracle,
                  ruleset_version, protocol_version, derived_at)
                 VALUES ('v-old', 'snap-1', 'readOnlyHint', 'true', 'holds',
                         'kernel_changeset', ?1, '2025-11-25', 'then')",
                [RULESET_IDENTITY],
            )
            .expect("0001-era verdict row");
        }

        // Re-open through the real runner: only 0002 should apply.
        let conn = open_and_migrate(path_str).expect("migrate to 0002");
        let applied: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| row.get(0))
            .expect("count");
        assert_eq!(applied, 2);

        let rows = list_verdicts(&conn).expect("list_verdicts");
        assert_eq!(rows.len(), 1, "the pre-existing row must survive the rename");
        assert_eq!(rows[0].ruleset_identity.as_deref(), Some(RULESET_IDENTITY));
        assert_eq!(rows[0].declared, "true", "an existing column must still read back");
        // Every column `0002` adds is honestly NULL on a row written before it existed,
        // rather than defaulted to something that would look recorded.
        assert_eq!(
            rows[0].derivation_version, None,
            "a row written before the column existed must say so, not guess"
        );
        assert_eq!(rows[0].counts, None, "no partition counts were recorded then");
        assert_eq!(rows[0].invocation_result, None);
        assert_eq!(rows[0].run_id, None);

        // The FK survived the rename of the column it points at.
        let err = conn
            .execute(
                "INSERT INTO verdict
                 (verdict_id, snapshot_id, annotation, declared, outcome, oracle,
                  ruleset_identity, protocol_version, derived_at)
                 VALUES ('v-dangling', 'snap-1', 'readOnlyHint', 'true', 'holds',
                         'kernel_changeset', 'v9+sha256:nope', '2025-11-25', 'now')",
                [],
            )
            .expect_err("a dangling ruleset_identity must still be rejected");
        assert!(format!("{err}").to_lowercase().contains("foreign key"), "{err}");
    }

    /// The FK is what makes `ruleset_identity` more than a string: a `kernel_changeset`
    /// verdict naming a ruleset that was never registered cannot be stored, so a published
    /// verdict always has rules a reviewer can be handed (ADR-005's reproducibility claim).
    #[test]
    fn a_kernel_changeset_verdict_needs_a_registered_ruleset() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        seed_run(&conn);

        let record = VerdictRecord {
            verdict_id: "v-1",
            snapshot_id: "snap-1",
            annotation: datamodel::Annotation::ReadOnlyHint,
            declared: "true",
            outcome: datamodel::Outcome::Violated,
            reason_code: None,
            provenance: VerdictProvenance::KernelChangeset {
                ruleset_identity: RULESET_IDENTITY,
                derivation_version: derivation_version(),
                counts: SAMPLE_COUNTS,
                call: datamodel::InvocationResult::Completed,
            },
            run_id: None,
            protocol_version: "2025-11-25",
            derived_at: "now",
        };
        let err = insert_verdict(&conn, &record).expect_err("unregistered ruleset");
        assert!(format!("{err}").to_lowercase().contains("foreign key"), "{err}");

        insert_ruleset(
            &conn,
            &RulesetRecord {
                ruleset_identity: RULESET_IDENTITY,
                rules: RULESET_RULES_JSON,
                published_at: "now",
            },
        )
        .expect("register the ruleset");
        insert_verdict(&conn, &record).expect("same verdict now insertable");
    }

    /// F3's row shape, which had no storage test anywhere in the tree: a derivation that
    /// failed inside the pure closure still produces a verdict (ADR-012 decision 5), and
    /// that verdict has to be storable **without** lying about which oracle produced it.
    ///
    /// `oracle = 'kernel_changeset'` with a NULL `ruleset_identity` is exactly the row the
    /// three-field version of `VerdictProvenance` could not express, which left a driver
    /// choosing between a false `protocol_probe` tag, a restated ruleset identity it was
    /// never given, and dropping the row.
    #[test]
    fn a_derivation_failure_row_keeps_the_kernel_oracle_with_no_ruleset() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        seed_run(&conn);

        // No `insert_ruleset` call: the point is that this row needs none, so the FK cannot
        // be what rejects it.
        insert_verdict(
            &conn,
            &VerdictRecord {
                verdict_id: "v-derivation-failed",
                snapshot_id: "snap-1",
                annotation: datamodel::Annotation::ReadOnlyHint,
                declared: "true",
                outcome: datamodel::Outcome::Unverifiable,
                reason_code: Some("malformed_evidence"),
                provenance: VerdictProvenance::KernelChangesetDerivationFailed {
                    derivation_version: derivation_version(),
                    call: datamodel::InvocationResult::Completed,
                },
                run_id: Some("run-1"),
                protocol_version: "2025-11-25",
                derived_at: "now",
            },
        )
        .expect("a derivation-failure verdict must be storable");

        let rows = list_verdicts(&conn).expect("list_verdicts");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].oracle,
            datamodel::Oracle::KernelChangeset,
            "the oracle must stay truthful — a derivation failure is not a protocol probe"
        );
        assert_eq!(rows[0].outcome, datamodel::Outcome::Unverifiable);
        assert_eq!(
            rows[0].ruleset_identity, None,
            "there was no changeset, so there is no identity to carry off one"
        );
        assert_eq!(rows[0].counts, None, "and nothing to count");
        assert_eq!(rows[0].derivation_version.as_deref(), Some(derivation_version()));
        assert_eq!(
            rows[0].invocation_result,
            Some(datamodel::InvocationResult::Completed),
            "the invocation result is a fact about the run, not about the derivation"
        );
        assert_eq!(rows[0].run_id.as_deref(), Some("run-1"));
    }

    /// `VERDICT.run_id` is what connects a verdict to the `EVIDENCE` blobs behind it
    /// (architecture.md §6's `EVIDENCE ||--o{ VERDICT : supports`, which no migration
    /// implemented until `0002`), and it is a real foreign key rather than a loose string.
    #[test]
    fn a_verdict_names_the_run_it_came_from_and_the_fk_bites() {
        let conn = open_and_migrate(":memory:").expect("open_and_migrate");
        seed_run(&conn);
        conn.execute(
            "INSERT INTO evidence (digest, run_id, kind, blob_ref)
             VALUES (?1, 'run-1', 'overlay_upper', 'blob-ref')",
            [&"a".repeat(64)],
        )
        .expect("insert evidence for the run");

        let record = |verdict_id: &'static str, run_id: Option<&'static str>| VerdictRecord {
            verdict_id,
            snapshot_id: "snap-1",
            annotation: datamodel::Annotation::ReadOnlyHint,
            declared: "true",
            outcome: datamodel::Outcome::Unverifiable,
            reason_code: Some("invocation_failed"),
            provenance: VerdictProvenance::KernelChangesetDerivationFailed {
                derivation_version: derivation_version(),
                call: datamodel::InvocationResult::NoResult,
            },
            run_id,
            protocol_version: "2025-11-25",
            derived_at: "now",
        };

        let err = insert_verdict(&conn, &record("v-dangling", Some("run-does-not-exist")))
            .expect_err("a dangling run_id must be rejected");
        assert!(format!("{err}").to_lowercase().contains("foreign key"), "{err}");

        insert_verdict(&conn, &record("v-ok", Some("run-1"))).expect("a real run_id is accepted");

        // The join architecture.md §6 asks for: verdict -> run -> evidence.
        let digest: String = conn
            .query_row(
                "SELECT e.digest FROM verdict v
                 JOIN evidence e ON e.run_id = v.run_id
                 WHERE v.verdict_id = 'v-ok'",
                [],
                |row| row.get(0),
            )
            .expect("a verdict must be able to name its evidence");
        assert_eq!(digest, "a".repeat(64));
    }

    /// An unidentified build must be visibly unidentified in every row it writes, rather
    /// than silently recording the workspace's static version as though it pinned anything —
    /// and a build that *is* identified must record exactly what it was told, not a
    /// decoration of it.
    ///
    /// Both branches are live: CI sets `MCP_CONFORMANCE_BUILD_ID` to the commit SHA (F4), so
    /// the first branch is what a CI run checks and the second what a local build does.
    #[test]
    fn derivation_version_admits_when_the_build_is_unpinned() {
        let v = derivation_version();
        assert!(!v.is_empty());
        match option_env!("MCP_CONFORMANCE_BUILD_ID") {
            Some(id) => assert_eq!(v, id, "a pinned build must record its id verbatim"),
            None => assert!(v.ends_with("+unpinned"), "got {v}"),
        }
    }
}
