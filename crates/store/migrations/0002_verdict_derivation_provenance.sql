-- P1-07 (ADR-012 decisions 6 to 8): make a verdict's derivation, its evidence and what it
-- actually observed all nameable from the row.
--
-- architecture.md §6 invariant 2 says a verdict is derived and disposable — re-runnable over
-- historical evidence to regenerate the whole table. Read literally, that requires the
-- derivation's inputs to be recoverable from the row, and at F-06 most of them were not.
-- Everything below closes one of those gaps while migration `0002` is still uncommitted, in
-- the same spirit as F-06 adding `embargo_state` ahead of Phase 5. The recurring shape of
-- the gaps is worth naming: in each case the distinction existed correctly in the Rust
-- types and was destroyed at the storage boundary, so the published artefact could not
-- support a claim the design makes.
--
-- 1. `ruleset_version` -> `ruleset_identity`, on both tables. The column now holds
--    `Ruleset::identity()` — `"<label>+sha256:<hex of the exact ruleset file bytes>"`,
--    which `CanonicalChangeset.ruleset_identity` already carries — rather than the bare
--    label `"v1"`. A label can be reused over edited rules; a digest cannot (ADR-011
--    decision 9). This is a two-table change by construction: `VERDICT.ruleset_identity`
--    is a foreign key into `RULESET`, so storing the identity means `RULESET`'s primary key
--    has to become the identity too, and both columns are renamed so the name never
--    disagrees with the contents.
--
-- 2. `VERDICT.derivation_version` added. ADR-011's own disclosed cost is that the
--    `mtime`/`inode` exclusions, the overlay-private xattr name set, the glob dialect and
--    the structural-omission rule live in `normalise`'s source rather than in ruleset data,
--    so a verdict is a function of `(evidence, ruleset)` *plus the code that derived it*.
--
-- 3. `VERDICT.{user_state,server_internal,ephemeral}_count` added. architecture.md §4.3
--    requires the verdict to be emitted against `user_state` *while reporting the other
--    two*, "so critics have something to argue with that isn't the verdict itself". The
--    engine computed those counts from the first commit and nothing stored them, which made
--    §4.3 true of the type and false of the artefact: a tool declaring `readOnlyHint: true`
--    that wrote `~/.cache/stolen-notes.md`, `~/.config/ssh-key-copy` and
--    `~/invoice-2026.pdf.lock` — overwriting a real document through an allowlisted suffix —
--    stored a row identical in every column to a tool that touched nothing. Both `holds`,
--    both with no reason. Every known ADR-008 laundering route went from visible and
--    arguable to invisible.
--
-- 4. `VERDICT.invocation_result` added. ADR-012 decision 4 deliberately allows a `violated`
--    resting on a *failed* invocation, and intends to keep it. Without this column such a
--    row is byte-identical to a `violated` from a clean successful call, so P5-03 cannot
--    triage a disclosure, P5-04 cannot report the populations separately, and a maintainer
--    objecting "your harness called my tool a violation when the call errored" cannot be
--    answered from the record.
--
-- 5. `VERDICT.run_id` added, a nullable FK to `RUN`. architecture.md §6 declares
--    `EVIDENCE ||--o{ VERDICT : supports` and no migration implemented it: `VERDICT` named
--    neither a run nor an evidence digest, so there was no path from a verdict row to the
--    two blobs that produced it, and a tampered verdict row could not be caught by
--    re-derivation because you could not tell which evidence it claimed. The join goes
--    through `RUN` rather than through a digest column on `VERDICT`, because a
--    kernel-changeset verdict rests on *two* blobs (base and upper layer) and `EVIDENCE` is
--    already keyed by `run_id`; a single `evidence_digest` column could only ever name one
--    of them, and a join table for a relationship this schema already expresses would be
--    worse. `GatedRun::run_id()` — the id the integrity gate attested — is what a driver
--    puts here.
--
-- Columns 2 to 5 are all nullable at the SQL level, for two reasons that are not
-- interchangeable. Rows written before this migration (the committed Track B sweep's
-- database, which is a 0001-era file) keep NULL, which is the honest value: nothing
-- recorded them. And a `protocol_probe` verdict genuinely has no derivation, no changeset
-- and therefore no ruleset and no counts. SQLite cannot express "non-null exactly when
-- `oracle = 'kernel_changeset'`", because a conditional CHECK cannot be added by
-- ALTER TABLE, so that enforcement lives in the type system instead:
-- `store::db::VerdictProvenance::KernelChangeset` requires the ruleset identity, the
-- derivation version, the partition counts *and* the invocation result together, and it is
-- the only way to write `oracle = 'kernel_changeset'` with a changeset behind it.
-- `KernelChangesetDerivationFailed` is the one variant that writes that oracle with a NULL
-- ruleset and NULL counts, and it exists precisely so a derivation-failure verdict can be
-- stored without lying about which oracle produced it.
--
-- `ALTER TABLE ... RENAME COLUMN` is used rather than a 12-step table rebuild: since SQLite
-- 3.25 it rewrites references to the renamed column in other tables' foreign-key clauses,
-- which is exactly what is needed here, and `rusqlite`'s `bundled` feature pins a modern
-- SQLite (F-06's reproducibility posture). `migration_0002_preserves_rows_and_keeps_the_fk`
-- proves the rewrite happened rather than assuming it.

ALTER TABLE ruleset RENAME COLUMN ruleset_version TO ruleset_identity;

ALTER TABLE verdict RENAME COLUMN ruleset_version TO ruleset_identity;

ALTER TABLE verdict ADD COLUMN derivation_version TEXT;

ALTER TABLE verdict ADD COLUMN invocation_result TEXT;

ALTER TABLE verdict ADD COLUMN user_state_count INTEGER;

ALTER TABLE verdict ADD COLUMN server_internal_count INTEGER;

ALTER TABLE verdict ADD COLUMN ephemeral_count INTEGER;

-- A NULL default is required here rather than merely chosen: SQLite refuses
-- ALTER TABLE ADD COLUMN with a REFERENCES clause and any other default.
ALTER TABLE verdict ADD COLUMN run_id TEXT REFERENCES run (run_id);

CREATE INDEX verdict_run_id_idx ON verdict (run_id);
