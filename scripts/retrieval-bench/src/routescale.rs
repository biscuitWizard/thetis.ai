//! Tool-group routing measured at scale, against dataset-derived labels.
//!
//! `toolret.rs` measures routing on 34 hand-written cases. That is enough to
//! catch a broken table and far too few to resolve a small regression: at n=34 a
//! single case is worth 3 points of recall. This module runs the same decision
//! over ~1,600 queries whose labels come from ToolRet rather than from me.
//!
//! What is ground truth here and what is not, stated plainly because it decides
//! how much the number is worth:
//!
//! - **Ground truth:** which *tools* answer a query. ToolRet ships per-tool
//!   relevance judgements; I did not write them.
//! - **Mine:** which *bucket* each tool belongs to. The buckets are synthesised
//!   by `fetch-toolret.py` from keyword rules over tool documentation.
//!
//! So the query-to-group labels are *derived* — gold tools mapped through my
//! bucketing — not invented per query. That is a real improvement over hand
//! authoring, because I cannot tune a label to a query I never looked at, and
//! the mapping is one rule applied uniformly to 9,529 tools. It is still not
//! independent of me, and a bucketing mistake shows up as a routing error. The
//! honest use of this number is as a *sensitive relative* measure: it compares
//! thresholds and tag sets on equal footing over a large sample.
//!
//! The scoring itself is the shipped code: `table::tokens`, `table::score` and
//! `table::tag_present`, lifted unmodified. Only the group table is substituted,
//! which is the one thing that has to change to talk about a different domain.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

use crate::corpus::ToolRetFile;
use crate::lifted::table::{self, ToolGroup};
use crate::metrics::{self, SetScore};

/// One routing decision, kept for the worst-case report.
#[derive(Debug, Clone, Serialize)]
pub struct CaseResult {
    pub query: String,
    pub want: Vec<String>,
    pub attached: Vec<String>,
    pub missed: Vec<String>,
    pub spurious: Vec<String>,
    pub f1: f64,
}

/// One threshold's worth of results.
#[derive(Debug, Clone, Serialize)]
pub struct ThresholdPoint {
    pub threshold: f64,
    pub recall: f64,
    pub specificity: f64,
    pub f1: f64,
    /// Mean groups attached per query. The cost side: every attached group's
    /// tools land in the prompt, so this is roughly what routing spends.
    pub mean_attached: f64,
    /// Queries where nothing at all was attached. A routing failure that a
    /// recall average hides, because the agent is left with no tools.
    pub unrouted: usize,
}

/// One routing *strategy*, as opposed to one threshold of the shipped one.
///
/// This is the part that answers "does the mechanism pay for itself": tag
/// matching is free, embedding the query costs a call, and the difference
/// between them is what that call buys.
#[derive(Debug, Clone, Serialize)]
pub struct StrategyResult {
    pub strategy: String,
    /// What it costs per query, in plain words, so the graph can label it.
    pub cost: String,
    pub recall: f64,
    pub specificity: f64,
    pub f1: f64,
    pub mean_attached: f64,
    pub unrouted: usize,
}

/// One dataset family, scored under the shipped strategy and the best
/// alternative, so breadth of the win is visible rather than assumed.
#[derive(Debug, Clone, Serialize)]
pub struct SubsetResult {
    pub subset: String,
    pub queries: usize,
    pub tags_f1: f64,
    pub tags_unrouted: usize,
    /// Absent when the run had no embedder, so there is no alternative to show.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_f1: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_strategy: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RouteScale {
    pub queries: usize,
    pub groups: usize,
    pub tools: usize,
    /// Queries dropped because no relevant tool mapped into any bucket. Reported
    /// rather than silently excluded: it bounds how much of the corpus this can
    /// actually speak for.
    pub unlabelled: usize,
    pub shipped_threshold: f64,
    pub sweep: Vec<ThresholdPoint>,
    /// The sweep point at the shipped threshold, for the headline.
    pub at_shipped: ThresholdPoint,
    /// Best F1 over the sweep, and where.
    pub best_f1: f64,
    pub best_threshold: f64,
    /// Strategy comparison at the shipped threshold. Empty when run without
    /// embeddings, since every alternative here needs a query vector.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub strategies: Vec<StrategyResult>,
    /// The same comparison split by dataset family. An aggregate win can be one
    /// family carrying thirty; this is how you tell the difference.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_subset: Vec<SubsetResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worst_cases: Option<Vec<CaseResult>>,
}

/// Leak a string to get the `&'static str` the shipped `ToolGroup` requires.
///
/// The table is `&'static` because in the real system it is a compile-time
/// constant. Building one at runtime therefore means leaking, which is fine
/// here: it happens once per group at startup in a short-lived process, and the
/// alternative is duplicating `ToolGroup` and with it `score`, which would mean
/// measuring my copy instead of the shipped one.
fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// Build a `ToolGroup` table from the synthesised buckets.
fn build_table(file: &ToolRetFile) -> Vec<ToolGroup> {
    file.groups
        .iter()
        .map(|g| ToolGroup {
            id: leak(&g.id),
            brief: leak(&g.brief),
            tags: Box::leak(
                g.tags
                    .iter()
                    .map(|t| leak(t))
                    .collect::<Vec<&'static str>>()
                    .into_boxed_slice(),
            ),
            // Nothing is always-on in the synthetic table. The shipped always-on
            // groups (core, skills, files) are a policy choice about what an
            // agent always needs, not a retrieval decision, and including them
            // would inflate recall without measuring anything.
            always_on: false,
            members: &[],
        })
        .collect()
}

/// Which groups routing attaches for a query, at a given threshold.
///
/// This is `route_once`'s tag arm, using the shipped scorer. The skill edge is
/// absent because the synthetic corpus has no skills pointing at groups; that
/// arm is measured on our own corpus in `toolret.rs`.
fn attach(groups: &[ToolGroup], query: &str, threshold: f64) -> Vec<String> {
    let tokens = table::tokens(query);
    groups
        .iter()
        .filter(|g| table::score(g, &tokens) >= threshold)
        .map(|g| g.id.to_string())
        .collect()
}

/// Route by embedding similarity to the group's own card instead of by tags.
///
/// The comparison this makes possible: tag matching costs nothing and, at the
/// shipped threshold, leaves a large share of queries with no group at all,
/// because a query only routes if it literally contains a tag word. Embedding
/// the query costs one call and can rank a group that shares no vocabulary with
/// the query. Whether that is worth the call is exactly the question, and the
/// two numbers sit side by side in the output.
///
/// Uses the shipped `cosine` from `skill_index`, not a local reimplementation.
fn dense_attach(
    group_vecs: &[(String, Vec<f32>)],
    qvec: &[f32],
    top_k: usize,
    floor: f64,
) -> Vec<String> {
    let mut scored: Vec<(f64, &String)> = group_vecs
        .iter()
        .map(|(id, v)| (crate::lifted::skill_index::cosine(qvec, v), id))
        .filter(|(s, _)| *s >= floor)
        .collect();
    // Ties by id, for determinism.
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(b.1))
    });
    scored
        .into_iter()
        .take(top_k)
        .map(|(_, id)| id.clone())
        .collect()
}

pub fn run(
    file: &ToolRetFile,
    keep_worst: usize,
    embedder: Option<&mut crate::embed::Embedder>,
) -> Result<RouteScale> {
    let groups = build_table(file);
    anyhow::ensure!(
        !groups.is_empty(),
        "the corpus carries no groups; re-fetch with grouping enabled"
    );

    // tool id -> bucket id
    let mut bucket: BTreeMap<&str, &str> = BTreeMap::new();
    for g in &file.groups {
        for m in &g.members {
            bucket.insert(m.as_str(), g.id.as_str());
        }
    }

    // Derive each query's wanted groups by mapping its gold tools through the
    // bucketing. A query whose gold tools all fall outside every bucket is
    // unlabelled and cannot be scored either way.
    struct Case {
        query: String,
        subset: String,
        want: BTreeSet<String>,
    }
    let mut cases: Vec<Case> = Vec::new();
    let mut unlabelled = 0usize;
    for q in &file.queries {
        let want: BTreeSet<String> = q
            .relevant
            .keys()
            .filter_map(|tool| bucket.get(tool.as_str()).map(|b| b.to_string()))
            .collect();
        if want.is_empty() {
            unlabelled += 1;
            continue;
        }
        cases.push(Case {
            query: q.query.clone(),
            subset: q.subset.clone(),
            want,
        });
    }
    anyhow::ensure!(
        !cases.is_empty(),
        "no query had a gold tool inside any bucket; the bucketing is broken"
    );

    let all_ids: BTreeSet<String> = groups.iter().map(|g| g.id.to_string()).collect();
    let shipped = 0.15_f64;

    // Sweep the thresholds that matter: the shipped one, plus the values either
    // side of the m/(m+1) steps. Because score is m/(m+1), the only thresholds
    // that change a decision sit between 0 and 0.5 (one tag match), 0.5 and
    // 0.667 (two), and so on. Sweeping uniformly past 0.7 would produce
    // identical points and a misleadingly smooth curve.
    let thresholds: Vec<f64> = vec![
        0.05, 0.10, 0.15, 0.20, 0.30, 0.40, 0.45, 0.50, 0.55, 0.60, 0.67, 0.70, 0.75, 0.80,
    ];

    let mut sweep = Vec::new();
    let mut worst: Vec<CaseResult> = Vec::new();

    for &t in &thresholds {
        let mut scores: Vec<SetScore> = Vec::with_capacity(cases.len());
        let mut attached_total = 0usize;
        let mut unrouted = 0usize;

        for c in &cases {
            let attached = attach(&groups, &c.query, t);
            attached_total += attached.len();
            if attached.is_empty() {
                unrouted += 1;
            }

            let want: Vec<String> = c.want.iter().cloned().collect();
            // Everything not wanted is unwanted: with a closed group table the
            // complement is well defined, so specificity is measured against
            // every group that should have stayed out rather than a hand-picked
            // few. That is stricter than the hand-written gold set can be.
            let unwanted: BTreeSet<&String> = all_ids.difference(&c.want).collect();

            let s = SetScore {
                wanted_found: want.iter().filter(|w| attached.contains(w)).count(),
                wanted_total: want.len(),
                unwanted_attached: attached.iter().filter(|a| unwanted.contains(a)).count(),
                unwanted_total: unwanted.len(),
            };

            if (t - shipped).abs() < 1e-9 && keep_worst > 0 {
                let missed: Vec<String> = c
                    .want
                    .iter()
                    .filter(|w| !attached.contains(w))
                    .cloned()
                    .collect();
                let spurious: Vec<String> = attached
                    .iter()
                    .filter(|a| !c.want.contains(*a))
                    .cloned()
                    .collect();
                worst.push(CaseResult {
                    query: c.query.clone(),
                    want: want.clone(),
                    attached: attached.clone(),
                    missed,
                    spurious,
                    f1: s.f1(),
                });
            }
            scores.push(s);
        }

        let recalls: Vec<f64> = scores.iter().map(|s| s.recall()).collect();
        let specs: Vec<f64> = scores.iter().map(|s| s.specificity()).collect();
        let f1s: Vec<f64> = scores.iter().map(|s| s.f1()).collect();

        sweep.push(ThresholdPoint {
            threshold: t,
            recall: metrics::mean(&recalls),
            specificity: metrics::mean(&specs),
            f1: metrics::mean(&f1s),
            mean_attached: attached_total as f64 / cases.len() as f64,
            unrouted,
        });
    }

    let at_shipped = sweep
        .iter()
        .find(|p| (p.threshold - shipped).abs() < 1e-9)
        .cloned()
        .context("the shipped threshold was not in the sweep")?;

    let best = sweep
        .iter()
        .max_by(|a, b| a.f1.partial_cmp(&b.f1).unwrap_or(std::cmp::Ordering::Equal))
        .cloned()
        .context("empty sweep")?;

    worst.sort_by(|a, b| {
        a.f1.partial_cmp(&b.f1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.query.cmp(&b.query))
    });
    worst.truncate(keep_worst);

    // Score a set of routing decisions, one per case, the same way the sweep
    // does. Shared so a strategy cannot accidentally be scored on a different
    // basis from the tag baseline it is being compared against.
    let tally = |name: &str, cost: &str, decisions: &[Vec<String>]| -> StrategyResult {
        let mut scores = Vec::with_capacity(cases.len());
        let mut attached_total = 0usize;
        let mut unrouted = 0usize;
        for (c, attached) in cases.iter().zip(decisions) {
            attached_total += attached.len();
            if attached.is_empty() {
                unrouted += 1;
            }
            let unwanted: BTreeSet<&String> = all_ids.difference(&c.want).collect();
            scores.push(SetScore {
                wanted_found: c.want.iter().filter(|w| attached.contains(w)).count(),
                wanted_total: c.want.len(),
                unwanted_attached: attached.iter().filter(|a| unwanted.contains(a)).count(),
                unwanted_total: unwanted.len(),
            });
        }
        let recalls: Vec<f64> = scores.iter().map(|s| s.recall()).collect();
        let specs: Vec<f64> = scores.iter().map(|s| s.specificity()).collect();
        let f1s: Vec<f64> = scores.iter().map(|s| s.f1()).collect();
        StrategyResult {
            strategy: name.to_string(),
            cost: cost.to_string(),
            recall: metrics::mean(&recalls),
            specificity: metrics::mean(&specs),
            f1: metrics::mean(&f1s),
            mean_attached: attached_total as f64 / cases.len() as f64,
            unrouted,
        }
    };

    // Per-case F1 for one set of decisions, on exactly the basis `tally` uses.
    // Needed separately because a per-family breakdown has to regroup the cases
    // after scoring, and an aggregate mean cannot be taken apart again.
    let each_f1 = |decisions: &[Vec<String>]| -> Vec<f64> {
        cases
            .iter()
            .zip(decisions)
            .map(|(c, attached)| {
                let unwanted: BTreeSet<&String> = all_ids.difference(&c.want).collect();
                SetScore {
                    wanted_found: c.want.iter().filter(|w| attached.contains(w)).count(),
                    wanted_total: c.want.len(),
                    unwanted_attached: attached.iter().filter(|a| unwanted.contains(a)).count(),
                    unwanted_total: unwanted.len(),
                }
                .f1()
            })
            .collect()
    };

    let mut strategies: Vec<StrategyResult> = Vec::new();

    // The shipped strategy, restated as a strategy so it sits on the same axis
    // as the alternatives rather than being read off a different table.
    let tag_decisions: Vec<Vec<String>> = cases
        .iter()
        .map(|c| attach(&groups, &c.query, shipped))
        .collect();
    strategies.push(tally("tags (shipped)", "free", &tag_decisions));

    // Decisions kept per strategy so the per-family breakdown can rescore the
    // winner without running it again.
    let mut alternatives: Vec<(String, Vec<Vec<String>>)> = Vec::new();

    if let Some(e) = embedder {
        // Embed each group's card once, and each query once. The group cards are
        // 13 vectors; the queries are the real cost, and it is the same cost the
        // live system would pay, one embedding per opening message.
        let group_texts: Vec<String> = groups
            .iter()
            .map(|g| format!("{}\n{}\n{}", g.id, g.brief, g.tags.join(" ")))
            .collect();
        let gvecs = e.embed_all(&group_texts)?;
        let group_vecs: Vec<(String, Vec<f32>)> = groups
            .iter()
            .map(|g| g.id.to_string())
            .zip(gvecs)
            .collect();

        let query_texts: Vec<String> = cases.iter().map(|c| c.query.clone()).collect();
        let qvecs = e.embed_all(&query_texts)?;

        // top-2 with no floor, and top-2 with a floor. The floor matters because
        // cosine over short cards is rarely near zero, so "nearest group" always
        // returns something -- which fixes the unrouted problem but can attach a
        // group on no real evidence.
        for (k, floor, label) in [
            (1usize, 0.0f64, "dense top-1"),
            (2, 0.0, "dense top-2"),
            (2, 0.25, "dense top-2, floor 0.25"),
        ] {
            let decisions: Vec<Vec<String>> = qvecs
                .iter()
                .map(|qv| dense_attach(&group_vecs, qv, k, floor))
                .collect();
            strategies.push(tally(label, "1 embedding call per query", &decisions));
            alternatives.push((label.to_string(), decisions));
        }

        // Tags, then dense only when tags found nothing. This is the cheap
        // hybrid: it pays for an embedding on the queries the shipped mechanism
        // abandons, and nothing on the ones it already handles.
        let mut paid = 0usize;
        let decisions: Vec<Vec<String>> = tag_decisions
            .iter()
            .zip(&qvecs)
            .map(|(tags, qv)| {
                if tags.is_empty() {
                    paid += 1;
                    dense_attach(&group_vecs, qv, 2, 0.0)
                } else {
                    tags.clone()
                }
            })
            .collect();
        let share = 100.0 * paid as f64 / cases.len() as f64;
        strategies.push(tally(
            "tags, dense fallback",
            &format!("1 call on {share:.0}% of queries"),
            &decisions,
        ));
        alternatives.push(("tags, dense fallback".to_string(), decisions));
    }

    // Break the comparison down by dataset family. The alternative shown per
    // family is the one that wins overall, not the one that wins in that family:
    // picking a per-family winner would be choosing the arm after seeing the
    // answer, and would flatter every family at once.
    let best_overall = strategies
        .iter()
        .max_by(|a, b| a.f1.total_cmp(&b.f1))
        .map(|s| s.strategy.clone());
    let tag_f1s = each_f1(&tag_decisions);
    let best_f1s = best_overall
        .as_deref()
        .and_then(|name| alternatives.iter().find(|(n, _)| n == name))
        .map(|(_, d)| each_f1(d));

    let mut families: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, c) in cases.iter().enumerate() {
        families.entry(c.subset.as_str()).or_default().push(i);
    }
    let mut by_subset: Vec<SubsetResult> = families
        .into_iter()
        .map(|(subset, idx)| {
            let pick = |v: &Vec<f64>| metrics::mean(&idx.iter().map(|&i| v[i]).collect::<Vec<_>>());
            SubsetResult {
                subset: subset.to_string(),
                queries: idx.len(),
                tags_f1: pick(&tag_f1s),
                tags_unrouted: idx
                    .iter()
                    .filter(|&&i| tag_decisions[i].is_empty())
                    .count(),
                best_f1: best_f1s.as_ref().map(pick),
                best_strategy: best_f1s.as_ref().and(best_overall.clone()),
            }
        })
        .collect();
    // Largest families first: they carry the aggregate, so they are what a
    // reader should weigh most.
    by_subset.sort_by(|a, b| b.queries.cmp(&a.queries).then(a.subset.cmp(&b.subset)));

    Ok(RouteScale {
        queries: cases.len(),
        groups: groups.len(),
        tools: file.tools.len(),
        unlabelled,
        shipped_threshold: shipped,
        sweep,
        at_shipped,
        best_f1: best.f1,
        best_threshold: best.threshold,
        strategies,
        by_subset,
        worst_cases: if worst.is_empty() { None } else { Some(worst) },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{GroupDoc, QueryDoc, ToolDoc};

    fn tool(id: &str, group: &str) -> ToolDoc {
        ToolDoc {
            id: id.to_string(),
            group: group.to_string(),
            subset: "t".into(),
            name: id.to_string(),
            brief: String::new(),
            when_to_use: String::new(),
        }
    }

    fn file() -> ToolRetFile {
        ToolRetFile {
            source: "test".into(),
            note: String::new(),
            tools: vec![tool("t1", "code"), tool("t2", "travel")],
            queries: vec![
                QueryDoc {
                    id: "q1".into(),
                    query: "please review my git repository".into(),
                    subset: "t".into(),
                    relevant: [("t1".to_string(), 1.0)].into_iter().collect(),
                },
                QueryDoc {
                    id: "q2".into(),
                    query: "book me a flight".into(),
                    subset: "t".into(),
                    relevant: [("t2".to_string(), 1.0)].into_iter().collect(),
                },
            ],
            groups: vec![
                GroupDoc {
                    id: "code".into(),
                    brief: "code things".into(),
                    tags: vec!["git".into(), "repository".into()],
                    members: vec!["t1".into()],
                },
                GroupDoc {
                    id: "travel".into(),
                    brief: "travel things".into(),
                    tags: vec!["flight".into(), "hotel".into()],
                    members: vec!["t2".into()],
                },
            ],
        }
    }

    #[test]
    fn labels_come_from_the_gold_tools() {
        let r = run(&file(), 5, None).unwrap();
        assert_eq!(r.queries, 2);
        assert_eq!(r.unlabelled, 0);
        // Both queries name a tag of exactly the right group, so routing at the
        // shipped threshold should be perfect.
        assert!(
            r.at_shipped.recall > 0.99,
            "recall {} should be 1.0",
            r.at_shipped.recall
        );
        assert!(r.at_shipped.specificity > 0.99);
    }

    #[test]
    fn a_query_with_no_bucketed_gold_tool_is_unlabelled_not_wrong() {
        let mut f = file();
        f.queries[0].relevant = [("nonexistent".to_string(), 1.0)].into_iter().collect();
        let r = run(&f, 0, None).unwrap();
        assert_eq!(r.unlabelled, 1);
        assert_eq!(r.queries, 1);
    }

    #[test]
    fn a_high_threshold_routes_nothing_and_says_so() {
        let r = run(&file(), 0, None).unwrap();
        let top = r.sweep.last().unwrap();
        // score is m/(m+1), so one tag match = 0.5 and can never reach 0.8.
        assert_eq!(top.unrouted, 2, "nothing should clear 0.8 on one tag match");
        assert_eq!(top.mean_attached, 0.0);
    }

    #[test]
    fn the_sweep_finds_the_shipped_threshold() {
        let r = run(&file(), 0, None).unwrap();
        assert!((r.at_shipped.threshold - 0.15).abs() < 1e-9);
        assert!(r.best_f1 >= r.at_shipped.f1, "best must be at least shipped");
    }
}
