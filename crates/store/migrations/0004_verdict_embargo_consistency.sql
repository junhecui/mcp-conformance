-- P5-03: closes a gap in `0001`'s `VERDICT.embargo_state`/`disclosed_at` pair, found while
-- giving the disclosure workflow's state machine real behaviour. `disclosed_at` had no
-- constraint tying it to `embargo_state` at all — nothing stopped a `disclosed_at` timestamp
-- from being set while `embargo_state` stayed `'none'` or `'embargoed'`, or a verdict from
-- being marked `'disclosed'` with no timestamp recorded for when. The same discipline
-- invariant 3 already applies to `outcome`/`reason_code` (a `CHECK`, not a comment asking
-- callers to keep the two in sync by hand) now applies to this pair too:
-- `embargo_state = 'disclosed'` if and only if `disclosed_at IS NOT NULL`.
--
-- Safe to do as a straight drop-and-recreate, same reasoning `0003` already used for
-- `EVIDENCE`: nothing outside this schema's own tests has ever written a `VERDICT` row
-- before P5-02 landed a handful of test-seeded ones, and nothing else in this schema holds
-- a foreign key into `verdict` (the `EVIDENCE ||--o{ VERDICT : supports` relationship in
-- architecture.md §6's ER diagram is conceptual — F-06's actual SQL never added a
-- `verdict_id` column to `EVIDENCE`), so there is no dependent table to preserve alongside
-- it.

DROP TABLE verdict;

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
    embargo_state     TEXT NOT NULL DEFAULT 'none'
                       CHECK (embargo_state IN ('none', 'embargoed', 'disclosed')),
    disclosed_at      TEXT,
    derived_at        TEXT NOT NULL,
    -- Invariant 3.
    CHECK (outcome != 'unverifiable' OR reason_code IS NOT NULL),
    -- P5-03's own invariant: a disclosure timestamp exists exactly when the state says the
    -- verdict has been disclosed, never before and never left unset after.
    CHECK ((embargo_state = 'disclosed') = (disclosed_at IS NOT NULL))
);

CREATE INDEX verdict_snapshot_id_idx ON verdict (snapshot_id);
