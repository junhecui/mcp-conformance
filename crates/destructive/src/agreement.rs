//! Q-02/Q-04: Cohen's kappa, the inter-rater agreement statistic `docs/labelling_protocol.md`
//! specifies. Pure arithmetic over two label sequences — no I/O, no opinion about who the
//! raters are or what the labels mean; that is entirely `docs/labelling_protocol.md`'s job.
//!
//! **What this module is not:** a source of labelled data. Nothing here produces or invents
//! a label — it only scores two label sequences a caller already has. Q-02's own status is
//! "protocol specified, this calculator built and tested against known-correct arithmetic;
//! no real labelling has happened," and this module cannot change that by itself.
//!
//! Generic over the label type (`T: Eq + Hash + Clone`) rather than hardcoded to the
//! `Destructive`/`Additive`/`Ambiguous` three-category label set `docs/labelling_protocol.md`
//! specifies for Q-02 — Q-04's own exit criterion ("agreement between mechanical proxy,
//! model classification, and human labels") is naturally three pairwise calls to this same
//! function over whatever category types those three sources each use, not a second
//! statistic built solely for Q-02.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// Why [`cohens_kappa`] could not be computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgreementError {
    /// The two raters labelled different numbers of items — they must be scored over
    /// exactly the same item sequence, in the same order, for a pairwise comparison to mean
    /// anything.
    MismatchedRaterCounts {
        /// How many labels the first rater's sequence had.
        rater_a_items: usize,
        /// How many labels the second rater's sequence had.
        rater_b_items: usize,
    },
    /// No items to score at all.
    NoItems,
}

impl std::fmt::Display for AgreementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MismatchedRaterCounts { rater_a_items, rater_b_items } => write!(
                f,
                "rater sequences must be the same length: rater A labelled {rater_a_items} \
                 items, rater B labelled {rater_b_items}"
            ),
            Self::NoItems => write!(f, "cannot compute agreement over zero items"),
        }
    }
}

impl std::error::Error for AgreementError {}

/// Cohen's kappa: `(p_o - p_e) / (1 - p_e)`, where `p_o` is the two raters' observed
/// agreement rate and `p_e` is the agreement rate expected by chance alone, given each
/// rater's own marginal label distribution.
///
/// `rater_a[i]` and `rater_b[i]` must be the same item, for every `i` — this function has no
/// way to check that itself (it only ever sees the labels, never an item identity), so
/// getting the two sequences into matching order is entirely the caller's responsibility.
///
/// Returns `1.0` in the degenerate case where `p_e` would otherwise be `1.0` (every label,
/// from both raters, across every item, is the same single category) rather than the
/// mathematically-undefined `0.0 / 0.0` — in that configuration `p_o` is also necessarily
/// `1.0`, so "perfect agreement" is the only value consistent with the inputs.
///
/// # Errors
/// [`AgreementError::MismatchedRaterCounts`] if the two sequences differ in length;
/// [`AgreementError::NoItems`] if both are empty.
pub fn cohens_kappa<T: Eq + Hash + Clone>(rater_a: &[T], rater_b: &[T]) -> Result<f64, AgreementError> {
    if rater_a.len() != rater_b.len() {
        return Err(AgreementError::MismatchedRaterCounts {
            rater_a_items: rater_a.len(),
            rater_b_items: rater_b.len(),
        });
    }
    if rater_a.is_empty() {
        return Err(AgreementError::NoItems);
    }

    #[allow(clippy::cast_precision_loss)] // item counts, not precision-critical at this scale
    let n = rater_a.len() as f64;

    let observed_agreement = rater_a.iter().zip(rater_b.iter()).filter(|(a, b)| a == b).count();
    #[allow(clippy::cast_precision_loss)]
    let p_o = observed_agreement as f64 / n;

    let mut counts_a: HashMap<T, usize> = HashMap::new();
    let mut counts_b: HashMap<T, usize> = HashMap::new();
    for label in rater_a {
        *counts_a.entry(label.clone()).or_insert(0) += 1;
    }
    for label in rater_b {
        *counts_b.entry(label.clone()).or_insert(0) += 1;
    }

    let categories: HashSet<T> = counts_a.keys().chain(counts_b.keys()).cloned().collect();
    let p_e: f64 = categories
        .iter()
        .map(|category| {
            #[allow(clippy::cast_precision_loss)]
            let a_fraction = *counts_a.get(category).unwrap_or(&0) as f64 / n;
            #[allow(clippy::cast_precision_loss)]
            let b_fraction = *counts_b.get(category).unwrap_or(&0) as f64 / n;
            a_fraction * b_fraction
        })
        .sum();

    if (1.0 - p_e).abs() < f64::EPSILON {
        return Ok(1.0);
    }
    Ok((p_o - p_e) / (1.0 - p_e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A worked example, checked by hand against the formula itself (not sourced from any
    /// particular textbook, just arithmetic that can be independently re-derived from the
    /// confusion matrix below): two raters label 100 items `"yes"`/`"no"`. Confusion matrix
    /// (rows = rater A, columns = rater B):
    ///
    /// |        | B: yes | B: no | row total |
    /// |--------|--------|-------|-----------|
    /// | A: yes |   20   |   5   |    25     |
    /// | A: no  |   10   |  65   |    75     |
    /// | col tot|   30   |  70   |   100     |
    ///
    /// `p_o = (20 + 65) / 100 = 0.85`. `p_e = (25/100)(30/100) + (75/100)(70/100) = 0.075 +
    /// 0.525 = 0.6`. `kappa = (0.85 - 0.6) / (1 - 0.6) = 0.25 / 0.4 = 0.625`.
    #[test]
    fn a_worked_example_matches_hand_computed_kappa() {
        let mut rater_a = Vec::new();
        let mut rater_b = Vec::new();
        for _ in 0..20 {
            rater_a.push("yes");
            rater_b.push("yes");
        }
        for _ in 0..5 {
            rater_a.push("yes");
            rater_b.push("no");
        }
        for _ in 0..10 {
            rater_a.push("no");
            rater_b.push("yes");
        }
        for _ in 0..65 {
            rater_a.push("no");
            rater_b.push("no");
        }
        assert_eq!(rater_a.len(), 100);

        let kappa = cohens_kappa(&rater_a, &rater_b).expect("cohens_kappa");
        assert!((kappa - 0.625).abs() < 1e-9, "expected kappa 0.625, got {kappa}");
    }

    #[test]
    fn perfect_agreement_is_kappa_one() {
        let labels = vec!["Destructive", "Additive", "Ambiguous", "Additive", "Destructive"];
        let kappa = cohens_kappa(&labels, &labels).expect("cohens_kappa");
        assert!((kappa - 1.0).abs() < 1e-9);
    }

    /// Agreement exactly at the rate chance alone would predict (both raters' marginals are
    /// 50/50, and half their agreements are attributable to that alone) must score `0.0`.
    #[test]
    fn agreement_at_exactly_the_chance_rate_is_kappa_zero() {
        // 4 items, both raters split 2/2 on "a"/"b", and observed agreement (2 out of 4)
        // exactly equals what the marginals alone predict: p_e = 0.5*0.5 + 0.5*0.5 = 0.5,
        // p_o = 2/4 = 0.5, kappa = (0.5 - 0.5) / (1 - 0.5) = 0.
        let rater_a = vec!["a", "a", "b", "b"];
        let rater_b = vec!["a", "b", "a", "b"];
        let kappa = cohens_kappa(&rater_a, &rater_b).expect("cohens_kappa");
        assert!(kappa.abs() < 1e-9, "expected kappa 0.0, got {kappa}");
    }

    /// A three-category label set (`docs/labelling_protocol.md`'s actual
    /// `Destructive`/`Additive`/`Ambiguous`) works exactly the same way as the two-category
    /// worked example above — the formula has no special case for exactly two categories.
    #[test]
    fn three_category_labels_are_supported() {
        #[derive(Clone, PartialEq, Eq, Hash)]
        enum Label {
            Destructive,
            Additive,
            Ambiguous,
        }
        let rater_a =
            vec![Label::Destructive, Label::Additive, Label::Ambiguous, Label::Destructive];
        let rater_b = vec![Label::Destructive, Label::Additive, Label::Additive, Label::Ambiguous];
        // Two of four agree exactly (Destructive, Additive); the other two disagree in two
        // different ways (Ambiguous vs Additive, Destructive vs Ambiguous) — just needs to
        // run without panicking and land in the valid kappa range for this assertion's
        // purposes; the two-category tests above already pin the formula down exactly.
        let kappa = cohens_kappa(&rater_a, &rater_b).expect("cohens_kappa");
        assert!((-1.0..=1.0).contains(&kappa));
    }

    #[test]
    fn every_label_identical_is_the_degenerate_perfect_agreement_case() {
        let labels = vec!["Additive", "Additive", "Additive"];
        let kappa = cohens_kappa(&labels, &labels).expect("cohens_kappa");
        assert!((kappa - 1.0).abs() < 1e-9, "p_e == 1.0 with p_o == 1.0 must report perfect agreement");
    }

    #[test]
    fn mismatched_lengths_is_a_named_error_not_a_panic() {
        let rater_a = vec!["a", "b"];
        let rater_b = vec!["a"];
        let err = cohens_kappa(&rater_a, &rater_b).expect_err("must fail");
        assert_eq!(err, AgreementError::MismatchedRaterCounts { rater_a_items: 2, rater_b_items: 1 });
    }

    #[test]
    fn empty_input_is_a_named_error_not_a_panic() {
        let empty: Vec<&str> = Vec::new();
        let err = cohens_kappa(&empty, &empty).expect_err("must fail");
        assert_eq!(err, AgreementError::NoItems);
    }
}
