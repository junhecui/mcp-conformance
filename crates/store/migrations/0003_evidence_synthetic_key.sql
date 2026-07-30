-- P5-02: fixes a real bug in 0001's EVIDENCE table, found empirically while building the
-- offline verdict-derivation batch job, not spotted by inspection alone. `digest` was the
-- table's sole PRIMARY KEY, which makes it impossible to record two different runs that
-- happen to produce byte-identical evidence — exactly the common case for a clean
-- read-only tool (P1-08's own write-up already noted this in passing: "same evidence
-- digest both times, since the tool writes nothing", without anything yet depending on
-- storing both). Reproduced directly before writing this migration: inserting a second
-- EVIDENCE row for a different run_id at an already-used digest fails with `UNIQUE
-- constraint failed: evidence.digest`. Content deduplication is F-05's `BlobStore`'s job
-- (one blob per digest, regardless of how many runs produced it); this table's job is
-- recording *which run* produced *which evidence*, and a digest is not a run — two
-- different rows must be able to point at the one deduplicated blob.
--
-- Fixed by giving EVIDENCE a synthetic, auto-assigned `evidence_id` primary key — the
-- pattern `run_queue.job_id` (0002) already established for a table with no natural
-- semantic string key — plus a `UNIQUE(run_id, kind)` constraint (one evidence row per
-- kind per run) in place of the old uniqueness-on-digest. `digest` remains `NOT NULL` and
-- keeps its shape `CHECK`, just no longer doubles as this table's identity. Safe to do as a
-- straight drop-and-recreate rather than a data-preserving migration: nothing in this
-- codebase has ever written a production EVIDENCE row before this task (the only prior
-- `INSERT`s into this table were raw SQL inside `store::db`'s own test module, seeded fresh
-- by each test).

DROP TRIGGER evidence_no_update;
DROP TRIGGER evidence_no_delete;
DROP TABLE evidence;

CREATE TABLE evidence (
    evidence_id  INTEGER PRIMARY KEY AUTOINCREMENT,
    digest       TEXT NOT NULL CHECK (length(digest) = 64 AND digest = lower(digest)),
    run_id       TEXT NOT NULL REFERENCES run (run_id),
    kind         TEXT NOT NULL,
    blob_ref     TEXT NOT NULL,
    UNIQUE (run_id, kind)
);

CREATE INDEX evidence_run_id_idx ON evidence (run_id);
CREATE INDEX evidence_digest_idx ON evidence (digest);

-- Invariant 2, unchanged: EVIDENCE stays immutable on both sides of the evidence/metadata
-- split.
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
