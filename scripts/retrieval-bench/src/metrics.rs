//! Ranking metrics, and the reasons for the particular ones chosen.
//!
//! nDCG@k is the headline because relevance here is graded, not binary: a child
//! skill returned in its own right (gain 2) really is worth more than its parent
//! (gain 1), which is worth more than a miss. A set metric like precision would
//! flatten that distinction, and the distinction is what parent absorption is
//! for.
//!
//! Recall@k is reported beside it because they fail differently and the pair
//! localises the fault. nDCG can stay flat while recall drops, if the ranker
//! keeps finding one good card and quietly loses the second; recall can hold
//! while nDCG drops, if everything relevant is still present but ordered worse.
//! One number alone would hide whichever case it is not sensitive to.
//!
//! hit@1 is the blunt one, and it is here because it is the only metric that
//! corresponds to something the agent actually experiences: with a small
//! `retrieve_limit`, the top card is most of what gets read.

use std::collections::BTreeMap;

/// A graded relevance judgement: skill id to gain.
///
/// Deliberately a concrete type rather than a closure. nDCG needs the *ideal*
/// ordering to normalise against, which means it has to enumerate the gold set
/// and not merely probe it -- a lookup function cannot be asked what it would
/// say yes to.
#[derive(Debug, Clone, Default)]
pub struct Gold(pub BTreeMap<String, f64>);

impl Gold {
    pub fn gain(&self, id: &str) -> f64 {
        self.0.get(id).copied().unwrap_or(0.0)
    }

    /// Gains in descending order: the best ranking that was achievable.
    pub fn ideal_gains(&self) -> Vec<f64> {
        let mut gains: Vec<f64> = self.0.values().copied().filter(|g| *g > 0.0).collect();
        gains.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        gains
    }

    pub fn len(&self) -> usize {
        self.0.values().filter(|g| **g > 0.0).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Discounted cumulative gain: each gain divided by the log of its position.
fn dcg(gains: &[f64]) -> f64 {
    gains
        .iter()
        .enumerate()
        .map(|(i, g)| g / ((i + 2) as f64).log2())
        .sum()
}

/// nDCG@k against a graded gold set.
///
/// Normalised by the ideal ordering *truncated to k*, so a query with more
/// relevant skills than k can still score 1.0. Without the truncation, any case
/// whose gold set is deeper than the limit could never reach 1.0, and the
/// aggregate would drift with the shape of the gold file rather than with the
/// ranker -- adding a third relevant card to a case would look like a
/// regression.
pub fn ndcg_at_k(ranked: &[String], gold: &Gold, k: usize) -> f64 {
    let gains: Vec<f64> = ranked.iter().take(k).map(|id| gold.gain(id)).collect();
    let actual = dcg(&gains);

    let mut ideal = gold.ideal_gains();
    ideal.truncate(k);
    let best = dcg(&ideal);

    if best == 0.0 {
        0.0
    } else {
        actual / best
    }
}

/// Fraction of the gold set appearing in the top k, ignoring order.
///
/// Capped at k: if the gold set is deeper than the limit, retrieving every card
/// the limit allows counts as full recall. Otherwise the metric would punish the
/// ranker for a limit it does not choose.
pub fn recall_at_k(ranked: &[String], gold: &Gold, k: usize) -> f64 {
    let total = gold.len().min(k);
    if total == 0 {
        return 0.0;
    }
    let found = ranked.iter().take(k).filter(|id| gold.gain(id) > 0.0).count();
    found as f64 / total as f64
}

/// Whether the top-ranked item is relevant at all.
pub fn hit_at_1(ranked: &[String], gold: &Gold) -> f64 {
    match ranked.first() {
        Some(id) if gold.gain(id) > 0.0 => 1.0,
        _ => 0.0,
    }
}

/// Reciprocal rank of the first relevant item; 0 when none is found.
///
/// Sensitive to exactly what the aggregate metrics smooth away: a card slipping
/// from rank 1 to rank 3 barely moves nDCG but halves MRR.
pub fn reciprocal_rank(ranked: &[String], gold: &Gold) -> f64 {
    for (i, id) in ranked.iter().enumerate() {
        if gold.gain(id) > 0.0 {
            return 1.0 / (i + 1) as f64;
        }
    }
    0.0
}

/// Mean of a slice, or 0 for an empty one.
/// A paired bootstrap test over per-query scores.
///
/// Why this is not optional. Every arm answers the *same* queries, so the two
/// score vectors are paired and the interesting quantity is the mean of their
/// per-query differences. Reporting a bare delta invites the two mistakes this
/// benchmark exists to prevent: keeping a mechanism whose +0.004 is one lucky
/// query, and cutting one whose real gain hid under sampling noise.
///
/// Resampling queries with replacement makes no assumption about how nDCG is
/// distributed — it is bounded, discrete and badly skewed, so a t-test's
/// normality assumption does not hold.
///
/// Returns (mean difference, p-value, 95% confidence interval). The p-value is
/// two-sided: the fraction of resamples whose mean difference lands on the
/// opposite side of zero from the observed one, doubled.
pub fn paired_bootstrap(a: &[f64], b: &[f64], iterations: usize) -> (f64, f64, (f64, f64)) {
    let n = a.len().min(b.len());
    if n == 0 {
        return (0.0, 1.0, (0.0, 0.0));
    }
    let diffs: Vec<f64> = (0..n).map(|i| a[i] - b[i]).collect();
    let observed = mean(&diffs);

    // A fixed seed: a benchmark that reports a different p-value each run is
    // not a benchmark. xorshift64* is plenty for resampling and avoids a dep.
    let mut state: u64 = 0x9E3779B97F4A7C15;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state = state.wrapping_mul(0x2545F4914F6CDD1D);
        state
    };

    let mut means = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let mut acc = 0.0;
        for _ in 0..n {
            acc += diffs[(next() % n as u64) as usize];
        }
        means.push(acc / n as f64);
    }
    means.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));

    let tail = if observed >= 0.0 {
        means.iter().filter(|m| **m <= 0.0).count()
    } else {
        means.iter().filter(|m| **m >= 0.0).count()
    };
    let p = (2.0 * tail as f64 / iterations as f64).min(1.0);

    let lo = means[(iterations as f64 * 0.025) as usize];
    let hi = means[((iterations as f64 * 0.975) as usize).min(iterations - 1)];
    (observed, p, (lo, hi))
}

pub fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

/// Set-comparison counts for ToolRet, where relevance is binary and both
/// directions cost something.
#[derive(Debug, Default, Clone, Copy)]
pub struct SetScore {
    pub wanted_found: usize,
    pub wanted_total: usize,
    pub unwanted_attached: usize,
    pub unwanted_total: usize,
}

impl SetScore {
    /// Fraction of needed groups attached. 1.0 when nothing was needed: a query
    /// that requires no group cannot fail to have it attached, and scoring it 0
    /// would drag the mean down for a case that passed.
    pub fn recall(&self) -> f64 {
        if self.wanted_total == 0 {
            1.0
        } else {
            self.wanted_found as f64 / self.wanted_total as f64
        }
    }

    /// Fraction of the groups named as wrong that were correctly withheld.
    pub fn specificity(&self) -> f64 {
        if self.unwanted_total == 0 {
            1.0
        } else {
            1.0 - (self.unwanted_attached as f64 / self.unwanted_total as f64)
        }
    }

    /// Both sides at once, so one number can front the benchmark without
    /// "attach everything" scoring well. Harmonic, so a collapse on either side
    /// pulls the result down rather than being averaged away.
    pub fn f1(&self) -> f64 {
        let (r, s) = (self.recall(), self.specificity());
        if r + s == 0.0 {
            0.0
        } else {
            2.0 * r * s / (r + s)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gold(pairs: &[(&str, f64)]) -> Gold {
        Gold(pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect())
    }

    fn ids(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn dcg_discounts_by_position() {
        assert!(dcg(&[2.0, 0.0]) > dcg(&[0.0, 2.0]));
    }

    #[test]
    fn perfect_ranking_scores_one() {
        let g = gold(&[("a", 2.0), ("b", 1.0)]);
        assert_eq!(ndcg_at_k(&ids(&["a", "b"]), &g, 4), 1.0);
        assert_eq!(recall_at_k(&ids(&["a", "b"]), &g, 4), 1.0);
        assert_eq!(hit_at_1(&ids(&["a", "b"]), &g), 1.0);
    }

    #[test]
    fn wrong_order_scores_below_one_but_above_zero() {
        let g = gold(&[("a", 2.0), ("b", 1.0)]);
        let n = ndcg_at_k(&ids(&["b", "a"]), &g, 4);
        assert!(n > 0.0 && n < 1.0, "got {n}");
    }

    #[test]
    fn gold_deeper_than_k_can_still_score_one() {
        // Three relevant cards but room for one: retrieving the best is all the
        // ranker could have done, so it must not be marked down for the limit.
        let g = gold(&[("a", 2.0), ("b", 2.0), ("c", 1.0)]);
        assert_eq!(ndcg_at_k(&ids(&["a"]), &g, 1), 1.0);
        assert_eq!(recall_at_k(&ids(&["a"]), &g, 1), 1.0);
    }

    #[test]
    fn total_miss_scores_zero() {
        let g = gold(&[("a", 2.0)]);
        assert_eq!(ndcg_at_k(&ids(&["x", "y"]), &g, 4), 0.0);
        assert_eq!(hit_at_1(&ids(&["x"]), &g), 0.0);
        assert_eq!(reciprocal_rank(&ids(&["x"]), &g), 0.0);
    }

    #[test]
    fn reciprocal_rank_halves_at_rank_two() {
        let g = gold(&[("a", 1.0)]);
        assert_eq!(reciprocal_rank(&ids(&["x", "a"]), &g), 0.5);
    }

    #[test]
    fn attaching_everything_does_not_score_well() {
        // The property the F1 exists to enforce.
        let greedy = SetScore {
            wanted_found: 1,
            wanted_total: 1,
            unwanted_attached: 4,
            unwanted_total: 4,
        };
        assert_eq!(greedy.recall(), 1.0);
        assert_eq!(greedy.specificity(), 0.0);
        assert_eq!(greedy.f1(), 0.0);
    }

    #[test]
    fn empty_want_is_not_a_failure() {
        let s = SetScore {
            wanted_found: 0,
            wanted_total: 0,
            unwanted_attached: 0,
            unwanted_total: 3,
        };
        assert_eq!(s.recall(), 1.0);
        assert_eq!(s.f1(), 1.0);
    }
}

#[cfg(test)]
mod bootstrap_tests {
    use super::*;

    #[test]
    fn identical_arms_are_not_significant() {
        // The null case. Two arms that scored identically must never be reported
        // as different, and the interval must contain zero.
        let a = vec![0.5, 0.2, 0.9, 0.1, 0.7, 0.3];
        let (d, p, (lo, hi)) = paired_bootstrap(&a, &a, 2000);
        assert_eq!(d, 0.0);
        assert!(p > 0.9, "p was {p}");
        assert!(lo <= 0.0 && hi >= 0.0);
    }

    #[test]
    fn a_consistent_win_is_significant() {
        // Every query improves by 0.2: no resample can undo that.
        let a: Vec<f64> = (0..40).map(|i| 0.2 + (i % 5) as f64 * 0.1).collect();
        let b: Vec<f64> = a.iter().map(|x| x - 0.2).collect();
        let (d, p, (lo, _)) = paired_bootstrap(&a, &b, 2000);
        assert!((d - 0.2).abs() < 1e-9);
        assert!(p < 0.01, "p was {p}");
        assert!(lo > 0.0, "CI should exclude zero, got lo={lo}");
    }

    #[test]
    fn one_lucky_query_is_not_significant() {
        // Exactly the mistake this guards against: a single large win among
        // many ties moves the mean but must not clear the bar.
        let mut a = vec![0.4; 40];
        let mut b = vec![0.4; 40];
        a[0] = 1.0;
        b[0] = 0.0;
        let (d, p, (lo, hi)) = paired_bootstrap(&a, &b, 4000);
        assert!(d > 0.0);
        assert!(p > 0.05, "a single outlier should not be significant, p={p}");
        assert!(lo <= 0.0 && hi >= 0.0);
    }

    #[test]
    fn bootstrap_is_deterministic() {
        // A benchmark that reports a different p each run cannot be tracked.
        let a = vec![0.6, 0.3, 0.8, 0.2, 0.5];
        let b = vec![0.5, 0.4, 0.6, 0.3, 0.4];
        assert_eq!(paired_bootstrap(&a, &b, 1000), paired_bootstrap(&a, &b, 1000));
    }

    #[test]
    fn empty_input_is_safe() {
        let (d, p, _) = paired_bootstrap(&[], &[], 100);
        assert_eq!(d, 0.0);
        assert_eq!(p, 1.0);
    }
}
