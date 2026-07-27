//! B-03: aggregate verdicts for reporting, and guard against a report that mixes oracles
//! without separating them.
//!
//! ADR-002 (architecture.md §9): *"Requires that results never aggregate across oracles
//! without disclosure — an easy invariant to state and an easy one to violate in a summary
//! table."* This module is that invariant made checkable rather than a habit.
//! [`aggregate`] is the only correct way to build a report from raw verdicts; it cannot
//! produce a row that pools two oracles together, because `oracle` is part of the grouping
//! key by construction. [`verify_report_matches_records`] is the independent guard: given
//! *any* report — including one built some other way, by code that never saw this module —
//! it checks whether that report's counts could only have come from a single oracle per
//! row. The tests at the bottom construct a deliberately mixed report and prove it gets
//! rejected, which is the literal B-03 exit criterion: *"Any report that mixes oracles
//! without separating them fails a test."*

use std::collections::HashMap;

use datamodel::{Annotation, Oracle, Outcome};

use crate::db::VerdictRow;

/// One verdict, reduced to what aggregation needs to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VerdictSummary {
    /// Which annotation this verdict assesses.
    pub annotation: Annotation,
    /// Which oracle produced it.
    pub oracle: Oracle,
    /// The outcome.
    pub outcome: Outcome,
}

impl From<VerdictRow> for VerdictSummary {
    fn from(row: VerdictRow) -> Self {
        Self { annotation: row.annotation, oracle: row.oracle, outcome: row.outcome }
    }
}

/// One row in a published aggregate report. `oracle` is a plain, mandatory field — there is
/// no variant of this type and no constructor anywhere in this module that produces a row
/// without one, which is exactly the property ADR-002 wants: a count can never appear in a
/// report without saying which oracle it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregateRow {
    /// Which annotation this row summarises.
    pub annotation: Annotation,
    /// Which oracle every verdict counted in this row came from.
    pub oracle: Oracle,
    /// The outcome this row counts.
    pub outcome: Outcome,
    /// How many verdicts matched `(annotation, oracle, outcome)`.
    pub count: usize,
}

/// The only correct way to build a report from raw verdicts: groups strictly by
/// `(annotation, oracle, outcome)`. Two verdicts that agree on the first and third but
/// differ on oracle always land in different rows — there is no code path here that could
/// merge their counts.
///
/// Output is sorted for determinism (by each field's `Display` text — none of
/// `Annotation`/`Oracle`/`Outcome` derive `Ord`, and adding it just for this would be
/// scope creep on types that exist for a different reason), so two calls over the same
/// input always produce identical output, useful for snapshotting a published report.
#[must_use]
pub fn aggregate(records: &[VerdictSummary]) -> Vec<AggregateRow> {
    let mut counts: HashMap<(Annotation, Oracle, Outcome), usize> = HashMap::new();
    for r in records {
        *counts.entry((r.annotation, r.oracle, r.outcome)).or_insert(0) += 1;
    }

    let mut rows: Vec<AggregateRow> = counts
        .into_iter()
        .map(|((annotation, oracle, outcome), count)| AggregateRow { annotation, oracle, outcome, count })
        .collect();
    rows.sort_by_key(|r| (r.annotation.as_db_str(), r.oracle.as_db_str(), r.outcome.as_db_str()));
    rows
}

/// A row in an arbitrary report — the shape this guard checks, deliberately weaker than
/// [`AggregateRow`]: `oracle` is `Option`, because the entire point is to be able to
/// represent (and then reject) the buggy report a careless aggregator would actually
/// produce, where a count was pooled across oracles and the oracle that would have said so
/// was simply never written down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportRow {
    /// Which annotation this row claims to summarise.
    pub annotation: Annotation,
    /// The outcome this row claims to count.
    pub outcome: Outcome,
    /// The oracle this row claims its count came from — `None` if the report never says.
    pub oracle: Option<Oracle>,
    /// The claimed count.
    pub count: usize,
}

/// Why a report failed the cross-oracle guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// A row's claimed count does not match the number of source records that actually
    /// share `(annotation, oracle, outcome)` — the signature of a count that was pooled
    /// across more than one oracle and then mislabelled under a single one of them.
    CountMismatch {
        /// The row's annotation.
        annotation: Annotation,
        /// The oracle the row claims.
        oracle: Oracle,
        /// The row's outcome.
        outcome: Outcome,
        /// What the row claimed.
        reported: usize,
        /// What the source records actually support for that exact
        /// `(annotation, oracle, outcome)`.
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
            Self::CountMismatch { annotation, oracle, outcome, reported, actual } => write!(
                f,
                "report row for {annotation}/{oracle}/{outcome} claims count {reported}, but only \
                 {actual} source records match exactly that oracle — this count was built some \
                 other way than counting records of one oracle, which is the mixing ADR-002 forbids"
            ),
        }
    }
}

impl std::error::Error for AggregationError {}

/// Verify that every row in `report` is a straightforward, accurate count of source
/// `records` that all share a single, disclosed oracle.
///
/// This is deliberately independent of [`aggregate`] — it takes an arbitrary `report`, not
/// `aggregate`'s own output, so it can catch a report built by code that never went through
/// this module at all. A caller who always builds reports via [`aggregate`] will find this
/// always passes (proven below); the value is in what it rejects.
///
/// # Errors
///
/// The first row that either omits its oracle or whose count doesn't match exactly the
/// records sharing that oracle.
pub fn verify_report_matches_records(
    report: &[ReportRow],
    records: &[VerdictSummary],
) -> Result<(), AggregationError> {
    for row in report {
        let Some(oracle) = row.oracle else {
            return Err(AggregationError::OracleNotDisclosed { annotation: row.annotation, outcome: row.outcome });
        };
        let actual = records
            .iter()
            .filter(|r| r.annotation == row.annotation && r.oracle == oracle && r.outcome == row.outcome)
            .count();
        if actual != row.count {
            return Err(AggregationError::CountMismatch {
                annotation: row.annotation,
                oracle,
                outcome: row.outcome,
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
        .map(|r| ReportRow { annotation: r.annotation, outcome: r.outcome, oracle: Some(r.oracle), count: r.count })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(annotation: Annotation, oracle: Oracle, outcome: Outcome) -> VerdictSummary {
        VerdictSummary { annotation, oracle, outcome }
    }

    fn sample_records() -> Vec<VerdictSummary> {
        let mut records = Vec::new();
        for _ in 0..5 {
            records.push(record(Annotation::ReadOnlyHint, Oracle::KernelChangeset, Outcome::Holds));
        }
        for _ in 0..3 {
            records.push(record(Annotation::ReadOnlyHint, Oracle::ProtocolProbe, Outcome::Holds));
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

    #[test]
    fn verify_report_matches_records_accepts_aggregates_own_output() {
        let records = sample_records();
        let report = to_report_rows(&aggregate(&records));
        verify_report_matches_records(&report, &records).expect("aggregate's own output must always verify");
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
            count: 8,     // 5 kernel_changeset + 3 protocol_probe, silently pooled
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
                reported: 8,
                actual: 5,
            }
        );
    }

    #[test]
    fn verdict_row_converts_into_verdict_summary() {
        let row = VerdictRow { annotation: Annotation::IdempotentHint, oracle: Oracle::ProtocolProbe, outcome: Outcome::Unverifiable };
        let summary: VerdictSummary = row.into();
        assert_eq!(summary.annotation, Annotation::IdempotentHint);
        assert_eq!(summary.oracle, Oracle::ProtocolProbe);
        assert_eq!(summary.outcome, Outcome::Unverifiable);
    }
}
