//! SkillRet: does skill retrieval surface the right card for an opening message?
//!
//! Runs the real `skill_index::rank` over the real corpus discovered by the real
//! `skills::discover`. Nothing about ranking is reimplemented here; this module
//! only feeds it queries and scores what comes back.

use crate::embed::{Embedder, Mode};
use crate::lifted::{skill_index, skills};
use crate::metrics::{self, Gold};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Deserialize)]
struct GoldFile {
    cases: Vec<GoldCase>,
}

#[derive(Deserialize)]
struct GoldCase {
    query: String,
    relevant: BTreeMap<String, f64>,
}

#[derive(Serialize)]
pub struct CaseResult {
    pub query: String,
    pub ndcg: f64,
    pub recall: f64,
    pub hit1: f64,
    pub rr: f64,
    pub returned: Vec<String>,
    pub missed: Vec<String>,
}

#[derive(Serialize)]
pub struct SkillRet {
    pub mode: String,
    pub limit: usize,
    pub corpus_size: usize,
    pub cases: usize,
    pub ndcg_at_k: f64,
    pub recall_at_k: f64,
    pub hit_at_1: f64,
    pub mrr: f64,
    /// Skills the gold set names that no longer exist in the corpus. A rename
    /// makes every case mentioning the old id unwinnable, which would look like
    /// a ranking regression; surfacing it as its own number keeps a corpus
    /// reorganisation from being misread as a quality drop.
    pub unknown_gold_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_case: Option<Vec<CaseResult>>,
}

/// `limit` mirrors the shipping `skills.retrieve_limit`.
///
/// It must stay strictly below the corpus size or the ranker short-circuits:
/// `rank` returns the whole corpus untouched when `corpus.len() <= limit`, which
/// would score a perfect 1.0 while measuring nothing whatsoever.
pub fn run(
    skills_dir: &Path,
    gold_path: &Path,
    limit: usize,
    embedder: Option<&mut Embedder>,
    per_case: bool,
) -> Result<SkillRet> {
    let tree = skills::discover(skills_dir)
        .with_context(|| format!("discovering skills in {}", skills_dir.display()))?;

    let gold: GoldFile = serde_json::from_str(
        &std::fs::read_to_string(gold_path)
            .with_context(|| format!("reading gold set {}", gold_path.display()))?,
    )
    .context("gold set is not valid JSON")?;

    // Stable order: HashMap iteration is not, and the ranker breaks ties by id,
    // so an unsorted corpus would make tied results flap between runs and show
    // up in the chart as noise.
    let mut ids: Vec<String> = tree.skills.keys().cloned().collect();
    ids.sort();

    if limit >= ids.len() {
        anyhow::bail!(
            "limit {limit} is not below the corpus size {}: rank() would \
             short-circuit and return everything, measuring nothing",
            ids.len()
        );
    }

    // Vectors keyed by the same card text the shipping embedder hashes.
    let mut vectors: BTreeMap<String, Vec<f32>> = BTreeMap::new();
    let mut mode = Mode::Lexical;

    if let Some(embedder) = embedder {
        let cards: Vec<String> = ids
            .iter()
            .map(|id| tree.skills[id].index_text())
            .collect();
        match embedder.embed_all(&cards) {
            Ok(vs) => {
                for (id, v) in ids.iter().zip(vs) {
                    vectors.insert(id.clone(), v);
                }
                mode = Mode::Dense;
            }
            Err(e) => {
                // Not fatal. A dense run is preferred but a lexical one is a
                // real measurement of a real code path, and losing the whole
                // datapoint to a transient HTTP failure is worse than labelling
                // this one honestly.
                eprintln!("warning: embedding the corpus failed, falling back to lexical: {e:#}");
            }
        }

        let mut results = Vec::new();
        let mut unknown = collect_unknown(&gold, &tree);

        for case in &gold.cases {
            let qv = if mode == Mode::Dense {
                match embedder.embed_one(&case.query) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        eprintln!("warning: embedding a query failed: {e:#}");
                        None
                    }
                }
            } else {
                None
            };
            results.push(score_case(&tree, &ids, &vectors, case, qv.as_deref(), limit));
        }

        unknown.sort();
        unknown.dedup();
        return Ok(aggregate(mode, limit, ids.len(), results, unknown, per_case));
    }

    let mut unknown = collect_unknown(&gold, &tree);
    let results: Vec<CaseResult> = gold
        .cases
        .iter()
        .map(|case| score_case(&tree, &ids, &vectors, case, None, limit))
        .collect();
    unknown.sort();
    unknown.dedup();
    Ok(aggregate(mode, limit, ids.len(), results, unknown, per_case))
}

fn collect_unknown(gold: &GoldFile, tree: &skills::SkillTree) -> Vec<String> {
    gold.cases
        .iter()
        .flat_map(|c| c.relevant.keys())
        .filter(|id| tree.get(id).is_none())
        .cloned()
        .collect()
}

fn score_case(
    tree: &skills::SkillTree,
    ids: &[String],
    vectors: &BTreeMap<String, Vec<f32>>,
    case: &GoldCase,
    query_vector: Option<&[f32]>,
    limit: usize,
) -> CaseResult {
    let corpus: Vec<skill_index::Indexed<'_>> = ids
        .iter()
        .map(|id| skill_index::Indexed {
            skill: &tree.skills[id],
            vector: vectors.get(id).map(|v| v.as_slice()),
        })
        .collect();

    let ranked = skill_index::rank(tree, &corpus, &case.query, query_vector, limit);
    let returned: Vec<String> = ranked.iter().map(|r| r.id.clone()).collect();

    let gold = Gold(case.relevant.clone());
    let missed: Vec<String> = gold
        .0
        .iter()
        .filter(|(id, gain)| **gain > 0.0 && !returned.contains(id))
        .map(|(id, _)| id.clone())
        .collect();

    CaseResult {
        query: case.query.clone(),
        ndcg: metrics::ndcg_at_k(&returned, &gold, limit),
        recall: metrics::recall_at_k(&returned, &gold, limit),
        hit1: metrics::hit_at_1(&returned, &gold),
        rr: metrics::reciprocal_rank(&returned, &gold),
        returned,
        missed,
    }
}

fn aggregate(
    mode: Mode,
    limit: usize,
    corpus_size: usize,
    results: Vec<CaseResult>,
    unknown_gold_ids: Vec<String>,
    per_case: bool,
) -> SkillRet {
    let ndcg = metrics::mean(&results.iter().map(|r| r.ndcg).collect::<Vec<_>>());
    let recall = metrics::mean(&results.iter().map(|r| r.recall).collect::<Vec<_>>());
    let hit1 = metrics::mean(&results.iter().map(|r| r.hit1).collect::<Vec<_>>());
    let mrr = metrics::mean(&results.iter().map(|r| r.rr).collect::<Vec<_>>());

    SkillRet {
        mode: mode.label().to_string(),
        limit,
        corpus_size,
        cases: results.len(),
        ndcg_at_k: ndcg,
        recall_at_k: recall,
        hit_at_1: hit1,
        mrr,
        unknown_gold_ids,
        per_case: per_case.then_some(results),
    }
}
