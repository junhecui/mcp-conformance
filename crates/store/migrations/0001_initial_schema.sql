-- F-06: initial metadata schema. Mirrors the ER diagram in docs/architecture.md §6, and
-- encodes its three stated invariants as constraints rather than as comments:
--
--   1. VERDICT keys on snapshot_id, never (server_id, tool_name) — there is deliberately
--      no server_id or tool_name column on `verdict`.
--   2. EVIDENCE is immutable and content-addressed; VERDICT is derived and disposable —
--      enforced below with triggers that reject UPDATE/DELETE on `evidence`.
--   3. VERDICT.reason_code is non-null whenever outcome = 'unverifiable' — a CHECK
--      constraint on `verdict`.
--
-- FIXTURE's columns are not specified in architecture.md §6 (only its relationship to RUN
-- is: "FIXTURE ||--o{ RUN : seeds"). The shape below is a considered gap-fill, grounded in
-- how fixtures are described elsewhere (design.md §4, architecture.md §3's World
-- provisioner, and the fixtures/generic + fixtures/per-server split in the repo layout),
-- and is mirrored back into docs/architecture.md §6 in the same change.

CREATE TABLE server (
    server_id             TEXT PRIMARY KEY,
    source_uri            TEXT NOT NULL,
    containability_class  TEXT NOT NULL
                           CHECK (containability_class IN ('A', 'B', 'unclassifiable')),
    spec_revision         TEXT NOT NULL
);

CREATE TABLE tool_snapshot (
    snapshot_id           TEXT PRIMARY KEY,
    server_id             TEXT NOT NULL REFERENCES server (server_id),
    tool_name             TEXT NOT NULL,
    metadata_pin          TEXT NOT NULL,
    annotations_raw       TEXT NOT NULL CHECK (json_valid(annotations_raw)),
    readonly_explicit     INTEGER NOT NULL CHECK (readonly_explicit IN (0, 1)),
    destructive_explicit  INTEGER NOT NULL CHECK (destructive_explicit IN (0, 1)),
    idempotent_explicit   INTEGER NOT NULL CHECK (idempotent_explicit IN (0, 1)),
    openworld_explicit    INTEGER NOT NULL CHECK (openworld_explicit IN (0, 1)),
    observed_at           TEXT NOT NULL
);

CREATE INDEX tool_snapshot_server_id_idx ON tool_snapshot (server_id);

CREATE TABLE fixture (
    fixture_id      TEXT PRIMARY KEY,
    -- NULL for a generic fixture (fixtures/generic); set for a per-server one
    -- (fixtures/per-server), and required to be set in exactly that case, below.
    server_id       TEXT REFERENCES server (server_id),
    kind            TEXT NOT NULL CHECK (kind IN ('generic', 'per_server')),
    content_digest  TEXT NOT NULL CHECK (length(content_digest) = 64
                                          AND content_digest = lower(content_digest)),
    created_at      TEXT NOT NULL,
    CHECK ((kind = 'generic' AND server_id IS NULL)
           OR (kind = 'per_server' AND server_id IS NOT NULL))
);

CREATE TABLE run (
    run_id           TEXT PRIMARY KEY,
    snapshot_id      TEXT NOT NULL REFERENCES tool_snapshot (snapshot_id),
    arm              TEXT NOT NULL,
    fixture_id       TEXT REFERENCES fixture (fixture_id),
    arguments        TEXT NOT NULL CHECK (json_valid(arguments)),
    harness_version  TEXT NOT NULL,
    started_at       TEXT NOT NULL
);

CREATE INDEX run_snapshot_id_idx ON run (snapshot_id);

CREATE TABLE integrity (
    run_id            TEXT PRIMARY KEY REFERENCES run (run_id),
    clean_teardown    INTEGER NOT NULL CHECK (clean_teardown IN (0, 1)),
    caps_respected    INTEGER NOT NULL CHECK (caps_respected IN (0, 1)),
    timed_out         INTEGER NOT NULL CHECK (timed_out IN (0, 1)),
    denied_syscalls   TEXT NOT NULL CHECK (json_valid(denied_syscalls)),
    adversarial_flag  INTEGER NOT NULL CHECK (adversarial_flag IN (0, 1))
);

-- digest is the same 64-lowercase-hex-character shape as store::BlobStore's addressing
-- (crates/store/src/lib.rs, F-05) — this is the metadata row that points at that blob.
CREATE TABLE evidence (
    digest    TEXT PRIMARY KEY CHECK (length(digest) = 64 AND digest = lower(digest)),
    run_id    TEXT NOT NULL REFERENCES run (run_id),
    kind      TEXT NOT NULL,
    blob_ref  TEXT NOT NULL
);

CREATE INDEX evidence_run_id_idx ON evidence (run_id);

-- Invariant 2. Matches F-05's BlobStore, which exposes no update/delete method either —
-- immutability enforced structurally on both sides of the evidence/metadata split, not by
-- convention.
CREATE TRIGGER evidence_no_update
BEFORE UPDATE ON evidence
BEGIN
    SELECT RAISE(ABORT, 'EVIDENCE is immutable: rows may not be updated');
END;

CREATE TRIGGER evidence_no_delete
BEFORE DELETE ON evidence
BEGIN
    SELECT RAISE(ABORT, 'EVIDENCE is immutable: rows may not be deleted');
END;

CREATE TABLE ruleset (
    ruleset_version  TEXT PRIMARY KEY,
    rules            TEXT NOT NULL CHECK (json_valid(rules)),
    published_at     TEXT NOT NULL
);

CREATE TABLE verdict (
    verdict_id        TEXT PRIMARY KEY,
    -- Invariant 1: no server_id or tool_name column here, deliberately.
    snapshot_id       TEXT NOT NULL REFERENCES tool_snapshot (snapshot_id),
    annotation        TEXT NOT NULL
                       CHECK (annotation IN ('readOnlyHint', 'destructiveHint',
                                              'idempotentHint', 'openWorldHint')),
    declared          TEXT NOT NULL,
    outcome           TEXT NOT NULL CHECK (outcome IN ('holds', 'violated', 'unverifiable')),
    reason_code       TEXT,
    oracle            TEXT NOT NULL CHECK (oracle IN ('kernel_changeset', 'protocol_probe')),
    ruleset_version   TEXT REFERENCES ruleset (ruleset_version),
    protocol_version  TEXT NOT NULL,
    -- §12 item 6: added now, ahead of Phase 5, per the disclosure workflow (P5-03).
    embargo_state     TEXT NOT NULL DEFAULT 'none'
                       CHECK (embargo_state IN ('none', 'embargoed', 'disclosed')),
    disclosed_at      TEXT,
    derived_at        TEXT NOT NULL,
    -- Invariant 3.
    CHECK (outcome != 'unverifiable' OR reason_code IS NOT NULL)
);

CREATE INDEX verdict_snapshot_id_idx ON verdict (snapshot_id);
