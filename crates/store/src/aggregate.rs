//! B-03: aggregate verdicts for reporting, and guard against a report that pools rows
//! across an axis it does not disclose.
//!
//! ADR-002 (architecture.md §9): *"Requires that results never aggregate across oracles
//! without disclosure — an easy invariant to state and an easy one to violate in a summary
//! table."* This module is that invariant made checkable rather than a habit.
//! [`aggregate`] is the only correct way to build a report from raw verdicts; it cannot
//! produce a row that pools two oracles together, because `oracle` is part of the grouping
//! key by construction. [`verify_report_matches_records`] is the independent guard: given
//! *any* report — including one built some other way, by code that never saw this module —
//! it checks whether that report's counts could only have come from a single oracle, and a
//! single declared value, per row. The tests at the bottom construct deliberately mixed
//! reports and prove they get rejected, which is the literal B-03 exit criterion: *"Any
//! report that mixes oracles without separating them fails a test."*
//!
//! # Why `declared` is part of the key, not just `oracle`
//!
//! ADR-012 decision 3 is right that a declared-`false` tool which mutated state is `holds`:
//! the declaration said the tool may modify state, and it did. But a published
//! `readOnlyHint / holds` count that does not say *what was declared* pools four different
//! realities into one number — a quiet tool that was genuinely verified read-only, a tool
//! that laundered three user-facing writes past ADR-008's allowlists, and two tools that
//! merely admitted they write. The attack needs no effort at all: declare
//! `readOnlyHint: false`, **or declare nothing** (`verdict::Declared::Defaulted` reaches the
//! same arm, and 48.6% of census-era tools declare nothing), touch one `user_state` path,
//! return successfully — `holds`, every time, for any number of tools. The cross-oracle
//! guard passed that report, because oracle disclosure is a different axis. So `declared` is
//! handled exactly as `oracle` is: part of the grouping key, mandatory on a published row,
//! and its absence is itself a disclosure failure.

use std::collections::HashMap;

use datamodel::{Annotation, Oracle, Outcome};

use crate::db::VerdictRow;

/// One verdict, reduced to what aggregation needs to see.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VerdictSummary {
    /// Which annotation this verdict assesses.
    pub annotation: Annotation,
    /// Which oracle produced it.
    pub oracle: Oracle,
    /// The outcome.
    pub outcome: Outcome,
    /// The declared value this verdict was checking, verbatim as the row stores it — an
    /// aggregation axis in its own right, for the reason in this module's docs.
    pub declared: String,
}

impl From<VerdictRow> for VerdictSummary {
    fn from(row: VerdictRow) -> Self {
        Self {
            annotation: row.annotation,
            oracle: row.oracle,
            outcome: row.outcome,
            declared: row.declared,
        }
    }
}

/// One row in a published aggregate report. `oracle` and `declared` are plain, mandatory
/// fields — there is no variant of this type and no constructor anywhere in this module that
/// produces a row without either, which is exactly the property ADR-002 wants: a count can
/// never appear in a report without saying which oracle it came from, or what the tools it
/// counts had actually declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateRow {
    /// Which annotation this row summarises.
    pub annotation: Annotation,
    /// Which oracle every verdict counted in this row came from.
    pub oracle: Oracle,
    /// The outcome this row counts.
    pub outcome: Outcome,
    /// The declared value every verdict counted in this row was checking.
    pub declared: String,
    /// How many verdicts matched `(annotation, oracle, outcome, declared)`.
    pub count: usize,
}

/// The only correct way to build a report from raw verdicts: groups strictly by
/// `(annotation, oracle, outcome, declared)`. Two verdicts that agree on annotation and
/// outcome but differ on oracle, or on what was declared, always land in different rows —
/// there is no code path here that could merge their counts.
///
/// Output is sorted for determinism (by each field's `Display`/stored text — none of
/// `Annotation`/`Oracle`/`Outcome` derive `Ord`, and adding it just for this would be scope
/// creep on types that exist for a different reason), so two calls over the same input always
/// produce identical output, useful for snapshotting a published report.
#[must_use]
pub fn aggregate(records: &[VerdictSummary]) -> Vec<AggregateRow> {
    let mut counts: HashMap<(Annotation, Oracle, Outcome, &str), usize> = HashMap::new();
    for r in records {
        *counts.entry((r.annotation, r.oracle, r.outcome, r.declared.as_str())).or_insert(0) += 1;
    }

    let mut rows: Vec<AggregateRow> = counts
        .into_iter()
        .map(|((annotation, oracle, outcome, declared), count)| AggregateRow {
            annotation,
            oracle,
            outcome,
            declared: declared.to_owned(),
            count,
        })
        .collect();
    rows.sort_by(|a, b| {
        (a.annotation.as_db_str(), a.oracle.as_db_str(), a.outcome.as_db_str(), &a.declared).cmp(
            &(b.annotation.as_db_str(), b.oracle.as_db_str(), b.outcome.as_db_str(), &b.declared),
        )
    });
    rows
}

/// A row in an arbitrary report — the shape this guard checks, deliberately weaker than
/// [`AggregateRow`]: `oracle` and `declared` are `Option`, because the entire point is to be
/// able to represent (and then reject) the buggy report a careless aggregator would actually
/// produce, where a count was pooled across one of those axes and the field that would have
/// said so was simply never written down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportRow {
    /// Which annotation this row claims to summarise.
    pub annotation: Annotation,
    /// The outcome this row claims to count.
    pub outcome: Outcome,
    /// The oracle this row claims its count came from — `None` if the report never says.
    pub oracle: Option<Oracle>,
    /// The declared value this row claims its count is about — `None` if the report never
    /// says.
    pub declared: Option<String>,
    /// The claimed count.
    pub count: usize,
}

/// Why a report failed the aggregation guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregationError {
    /// A row claims to summarise a bucket without recording which oracle it came from —
    /// the report cannot be checked at all, which is itself a disclosure failure per
    /// ADR-002.
    OracleNotDisclosed {
        /// The row's annotation.
        annotation: Annotation,
        /// The row's outcome.
        outcome: Outcome,
    },
    /// A row claims to summarise a bucket without recording what the tools it counts had
    /// declared. Uninterpretable for the same reason an undisclosed oracle is: a
    /// `readOnlyHint / holds` count pools a verified read-only tool with one that merely
    /// admitted it writes.
    DeclaredNotDisclosed {
        /// The row's annotation.
        annotation: Annotation,
        /// The oracle the row claims.
        oracle: Oracle,
        /// The row's outcome.
        outcome: Outcome,
    },
    /// A row's claimed count does not match the number of source records that actually
    /// share `(annotation, oracle, outcome, declared)` — the signature of a count that was
    /// pooled across more than one oracle, or more than one declared value, and then
    /// mislabelled under a single one of them.
    CountMismatch {
        /// The row's annotation.
        annotation: Annotation,
        /// The oracle the row claims.
        oracle: Oracle,
        /// The row's outcome.
        outcome: Outcome,
        /// The declared value the row claims.
        declared: String,
        /// What the row claimed.
        reported: usize,
        /// What the source records actually support for that exact
        /// `(annotation, oracle, outcome, declared)`.
        actual: usize,
    },
}

impl std::fmt::Display for AggregationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OracleNotDisclosed { annotation, outcome } => write!(
                f,
                "report row for {annotation}/{outcome} does not disclose an oracle — ADR-002 forbids this"
            ),
            Self::DeclaredNotDisclosed { annotation, oracle, outcome } => write!(
                f,
                "report row for {annotation}/{oracle}/{outcome} does not disclose the declared \
                 value it counts — a holds count that does not say what was declared pools a \
                 verified read-only tool with one that merely admitted it writes"
            ),
            Self::CountMismatch { annotation, oracle, outcome, declared, reported, actual } => write!(
                f,
                "report row for {annotation}/{oracle}/{outcome}/declared={declared} claims count \
                 {reported}, but only {actual} source records match exactly that oracle and \
                 declared value — this count was built some other way than counting records of \
                 one oracle and one declaration, which is the pooling ADR-002 forbids"
            ),
        }
    }
}

impl std::error::Error for AggregationError {}

/// Verify that every row in `report` is a straightforward, accurate count of source
/// `records` that all share a single, disclosed oracle *and* a single, disclosed declared
/// value.
///
/// This is deliberately independent of [`aggregate`] — it takes an arbitrary `report`, not
/// `aggregate`'s own output, so it can catch a report built by code that never went through
/// this module at all. A caller who always builds reports via [`aggregate`] will find this
/// always passes (proven below); the value is in what it rejects.
///
/// # Errors
///
/// The first row that omits its oracle, omits its declared value, or whose count doesn't
/// match exactly the records sharing both.
pub fn verify_report_matches_records(
    report: &[ReportRow],
    records: &[VerdictSummary],
) -> Result<(), AggregationError> {
    for row in report {
        let Some(oracle) = row.oracle else {
            return Err(AggregationError::OracleNotDisclosed {
                annotation: row.annotation,
                outcome: row.outcome,
            });
        };
        let Some(declared) = row.declared.as_deref() else {
            return Err(AggregationError::DeclaredNotDisclosed {
                annotation: row.annotation,
                oracle,
                outcome: row.outcome,
            });
        };
        let actual = records
            .iter()
            .filter(|r| {
                r.annotation == row.annotation
                    && r.oracle == oracle
                    && r.outcome == row.outcome
                    && r.declared == declared
            })
            .count();
        if actual != row.count {
            return Err(AggregationError::CountMismatch {
                annotation: row.annotation,
                oracle,
                outcome: row.outcome,
                declared: declared.to_owned(),
                reported: row.count,
                actual,
            });
        }
    }
    Ok(())
}

/// Convenience: [`AggregateRow`]s (always correctly disclosed) as [`ReportRow`]s, for
/// feeding straight into [`verify_report_matches_records`].
#[must_use]
pub fn to_report_rows(rows: &[AggregateRow]) -> Vec<ReportRow> {
    rows.iter()
        .map(|r| ReportRow {
            annotation: r.annotation,
            outcome: r.outcome,
            oracle: Some(r.oracle),
            declared: Some(r.declared.clone()),
            count: r.count,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        annotation: Annotation,
        oracle: Oracle,
        outcome: Outcome,
        declared: &str,
    ) -> VerdictSummary {
        VerdictSummary {
            annotation,
            oracle,
            outcome,
            declared: declared.to_owned(),
        }
    }

    /// 5 `kernel_changeset` + 3 `protocol_probe` verdicts, all `readOnlyHint`/`holds`/
    /// declared `true`: the same annotation and outcome, differing only on oracle.
    fn sample_records() -> Vec<VerdictSummary> {
        let mut records = Vec::new();
        for _ in 0..5 {
            records.push(record(
                Annotation::ReadOnlyHint,
                Oracle::KernelChangeset,
                Outcome::Holds,
                "true",
            ));
        }
        for _ in 0..3 {
            records.push(record(
                Annotation::ReadOnlyHint,
                Oracle::ProtocolProbe,
                Outcome::Holds,
                "true",
            ));
        }
        records
    }

    /// The four `readOnlyHint`/`holds` realities ADR-012 decision 3 lets coexist, under one
    /// oracle: 2 tools that declared `true` and were verified quiet, and 3 that declared
    /// `false` (or nothing, which renders the same way) and mutated state. Pooled, they
    /// publish as `holds = 5`.
    fn records_differing_only_in_declaration() -> Vec<VerdictSummary> {
        let mut records = Vec::new();
        for _ in 0..2 {
            records.push(record(
                Annotation::ReadOnlyHint,
                Oracle::KernelChangeset,
                Outcome::Holds,
                "true",
            ));
        }
        for _ in 0..3 {
            records.push(record(
                Annotation::ReadOnlyHint,
                Oracle::KernelChangeset,
                Outcome::Holds,
                "false",
            ));
        }
        records
    }

    #[test]
    fn aggregate_never_merges_counts_across_oracles() {
        let rows = aggregate(&sample_records());
        assert_eq!(rows.len(), 2, "same annotation and outcome, different oracle, must stay two rows");

        let kernel = rows
            .iter()
            .find(|r| r.oracle == Oracle::KernelChangeset)
            .expect("kernel-changeset row present");
        assert_eq!(kernel.count, 5);

        let probe = rows.iter().find(|r| r.oracle == Oracle::ProtocolProbe).expect("protocol-probe row present");
        assert_eq!(probe.count, 3);

        assert!(
            rows.iter().all(|r| r.count != 8),
            "no row may report the pooled total of 8 — that would be exactly the mixing ADR-002 forbids"
        );
    }

    /// The `declared` axis, same shape as the oracle one: one oracle, one annotation, one
    /// outcome, two declarations must stay two rows. A single `holds = 5` here is the number
    /// that makes an unlimited supply of published `holds` rows available to any tool willing
    /// to declare nothing and write one path.
    #[test]
    fn aggregate_never_merges_counts_across_declared_values() {
        let rows = aggregate(&records_differing_only_in_declaration());
        assert_eq!(rows.len(), 2, "same annotation, oracle and outcome, different declaration");

        let claimed_read_only =
            rows.iter().find(|r| r.declared == "true").expect("declared-true row present");
        assert_eq!(claimed_read_only.count, 2);
        let admitted_writes =
            rows.iter().find(|r| r.declared == "false").expect("declared-false row present");
        assert_eq!(admitted_writes.count, 3);

        assert!(
            rows.iter().all(|r| r.count != 5),
            "no row may report the pooled total of 5 — a holds count that doesn't say what was \
             declared is uninterpretable"
        );
    }

    #[test]
    fn verify_report_matches_records_accepts_aggregates_own_output() {
        for records in [sample_records(), records_differing_only_in_declaration()] {
            let report = to_report_rows(&aggregate(&records));
            verify_report_matches_records(&report, &records)
                .expect("aggregate's own output must always verify");
        }
    }

    /// The literal B-03 exit criterion: a report that mixes oracles without separating
    /// them — here, a naive aggregator that ignored `oracle` entirely and reported one
    /// pooled count with no oracle disclosed — must fail verification.
    #[test]
    fn verify_report_matches_records_rejects_a_report_with_no_oracle_disclosed() {
        let records = sample_records();
        let naively_pooled_report = vec![ReportRow {
            annotation: Annotation::ReadOnlyHint,
            outcome: Outcome::Holds,
            oracle: None, // the bug: never says which oracle(s) contributed
            declared: Some("true".to_owned()),
            count: 8, // 5 kernel_changeset + 3 protocol_probe, silently pooled
        }];

        let err = verify_report_matches_records(&naively_pooled_report, &records)
            .expect_err("a report that never discloses its oracle must fail");
        assert_eq!(
            err,
            AggregationError::OracleNotDisclosed { annotation: Annotation::ReadOnlyHint, outcome: Outcome::Holds }
        );
    }

    /// A subtler version of the same bug: the report *does* attach an oracle label, but
    /// the count behind it is still the pooled total from both oracles — e.g. a report
    /// author who tagged the row `kernel_changeset` because that was the stronger, more
    /// "headline" oracle, while the number itself silently includes the weaker
    /// `protocol_probe` verdicts too. This must be caught exactly as surely as the
    /// undisclosed case above.
    #[test]
    fn verify_report_matches_records_rejects_a_pooled_count_mislabelled_under_one_oracle() {
        let records = sample_records();
        let mislabelled_report = vec![ReportRow {
            annotation: Annotation::ReadOnlyHint,
            outcome: Outcome::Holds,
            oracle: Some(Oracle::KernelChangeset),
            declared: Some("true".to_owned()),
            count: 8, // should be 5 for kernel_changeset alone
        }];

        let err = verify_report_matches_records(&mislabelled_report, &records)
            .expect_err("a count that doesn't match its disclosed oracle alone must fail");
        assert_eq!(
            err,
            AggregationError::CountMismatch {
                annotation: Annotation::ReadOnlyHint,
                oracle: Oracle::KernelChangeset,
                outcome: Outcome::Holds,
                declared: "true".to_owned(),
                reported: 8,
                actual: 5,
            }
        );
    }

    /// The `declared` axis's version of the undisclosed case: one oracle, correctly
    /// disclosed, but the row never says what the tools it counts had declared — so the
    /// reader cannot tell a verified read-only tool from one that admitted it writes.
    #[test]
    fn verify_report_matches_records_rejects_a_report_with_no_declaration_disclosed() {
        let records = records_differing_only_in_declaration();
        let pooled_over_declaration = vec![ReportRow {
            annotation: Annotation::ReadOnlyHint,
            outcome: Outcome::Holds,
            oracle: Some(Oracle::KernelChangeset),
            declared: None, // the bug
            count: 5,       // 2 declared true + 3 declared false, silently pooled
        }];

        let err = verify_report_matches_records(&pooled_over_declaration, &records)
            .expect_err("a report that never discloses the declaration must fail");
        assert_eq!(
            err,
            AggregationError::DeclaredNotDisclosed {
                annotation: Annotation::ReadOnlyHint,
                oracle: Oracle::KernelChangeset,
                outcome: Outcome::Holds,
            }
        );
    }

    /// And the mislabelled version: the row claims `declared = "true"` — the flattering
    /// reading, since a declared-`true` `holds` is the only one that means "verified
    /// read-only" — while the count behind it includes the declared-`false` rows too.
    #[test]
    fn verify_report_matches_records_rejects_a_pooled_count_mislabelled_under_one_declaration() {
        let records = records_differing_only_in_declaration();
        let mislabelled_report = vec![ReportRow {
            annotation: Annotation::ReadOnlyHint,
            outcome: Outcome::Holds,
            oracle: Some(Oracle::KernelChangeset),
            declared: Some("true".to_owned()),
            count: 5, // should be 2 for declared-true alone
        }];

        let err = verify_report_matches_records(&mislabelled_report, &records)
            .expect_err("a count that doesn't match its disclosed declaration alone must fail");
        assert_eq!(
            err,
            AggregationError::CountMismatch {
                annotation: Annotation::ReadOnlyHint,
                oracle: Oracle::KernelChangeset,
                outcome: Outcome::Holds,
                declared: "true".to_owned(),
                reported: 5,
                actual: 2,
            }
        );
    }

    #[test]
    fn verdict_row_converts_into_verdict_summary() {
        let row = VerdictRow {
            annotation: Annotation::IdempotentHint,
            oracle: Oracle::ProtocolProbe,
            outcome: Outcome::Unverifiable,
            declared: "false".to_owned(),
            ruleset_identity: None,
            derivation_version: None,
            counts: None,
            invocation_result: None,
            run_id: None,
        };
        let summary: VerdictSummary = row.into();
        assert_eq!(summary.annotation, Annotation::IdempotentHint);
        assert_eq!(summary.oracle, Oracle::ProtocolProbe);
        assert_eq!(summary.outcome, Outcome::Unverifiable);
        assert_eq!(summary.declared, "false");
    }
}
