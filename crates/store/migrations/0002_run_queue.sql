-- P5-01: the run queue a worker pool leases jobs from. Deliberately its own migration
-- rather than folded into 0001 — this table is queue-shaped (rows are mutated in place:
-- leased, completed, retried), the opposite mutability contract from every table 0001
-- defined (RUN/INTEGRITY/EVIDENCE/VERDICT are append-only or immutable), so it earns its
-- own file rather than blurring that line in the schema's oldest migration.
--
-- `leased_until` is a unix-epoch-seconds INTEGER, not a TEXT timestamp like the rest of
-- this schema's timestamp columns: the lease query below compares it against "now" in a
-- `WHERE` clause, and SQLite compares INTEGER columns numerically — a TEXT timestamp would
-- only sort correctly if every writer used one exact, fixed-width format forever, which is
-- exactly the kind of implicit contract this schema otherwise avoids via `CHECK`s.

CREATE TABLE run_queue (
    job_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    payload       TEXT NOT NULL CHECK (json_valid(payload)),
    status        TEXT NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('pending', 'leased', 'done', 'failed')),
    leased_by     TEXT,
    leased_until  INTEGER,
    attempts      INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL
);

CREATE INDEX run_queue_status_idx ON run_queue (status);
