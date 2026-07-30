//! B-03: aggregate verdicts for reporting, and guard against a report that mixes oracles
//! without separating them. Extended by P5-04 with the other two grouping dimensions its
//! exit criterion names ("rates by annotation, by containability class, and by oracle") and
//! with the two harness-maturity signals ADR-004 and P5-04's own checklist call for.
//!
//! ADR-002 (architecture.md §9): *"Requires that results never aggregate across oracles
//! without disclosure — an easy invariant to state and an easy one to violate in a summary
//! table."* This module is that invariant made checkable rather than a habit.
//! [`aggregate`] is the only correct way to build a report from raw verdicts; it cannot
//! produce a row that pools two oracles together, because `oracle` is part of the grouping
//! key by construction — and, as of P5-04, so is `containability_class`, for the same
//! reason: a rate "by containability class" that silently pooled Class A and Class B
//! results together would be exactly as misleading as pooling oracles, just along a
//! different axis. [`verify_report_matches_records`] is the independent guard: given *any*
//! report — including one built some other way, by code that never saw this module — it
//! checks whether that report's counts could only have come from a single oracle per row.
//! The tests at the bottom construct a deliberately mixed report and prove it gets
//! rejected, which is the literal B-03 exit criterion: *"Any report that mixes oracles
//! without separating them fails a test."*
//!
//! **Two distinct "how often can't this harness decide" signals, kept separate rather than
//! folded into one "no-verdict" number (P5-04's own checklist item, ADR-004's own
//! prediction):** [`unverifiable_rate`] is the fraction of *produced* verdicts whose
//! outcome is [`Outcome::Unverifiable`] — ADR-004's literal subject ("a truncated,
//! timed-out, or resource-capped run... [produces] `unverifiable` with a reason code...
//! [that] fraction is itself a reported metric"). [`snapshot_coverage`] is a different
//! question — how many discovered, pinned tools never got as far as producing *any*
//! verdict at all, decisive or not (excluded by containability class, never reached by a
//! run planner, ...). `datamodel::Outcome`'s own doc comment already insists `Unverifiable`
//! "is a first-class verdict, not an error path" — collapsing these two numbers into one
//! would blur exactly that distinction this codebase has otherwise been careful to keep.

use std::collections::HashMap;

use datamodel::{Annotation, ContainabilityClass, Oracle, Outcome};

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
    /// The containability class of the server whose tool this verdict is about — P5-04's
    /// second grouping dimension, alongside `annotation` and `oracle`.
    pub containability_class: ContainabilityClass,
}

impl From<VerdictRow> for VerdictSummary {
    fn from(row: VerdictRow) -> Self {
        Self {
            annotation: row.annotation,
            oracle: row.oracle,
            outcome: row.outcome,
            containability_class: row.containability_class,
        }
    }
}

/// One row in a published aggregate report. `oracle` is a plain, mandatory field — there is
/// no variant of this type and no constructor anywhere in this module that produces a row
/// without one, which is exactly the property ADR-002 wants: a count can never appear in a
/// report without saying which oracle it came from. `containability_class` is equally
/// mandatory, for the matching reason stated in this module's own doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregateRow {
    /// Which annotation this row summarises.
    pub annotation: Annotation,
    /// The containability class every verdict counted in this row came from.
    pub containability_class: ContainabilityClass,
    /// Which oracle every verdict counted in this row came from.
    pub oracle: Oracle,
    /// The outcome this row counts.
    pub outcome: Outcome,
    /// How many verdicts matched `(annotation, containability_class, oracle, outcome)`.
    pub count: usize,
}

/// The only correct way to build a report from raw verdicts: groups strictly by
/// `(annotation, containability_class, oracle, outcome)`. Two verdicts that agree on three
/// of these four but differ on any one always land in different rows — there is no code
/// path here that could merge their counts.
///
/// Output is sorted for determinism (by each field's `Display` text — none of
/// `Annotation`/`ContainabilityClass`/`Oracle`/`Outcome` derive `Ord`, and adding it just
/// for this would be scope creep on types that exist for a different reason), so two calls
/// over the same input always produce identical output, useful for snapshotting a published
/// report.
#[must_use]
pub fn aggregate(records: &[VerdictSummary]) -> Vec<AggregateRow> {
    let mut counts: HashMap<(Annotation, ContainabilityClass, Oracle, Outcome), usize> = HashMap::new();
    for r in records {
        *counts.entry((r.annotation, r.containability_class, r.oracle, r.outcome)).or_insert(0) += 1;
    }

    let mut rows: Vec<AggregateRow> = counts
        .into_iter()
        .map(|((annotation, containability_class, oracle, outcome), count)| AggregateRow {
            annotation,
            containability_class,
            oracle,
            outcome,
            count,
        })
        .collect();
    rows.sort_by_key(|r| {
        (r.annotation.as_db_str(), r.containability_class.as_db_str(), r.oracle.as_db_str(), r.outcome.as_db_str())
    });
    rows
}

/// One outcome's rate within its `(annotation, containability_class, oracle)` bucket —
/// P5-04's literal exit criterion ("rates by annotation, by containability class, and by
/// oracle"), computed from [`AggregateRow`] counts rather than re-deriving them from raw
/// records, so a rate can never disagree with the count it was computed from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateRow {
    /// Which annotation this rate is about.
    pub annotation: Annotation,
    /// Which containability class this rate is about.
    pub containability_class: ContainabilityClass,
    /// Which oracle produced every verdict this rate is computed over.
    pub oracle: Oracle,
    /// The outcome this rate is the proportion of.
    pub outcome: Outcome,
    /// How many verdicts in this bucket had this outcome.
    pub count: usize,
    /// How many verdicts exist in total for this bucket's `(annotation,
    /// containability_class, oracle)`, across every outcome.
    pub bucket_total: usize,
    /// `count as f64 / bucket_total as f64`.
    pub rate: f64,
}

/// Derive per-outcome rates from `rows` — one [`RateRow`] per input [`AggregateRow`], each
/// carrying its own bucket's total alongside it so a reader never has to re-sum the input to
/// interpret a single rate in isolation.
#[must_use]
pub fn rates(rows: &[AggregateRow]) -> Vec<RateRow> {
    let mut bucket_totals: HashMap<(Annotation, ContainabilityClass, Oracle), usize> = HashMap::new();
    for r in rows {
        *bucket_totals.entry((r.annotation, r.containability_class, r.oracle)).or_insert(0) += r.count;
    }

    rows.iter()
        .map(|r| {
            let bucket_total = bucket_totals[&(r.annotation, r.containability_class, r.oracle)];
            #[allow(clippy::cast_precision_loss)] // report-scale counts, not precision-critical
            let rate = if bucket_total == 0 { 0.0 } else { r.count as f64 / bucket_total as f64 };
            RateRow {
                annotation: r.annotation,
                containability_class: r.containability_class,
                oracle: r.oracle,
                outcome: r.outcome,
                count: r.count,
                bucket_total,
                rate,
            }
        })
        .collect()
}

/// ADR-004's own literal subject: the fraction of `records` whose outcome is
/// [`Outcome::Unverifiable`] — "a truncated, timed-out, or resource-capped run... [produces]
/// `unverifiable`... that fraction is itself a reported metric and a useful signal about
/// harness maturity." `0.0` for an empty slice, not `NaN` — an empty corpus has no
/// unverifiable fraction to report, and `0.0` is a more useful default for a caller
/// formatting this straight into a report than a value that would corrupt any arithmetic
/// downstream.
#[must_use]
pub fn unverifiable_rate(records: &[VerdictSummary]) -> f64 {
    if records.is_empty() {
        return 0.0;
    }
    let unverifiable = records.iter().filter(|r| r.outcome == Outcome::Unverifiable).count();
    #[allow(clippy::cast_precision_loss)]
    let rate = unverifiable as f64 / records.len() as f64;
    rate
}

/// How much of the discovered, pinned tool corpus this harness actually managed to assess
/// at all — deliberately *not* the same question [`unverifiable_rate`] answers (see this
/// module's own doc comment). A snapshot counts as "with a verdict" the moment it has even
/// one `VERDICT` row, decisive or not.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SnapshotCoverage {
    /// Every `TOOL_SNAPSHOT` row that exists.
    pub total_snapshots: usize,
    /// How many of those have at least one `VERDICT` row.
    pub snapshots_with_a_verdict: usize,
    /// `(total_snapshots - snapshots_with_a_verdict) / total_snapshots`. `0.0` for an empty
    /// corpus, same reasoning as [`unverifiable_rate`].
    pub no_verdict_fraction: f64,
}

/// Compute [`SnapshotCoverage`] from the two counts a caller reads out of the metadata DB
/// (`store::db::count_tool_snapshots`/`count_tool_snapshots_with_a_verdict`) — kept as a
/// pure function of those two integers, not a DB-touching one itself, the same split this
/// crate already draws elsewhere between raw persistence and the reporting logic built on
/// top of it.
#[must_use]
pub fn snapshot_coverage(total_snapshots: usize, snapshots_with_a_verdict: usize) -> SnapshotCoverage {
    #[allow(clippy::cast_precision_loss)]
    let no_verdict_fraction = if total_snapshots == 0 {
        0.0
    } else {
        (total_snapshots - snapshots_with_a_verdict) as f64 / total_snapshots as f64
    };
    SnapshotCoverage { total_snapshots, snapshots_with_a_verdict, no_verdict_fraction }
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
    /// Which containability class this row claims to summarise.
    pub containability_class: ContainabilityClass,
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
        /// The row's containability class.
        containability_class: ContainabilityClass,
        /// The row's outcome.
        outcome: Outcome,
    },
    /// A row's claimed count does not match the number of source records that actually
    /// share `(annotation, containability_class, oracle, outcome)` — the signature of a
    /// count that was pooled across more than one oracle (or class) and then mislabelled
    /// under a single one of them.
    CountMismatch {
        /// The row's annotation.
        annotation: Annotation,
        /// The row's containability class.
        containability_class: ContainabilityClass,
        /// The oracle the row claims.
        oracle: Oracle,
        /// The row's outcome.
        outcome: Outcome,
        /// What the row claimed.
        reported: usize,
        /// What the source records actually support for that exact
        /// `(annotation, containability_class, oracle, outcome)`.
        actual: usize,
    },
}

impl std::fmt::Display for AggregationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OracleNotDisclosed { annotation, containability_class, outcome } => write!(
                f,
                "report row for {annotation}/{containability_class}/{outcome} does not disclose an \
                 oracle — ADR-002 forbids this"
            ),
            Self::CountMismatch { annotation, containability_class, oracle, outcome, reported, actual } => write!(
                f,
                "report row for {annotation}/{containability_class}/{oracle}/{outcome} claims count \
                 {reported}, but only {actual} source records match exactly that bucket — this count \
                 was built some other way than counting records of one oracle and class, which is the \
                 mixing ADR-002 forbids"
            ),
        }
    }
}

impl std::error::Error for AggregationError {}

/// Verify that every row in `report` is a straightforward, accurate count of source
/// `records` that all share a single, disclosed oracle (and the row's own containability
/// class).
///
/// This is deliberately independent of [`aggregate`] — it takes an arbitrary `report`, not
/// `aggregate`'s own output, so it can catch a report built by code that never went through
/// this module at all. A caller who always builds reports via [`aggregate`] will find this
/// always passes (proven below); the value is in what it rejects.
///
/// # Errors
///
/// The first row that either omits its oracle or whose count doesn't match exactly the
/// records sharing that oracle and containability class.
pub fn verify_report_matches_records(
    report: &[ReportRow],
    records: &[VerdictSummary],
) -> Result<(), AggregationError> {
    for row in report {
        let Some(oracle) = row.oracle else {
            return Err(AggregationError::OracleNotDisclosed {
                annotation: row.annotation,
                containability_class: row.containability_class,
                outcome: row.outcome,
            });
        };
        let actual = records
            .iter()
            .filter(|r| {
                r.annotation == row.annotation
                    && r.containability_class == row.containability_class
                    && r.oracle == oracle
                    && r.outcome == row.outcome
            })
            .count();
        if actual != row.count {
            return Err(AggregationError::CountMismatch {
                annotation: row.annotation,
                containability_class: row.containability_class,
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
        .map(|r| ReportRow {
            annotation: r.annotation,
            containability_class: r.containability_class,
            outcome: r.outcome,
            oracle: Some(r.oracle),
            count: r.count,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        annotation: Annotation,
        containability_class: ContainabilityClass,
        oracle: Oracle,
        outcome: Outcome,
    ) -> VerdictSummary {
        VerdictSummary { annotation, containability_class, oracle, outcome }
    }

    fn sample_records() -> Vec<VerdictSummary> {
        let mut records = Vec::new();
        for _ in 0..5 {
            records.push(record(
                Annotation::ReadOnlyHint,
                ContainabilityClass::A,
                Oracle::KernelChangeset,
                Outcome::Holds,
            ));
        }
        for _ in 0..3 {
            records.push(record(
                Annotation::ReadOnlyHint,
                ContainabilityClass::B,
                Oracle::ProtocolProbe,
                Outcome::Holds,
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

    /// P5-04's own extension: the same never-merge guarantee, but across containability
    /// class instead of oracle — two verdicts that would otherwise land in the same bucket
    /// (same annotation, oracle, outcome) but come from different classes must stay
    /// separate rows too.
    #[test]
    fn aggregate_never_merges_counts_across_containability_class() {
        let records = vec![
            record(Annotation::ReadOnlyHint, ContainabilityClass::A, Oracle::KernelChangeset, Outcome::Violated),
            record(Annotation::ReadOnlyHint, ContainabilityClass::A, Oracle::KernelChangeset, Outcome::Violated),
            record(Annotation::ReadOnlyHint, ContainabilityClass::B, Oracle::KernelChangeset, Outcome::Violated),
        ];
        let rows = aggregate(&records);
        assert_eq!(rows.len(), 2, "same annotation, oracle, and outcome, different class, must stay two rows");
        let class_a = rows.iter().find(|r| r.containability_class == ContainabilityClass::A).expect("class A row");
        assert_eq!(class_a.count, 2);
        let class_b = rows.iter().find(|r| r.containability_class == ContainabilityClass::B).expect("class B row");
        assert_eq!(class_b.count, 1);
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
            containability_class: ContainabilityClass::A,
            outcome: Outcome::Holds,
            oracle: None, // the bug: never says which oracle(s) contributed
            count: 8,     // 5 kernel_changeset + 3 protocol_probe, silently pooled
        }];

        let err = verify_report_matches_records(&naively_pooled_report, &records)
            .expect_err("a report that never discloses its oracle must fail");
        assert_eq!(
            err,
            AggregationError::OracleNotDisclosed {
                annotation: Annotation::ReadOnlyHint,
                containability_class: ContainabilityClass::A,
                outcome: Outcome::Holds
            }
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
            containability_class: ContainabilityClass::A,
            outcome: Outcome::Holds,
            oracle: Some(Oracle::KernelChangeset),
            count: 8, // should be 5 for kernel_changeset/class-A alone
        }];

        let err = verify_report_matches_records(&mislabelled_report, &records)
            .expect_err("a count that doesn't match its disclosed oracle alone must fail");
        assert_eq!(
            err,
            AggregationError::CountMismatch {
                annotation: Annotation::ReadOnlyHint,
                containability_class: ContainabilityClass::A,
                oracle: Oracle::KernelChangeset,
                outcome: Outcome::Holds,
                reported: 8,
                actual: 5,
            }
        );
    }

    #[test]
    fn verdict_row_converts_into_verdict_summary() {
        let row = VerdictRow {
            annotation: Annotation::IdempotentHint,
            oracle: Oracle::ProtocolProbe,
            outcome: Outcome::Unverifiable,
            containability_class: ContainabilityClass::B,
        };
        let summary: VerdictSummary = row.into();
        assert_eq!(summary.annotation, Annotation::IdempotentHint);
        assert_eq!(summary.oracle, Oracle::ProtocolProbe);
        assert_eq!(summary.outcome, Outcome::Unverifiable);
        assert_eq!(summary.containability_class, ContainabilityClass::B);
    }

    // ---- Rates ----

    #[test]
    fn rates_computes_the_proportion_within_each_bucket() {
        let records = vec![
            record(Annotation::ReadOnlyHint, ContainabilityClass::A, Oracle::KernelChangeset, Outcome::Holds),
            record(Annotation::ReadOnlyHint, ContainabilityClass::A, Oracle::KernelChangeset, Outcome::Holds),
            record(Annotation::ReadOnlyHint, ContainabilityClass::A, Oracle::KernelChangeset, Outcome::Holds),
            record(Annotation::ReadOnlyHint, ContainabilityClass::A, Oracle::KernelChangeset, Outcome::Violated),
        ];
        let rate_rows = rates(&aggregate(&records));
        assert_eq!(rate_rows.len(), 2);

        let holds = rate_rows.iter().find(|r| r.outcome == Outcome::Holds).expect("holds row");
        assert_eq!(holds.count, 3);
        assert_eq!(holds.bucket_total, 4);
        assert!((holds.rate - 0.75).abs() < f64::EPSILON);

        let violated = rate_rows.iter().find(|r| r.outcome == Outcome::Violated).expect("violated row");
        assert_eq!(violated.count, 1);
        assert_eq!(violated.bucket_total, 4);
        assert!((violated.rate - 0.25).abs() < f64::EPSILON);
    }

    /// A different oracle for the same annotation/class is a different bucket — rates must
    /// never blend the two denominators together, the same guarantee `aggregate` itself
    /// already gives the raw counts.
    #[test]
    fn rates_keep_separate_buckets_across_oracle_and_class() {
        let rate_rows = rates(&aggregate(&sample_records()));
        // Both sample buckets are pure single-outcome buckets (all `Holds`): each rate must
        // be exactly 1.0, never diluted by the other bucket's records.
        for row in &rate_rows {
            assert!((row.rate - 1.0).abs() < f64::EPSILON, "{row:?} must be entirely Holds within its own bucket");
        }
    }

    // ---- Unverifiable rate (ADR-004) ----

    #[test]
    fn unverifiable_rate_over_a_mixed_corpus() {
        let records = vec![
            record(Annotation::ReadOnlyHint, ContainabilityClass::A, Oracle::KernelChangeset, Outcome::Holds),
            record(Annotation::ReadOnlyHint, ContainabilityClass::A, Oracle::KernelChangeset, Outcome::Violated),
            record(
                Annotation::ReadOnlyHint,
                ContainabilityClass::A,
                Oracle::KernelChangeset,
                Outcome::Unverifiable,
            ),
            record(
                Annotation::ReadOnlyHint,
                ContainabilityClass::A,
                Oracle::KernelChangeset,
                Outcome::Unverifiable,
            ),
        ];
        assert!((unverifiable_rate(&records) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn unverifiable_rate_of_an_empty_corpus_is_zero_not_nan() {
        assert_eq!(unverifiable_rate(&[]), 0.0);
    }

    #[test]
    fn unverifiable_rate_of_an_entirely_decisive_corpus_is_zero() {
        assert_eq!(unverifiable_rate(&sample_records()), 0.0);
    }

    // ---- Snapshot coverage (distinct from unverifiable_rate) ----

    #[test]
    fn snapshot_coverage_computes_the_no_verdict_fraction() {
        let coverage = snapshot_coverage(10, 4);
        assert_eq!(coverage.total_snapshots, 10);
        assert_eq!(coverage.snapshots_with_a_verdict, 4);
        assert!((coverage.no_verdict_fraction - 0.6).abs() < f64::EPSILON);
    }

    #[test]
    fn snapshot_coverage_of_an_empty_corpus_is_zero_not_nan() {
        let coverage = snapshot_coverage(0, 0);
        assert_eq!(coverage.no_verdict_fraction, 0.0);
    }

    #[test]
    fn full_snapshot_coverage_has_a_zero_no_verdict_fraction() {
        let coverage = snapshot_coverage(5, 5);
        assert_eq!(coverage.no_verdict_fraction, 0.0);
    }
}
