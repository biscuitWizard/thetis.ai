//! Ablation: what does each retrieval mechanism actually buy?
//!
//! Per-commit charting cannot answer that question in this repo. Across all 783
//! commits there is exactly *one* distinct version of `skill_index.rs`: the
//! ranker arrived complete in a squashed root commit, with dense, BM25,
//! absorption and promotion already in place. A time series over that history is
//! a flat line by construction, and reading it as "these mechanisms do nothing"
//! would be exactly wrong.
//!
//! So measure them directly. Hold the corpus, the queries and the commit fixed,
//! switch one mechanism off, and report the delta. That is what answers "this
//! costs money to run, should we keep it".
//!
//! Every arm calls the **real lifted scorers** from `skill_index`, never a
//! reimplementation, because a lookalike can agree with the shipped code today
//! and quietly disagree after someone edits it.
//!
//! The arms:
//!
//! | arm | what it isolates |
//! |---|---|
//! | `bm25` | the lexical fallback alone |
//! | `dense` | embeddings alone — the intended path |
//! | `fusion-w{N}` | convex mix of the two, to test the "not fused" decision |
//! | `no-absorption` | parent absorption removed |
//! | `no-promotion` | child-to-parent promotion removed |
//! | `pool{N}` | candidate pool size |
//! | `limit{N}` | how many cards are returned |
//!
//! Cost is reported alongside quality, because a mechanism that adds 0.002 nDCG
//! for an embedding call per turn is a bad trade and the table should show it.

use std::collections::HashMap;

use anyhow::Result;

use crate::embed::{Embedder, Mode};
use crate::lifted::skill_index::{
    self, absorb_into_parents, bm25_scores, dense_scores, Indexed, Ranked,
};
use crate::lifted::skills::{Skill, SkillTree};
use crate::metrics::{
    hit_at_1, mean, ndcg_at_k, paired_bootstrap, recall_at_k, reciprocal_rank, Gold,
};

/// One query with graded relevance, in the ranker's own vocabulary.
pub struct Case {
    pub id: String,
    pub query: String,
    /// Document id to gain. Graded, so nDCG uses the real gains.
    pub gold: HashMap<String, f64>,
}

/// What a single arm scored.
#[derive(Clone, serde::Serialize)]
pub struct ArmResult {
    pub arm: String,
    pub ndcg_at_k: f64,
    pub recall_at_k: f64,
    pub hit_at_1: f64,
    pub mrr: f64,
    pub cases: usize,
    /// Embedding vectors this arm needed. Zero for the purely lexical arms:
    /// this is the column that turns a quality delta into a cost decision.
    pub vectors_used: usize,
    /// Delta in nDCG against the named baseline arm, once known.
    pub vs_baseline: Option<f64>,
    /// Two-sided p-value for that delta, from a paired bootstrap over queries.
    pub p_value: Option<f64>,
    /// 95% confidence interval on the delta. Straddling zero means the arm has
    /// not been shown to differ from the baseline at this sample size.
    pub ci95: Option<(f64, f64)>,
    /// Per-query nDCG, kept so deltas can be tested pairwise rather than by
    /// comparing two means that came from the same queries.
    #[serde(skip)]
    pub per_query: Vec<f64>,
}

/// How an arm scores a query. Each variant maps to real lifted code.
#[derive(Clone, Copy, PartialEq)]
pub enum Scorer {
    Bm25,
    Dense,
    /// `w` weights dense; `1 - w` weights BM25, after each side is normalised.
    Fusion(f64),
}

#[derive(Clone, Copy)]
pub struct Arm {
    pub scorer: Scorer,
    pub absorption: bool,
    pub promotion: bool,
    pub pool: usize,
    pub limit: usize,
}

impl Arm {
    pub fn baseline(limit: usize) -> Self {
        Arm {
            scorer: Scorer::Bm25,
            absorption: true,
            promotion: true,
            pool: skill_index::CANDIDATE_POOL,
            limit,
        }
    }
}

/// Min-max normalise to [0,1] so two scorers on different scales can be mixed.
///
/// Cosine similarity sits in about [-1,1] while BM25 is unbounded above; adding
/// them raw would let BM25 dominate purely through magnitude, which would make a
/// fusion result meaningless rather than merely bad.
fn normalise(scored: &mut [(String, f64)]) {
    if scored.is_empty() {
        return;
    }
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    for (_, s) in scored.iter() {
        lo = lo.min(*s);
        hi = hi.max(*s);
    }
    let span = hi - lo;
    if span <= f64::EPSILON {
        // Every candidate tied: flat 1.0 keeps the set, drops the false ordering.
        for (_, s) in scored.iter_mut() {
            *s = 1.0;
        }
        return;
    }
    for (_, s) in scored.iter_mut() {
        *s = (*s - lo) / span;
    }
}

/// Raw candidate scores per query, computed once and shared by every arm that
/// uses the same scorer.
///
/// This is what makes the sweep tractable. Twelve of the thirteen default arms
/// score densely, and cosine similarity over 9,500 documents x 1,536 dimensions
/// x 1,600 queries is ~24 GFLOP *per arm*; recomputing it thirteen times turned a
/// two-minute job into one that got killed before finishing. Scoring depends only
/// on (query, scorer), while the arms differ in what they do *after* scoring, so
/// the expensive half is cached and only the structural half is repeated.
///
/// It also removes a source of doubt: every arm on a given scorer now provably
/// sees byte-identical candidate scores, so a delta between them can only come
/// from the mechanism under test.
pub struct ScoreCache {
    /// (scorer key, query id) -> descending candidate list, truncated to `keep`.
    inner: HashMap<(u8, String), Vec<(String, f64)>>,
    /// How deep a list to retain. Holding all 9,500 scored documents for every
    /// (scorer, query) pair would be ~60M entries and gigabytes of resident
    /// memory; no arm looks deeper than its pool, so anything past the largest
    /// pool in the plan is dead weight. Set from the plan, never guessed.
    keep: usize,
}

fn scorer_key(s: Scorer) -> u8 {
    match s {
        Scorer::Bm25 => 0,
        Scorer::Dense => 1,
        // Each fusion weight is its own entry; the weight is quantised into the
        // key so 0.3 and 0.7 cannot collide.
        Scorer::Fusion(w) => 2 + (w * 10.0).round() as u8,
    }
}

impl ScoreCache {
    /// `keep` must be at least the largest pool any arm will ask for, or an arm
    /// would silently see a shorter candidate list than it requested.
    pub fn new(keep: usize) -> Self {
        ScoreCache {
            inner: HashMap::new(),
            keep: keep.max(1),
        }
    }

    /// Fill the cache for every (scorer, query) pair the plan needs, in parallel.
    ///
    /// Necessary because of a property of the shipped ranker: `bm25_scores`
    /// rebuilds its entire document index — tokenising every document and
    /// recomputing document frequencies — on *each* call, since it takes a corpus
    /// slice and holds no state between queries. At 61 skills that is invisible.
    /// At 9,529 documents it is about a quarter-second per query, so one BM25 arm
    /// over 1,634 queries takes seven minutes single-threaded and the sweep never
    /// finishes.
    ///
    /// This is deliberately fixed here by *scheduling*, not by rewriting the
    /// scorer: the whole point is to measure the shipped code, so the harness
    /// runs the same function the same number of times and merely spreads the
    /// calls across cores. Queries are independent, so this changes nothing about
    /// the results — verified by `prefill_matches_serial_scores`.
    pub fn prefill(
        &mut self,
        plan: &Plan,
        skills: &[Skill],
        vectors: &HashMap<String, Vec<f32>>,
        cases: &[Case],
        qvecs: &HashMap<String, Vec<f32>>,
    ) {
        // Distinct scorers only: twelve dense arms share one set of scores.
        let mut scorers: Vec<Scorer> = Vec::new();
        for (_, arm) in &plan.arms {
            if !scorers.iter().any(|s| scorer_key(*s) == scorer_key(arm.scorer)) {
                scorers.push(arm.scorer);
            }
        }

        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(cases.len().max(1));
        let keep = self.keep;

        for scorer in scorers {
            let needs_vectors = !matches!(scorer, Scorer::Bm25);
            let corpus: Vec<Indexed<'_>> = skills
                .iter()
                .map(|s| Indexed {
                    skill: s,
                    vector: if needs_vectors {
                        vectors.get(&s.id).map(|v| v.as_slice())
                    } else {
                        None
                    },
                })
                .collect();

            eprintln!(
                "  scoring {} queries with {} ({} threads)...",
                cases.len(),
                match scorer {
                    Scorer::Bm25 => "bm25".to_string(),
                    Scorer::Dense => "dense".to_string(),
                    Scorer::Fusion(w) => format!("fusion w={w:.1}"),
                },
                threads
            );

            let chunk = cases.len().div_ceil(threads);
            let results: Vec<(String, Vec<(String, f64)>)> = std::thread::scope(|scope| {
                let handles: Vec<_> = cases
                    .chunks(chunk.max(1))
                    .map(|slice| {
                        let corpus = &corpus;
                        scope.spawn(move || {
                            slice
                                .iter()
                                .map(|c| {
                                    let qvec = if needs_vectors {
                                        qvecs.get(&c.id).map(|v| v.as_slice())
                                    } else {
                                        None
                                    };
                                    let mut v =
                                        raw_scores(scorer, corpus, &c.query, qvec);
                                    v.truncate(keep);
                                    (c.id.clone(), v)
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().unwrap_or_default())
                    .collect()
            });

            let sk = scorer_key(scorer);
            for (qid, scored) in results {
                self.inner.insert((sk, qid), scored);
            }
        }
    }

    fn get_or_compute(
        &mut self,
        scorer: Scorer,
        corpus: &[Indexed<'_>],
        case: &Case,
        qvec: Option<&[f32]>,
    ) -> &Vec<(String, f64)> {
        let key = (scorer_key(scorer), case.id.clone());
        let keep = self.keep;
        self.inner.entry(key).or_insert_with(|| {
            let mut v = raw_scores(scorer, corpus, &case.query, qvec);
            v.truncate(keep);
            v
        })
    }
}

/// The scoring half of the pipeline: lifted scorers, optionally mixed.
fn raw_scores(
    scorer: Scorer,
    corpus: &[Indexed<'_>],
    query: &str,
    qvec: Option<&[f32]>,
) -> Vec<(String, f64)> {
    match scorer {
        Scorer::Bm25 => bm25_scores(corpus, query),
        Scorer::Dense => match qvec {
            Some(v) => dense_scores(corpus, v),
            None => bm25_scores(corpus, query),
        },
        Scorer::Fusion(w) => match qvec {
            Some(v) => {
                let mut d = dense_scores(corpus, v);
                let mut l = bm25_scores(corpus, query);
                normalise(&mut d);
                normalise(&mut l);
                let mut acc: HashMap<String, f64> = HashMap::new();
                for (id, s) in d {
                    *acc.entry(id).or_insert(0.0) += w * s;
                }
                for (id, s) in l {
                    *acc.entry(id).or_insert(0.0) += (1.0 - w) * s;
                }
                let mut v: Vec<(String, f64)> = acc.into_iter().collect();
                // Sort by score, then id: ties must not depend on HashMap order,
                // or the same arm gives different numbers between runs.
                v.sort_by(|a, b| {
                    b.1.partial_cmp(&a.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.0.cmp(&b.0))
                });
                v
            }
            None => bm25_scores(corpus, query),
        },
    }
}

/// The structural half: pool, absorption, truncation, promotion.
fn stage(arm: &Arm, tree: &SkillTree, scored: &[(String, f64)]) -> Vec<Ranked> {
    let mut pooled: Vec<(String, f64)> = scored.iter().take(arm.pool).cloned().collect();

    let staged = if arm.absorption {
        absorb_into_parents(tree, pooled)
    } else {
        pooled.truncate(arm.pool);
        pooled
    };

    let mut out: Vec<Ranked> = staged
        .into_iter()
        .map(|(id, score)| Ranked {
            id,
            score,
            how: skill_index::How::Lexical,
        })
        .collect();

    out.truncate(arm.limit);
    if arm.promotion {
        skill_index::promote_parents(tree, &mut out, arm.limit);
    }
    out
}

/// Score every case under one arm.
pub fn run_arm(
    label: &str,
    arm: &Arm,
    tree: &SkillTree,
    skills: &[Skill],
    vectors: &HashMap<String, Vec<f32>>,
    cases: &[Case],
    qvecs: &HashMap<String, Vec<f32>>,
    cache: &mut ScoreCache,
) -> ArmResult {
    let needs_vectors = !matches!(arm.scorer, Scorer::Bm25);

    let corpus: Vec<Indexed<'_>> = skills
        .iter()
        .map(|s| Indexed {
            skill: s,
            vector: if needs_vectors {
                vectors.get(&s.id).map(|v| v.as_slice())
            } else {
                None
            },
        })
        .collect();

    let mut ndcgs: Vec<f64> = Vec::new();
    let mut recalls = Vec::new();
    let mut hits = Vec::new();
    let mut rrs = Vec::new();

    for c in cases {
        let qvec = if needs_vectors {
            qvecs.get(&c.id).map(|v| v.as_slice())
        } else {
            None
        };
        let scored = cache.get_or_compute(arm.scorer, &corpus, c, qvec);
        let ranked = stage(arm, tree, scored);
        let ids: Vec<String> = ranked.into_iter().map(|r| r.id).collect();
        let gold = Gold(c.gold.iter().map(|(k, v)| (k.clone(), *v)).collect());

        ndcgs.push(ndcg_at_k(&ids, &gold, arm.limit));
        recalls.push(recall_at_k(&ids, &gold, arm.limit));
        hits.push(hit_at_1(&ids, &gold));
        rrs.push(reciprocal_rank(&ids, &gold));
    }

    ArmResult {
        arm: label.to_string(),
        ndcg_at_k: mean(&ndcgs),
        recall_at_k: mean(&recalls),
        hit_at_1: mean(&hits),
        mrr: mean(&rrs),
        cases: cases.len(),
        vectors_used: if needs_vectors {
            vectors.len() + qvecs.len()
        } else {
            0
        },
        vs_baseline: None,
        p_value: None,
        ci95: None,
        per_query: ndcgs,
    }
}

/// The full sweep. `baseline` is whichever arm the deltas are quoted against.
pub struct Plan {
    pub arms: Vec<(String, Arm)>,
    pub baseline: String,
}

impl Plan {
    /// The default sweep: one arm per mechanism, plus the fusion weights that
    /// the "dense and BM25 are not fused" decision rests on.
    pub fn default_sweep(limit: usize, dense_ok: bool) -> Self {
        let base = Arm::baseline(limit);
        let mut arms: Vec<(String, Arm)> = vec![("bm25".into(), base)];

        if dense_ok {
            arms.push((
                "dense".into(),
                Arm {
                    scorer: Scorer::Dense,
                    ..base
                },
            ));
            for w in [0.3, 0.5, 0.7, 0.9] {
                arms.push((
                    format!("fusion-w{w:.1}"),
                    Arm {
                        scorer: Scorer::Fusion(w),
                        ..base
                    },
                ));
            }
        }

        // Structural stages, ablated against whichever scorer is available so the
        // comparison is like-for-like with the headline arm.
        let structural = if dense_ok { Scorer::Dense } else { Scorer::Bm25 };
        let sbase = Arm {
            scorer: structural,
            ..base
        };
        arms.push((
            "no-absorption".into(),
            Arm {
                absorption: false,
                ..sbase
            },
        ));
        arms.push((
            "no-promotion".into(),
            Arm {
                promotion: false,
                ..sbase
            },
        ));
        arms.push((
            "no-absorption-no-promotion".into(),
            Arm {
                absorption: false,
                promotion: false,
                ..sbase
            },
        ));

        for p in [10usize, 25, 100, 200] {
            arms.push((format!("pool{p}"), Arm { pool: p, ..sbase }));
        }

        Plan {
            arms,
            baseline: if dense_ok { "dense".into() } else { "bm25".into() },
        }
    }
}

/// Embed the corpus and the queries, reusing the on-disk cache.
///
/// Returned separately from scoring so a lexical-only sweep costs nothing and
/// needs no key at all.
pub fn embed_corpus(
    embedder: Option<&mut Embedder>,
    skills: &[Skill],
    cases: &[Case],
) -> Result<(HashMap<String, Vec<f32>>, HashMap<String, Vec<f32>>, Mode)> {
    let mut docs = HashMap::new();
    let mut queries = HashMap::new();

    let Some(embedder) = embedder else {
        return Ok((docs, queries, Mode::Lexical));
    };

    let texts: Vec<String> = skills.iter().map(|s| s.index_text()).collect();

    // Before trusting a warm cache, check one entry against the live endpoint.
    // A cache that disagrees with its own endpoint about the same text is
    // poisoned, and every dense score computed from it is fiction; refusing to
    // run is the only safe response, because the resulting numbers look
    // perfectly plausible.
    match embedder.verify(&texts) {
        Ok(Some(sim)) if sim < 0.99 => {
            anyhow::bail!(
                "embedding cache is not consistent with the endpoint\n\
                 \n\
                 A cached vector and a freshly fetched one for the SAME text have \
                 cosine {sim:.4}; it must be ~1.0.\n\
                 The cache holds vectors from a different producer (a mock \
                 endpoint, a different model, or a truncated write), so every \
                 dense score from it would be meaningless.\n\
                 \n\
                 Delete the cache and re-run to re-embed from scratch."
            );
        }
        Ok(_) => {}
        // A verification failure is not itself a reason to stop: the real
        // embed_all below will surface a genuine outage with a better error.
        Err(e) => eprintln!("  note: could not verify the embedding cache: {e}"),
    }

    let vecs = embedder.embed_all(&texts)?;
    for (s, v) in skills.iter().zip(vecs) {
        docs.insert(s.id.clone(), v);
    }

    let qtexts: Vec<String> = cases.iter().map(|c| c.query.clone()).collect();
    let qvecs = embedder.embed_all(&qtexts)?;
    for (c, v) in cases.iter().zip(qvecs) {
        queries.insert(c.id.clone(), v);
    }

    let mode = if docs.is_empty() {
        Mode::Lexical
    } else {
        Mode::Dense
    };
    Ok((docs, queries, mode))
}

/// Run a plan and fill in the baseline deltas.
pub fn run_plan(
    plan: &Plan,
    tree: &SkillTree,
    skills: &[Skill],
    vectors: &HashMap<String, Vec<f32>>,
    cases: &[Case],
    qvecs: &HashMap<String, Vec<f32>>,
) -> Vec<ArmResult> {
    // Sized to the deepest pool in the plan so no arm is starved of candidates.
    let keep = plan.arms.iter().map(|(_, a)| a.pool).max().unwrap_or(50);
    let mut cache = ScoreCache::new(keep);
    cache.prefill(plan, skills, vectors, cases, qvecs);

    let mut out: Vec<ArmResult> = plan
        .arms
        .iter()
        .map(|(label, arm)| {
            eprintln!("  {label} ...");
            run_arm(label, arm, tree, skills, vectors, cases, qvecs, &mut cache)
        })
        .collect();

    if let Some(b) = out.iter().find(|r| r.arm == plan.baseline).cloned() {
        for r in out.iter_mut() {
            r.vs_baseline = Some(r.ndcg_at_k - b.ndcg_at_k);
            // 10k resamples: enough that the p-value is stable to ~0.005, and
            // still trivial next to the ranking work already done.
            let (_, p, ci) = paired_bootstrap(&r.per_query, &b.per_query, 10_000);
            r.p_value = Some(p);
            r.ci95 = Some(ci);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::lifted::skills::ChildSpec;
    use std::path::PathBuf;

    fn skill(id: &str, brief: &str) -> Skill {
        Skill {
            id: id.into(),
            path: PathBuf::from(id),
            parent: String::new(),
            depth: 0,
            name: id.into(),
            brief: brief.into(),
            when_to_use: String::new(),
            universal: false,
            tags: Vec::new(),
            children: ChildSpec::Auto,
            related: Vec::new(),
            status: String::new(),
            superseded_by: String::new(),
            version: 1,
            body: String::new(),
            resources: Vec::new(),
            content_hash: String::new(),
            owns_dir: true,
        }
    }

    /// The parallel prefill must agree with computing each query on demand.
    ///
    /// This is the guard on the optimisation that made the sweep finishable: if
    /// threading ever perturbed a score, every delta in the table would be
    /// suspect. Compares full candidate lists, not just the means.
    #[test]
    fn prefill_matches_serial_scores() {
        let words = [
            "deploy a service to production",
            "read a file from disk",
            "query the sql warehouse",
            "resize an image thumbnail",
            "send an email to the team",
        ];
        let skills: Vec<Skill> = (0..40)
            .map(|i| skill(&format!("s{i}"), words[i % words.len()]))
            .collect();
        let mut map = HashMap::new();
        let mut roots = Vec::new();
        for s in &skills {
            roots.push(s.id.clone());
            map.insert(s.id.clone(), s.clone());
        }
        let tree = SkillTree {
            skills: map,
            roots,
        };

        let cases: Vec<Case> = words
            .iter()
            .enumerate()
            .map(|(i, w)| Case {
                id: format!("q{i}"),
                query: (*w).to_string(),
                gold: HashMap::new(),
            })
            .collect();

        let vectors = HashMap::new();
        let qvecs = HashMap::new();
        let plan = Plan {
            arms: vec![("bm25".into(), Arm::baseline(5))],
            baseline: "bm25".into(),
        };

        let mut parallel = ScoreCache::new(50);
        parallel.prefill(&plan, &skills, &vectors, &cases, &qvecs);

        let corpus: Vec<Indexed<'_>> = skills
            .iter()
            .map(|s| Indexed {
                skill: s,
                vector: None,
            })
            .collect();

        for c in &cases {
            let mut serial = raw_scores(Scorer::Bm25, &corpus, &c.query, None);
            serial.truncate(50);
            let got = parallel
                .inner
                .get(&(scorer_key(Scorer::Bm25), c.id.clone()))
                .expect("prefill should have cached every query");
            assert_eq!(got.len(), serial.len(), "length differs for {}", c.id);
            for (a, b) in got.iter().zip(serial.iter()) {
                assert_eq!(a.0, b.0, "id order differs for {}", c.id);
                assert!((a.1 - b.1).abs() < 1e-12, "score differs for {}", c.id);
            }
        }
        let _ = &tree;
    }

    #[test]
    fn normalise_maps_onto_unit_range() {
        let mut v = vec![("a".into(), 2.0), ("b".into(), 4.0), ("c".into(), 6.0)];
        normalise(&mut v);
        assert_eq!(v[0].1, 0.0);
        assert_eq!(v[2].1, 1.0);
        assert!((v[1].1 - 0.5).abs() < 1e-9);
    }

    #[test]
    fn normalise_handles_a_total_tie() {
        // A flat set must not become NaN through division by a zero span.
        let mut v = vec![("a".into(), 3.0), ("b".into(), 3.0)];
        normalise(&mut v);
        assert!(v.iter().all(|(_, s)| *s == 1.0));
    }

    #[test]
    fn normalise_tolerates_empty() {
        let mut v: Vec<(String, f64)> = Vec::new();
        normalise(&mut v);
        assert!(v.is_empty());
    }

    #[test]
    fn fusion_at_w1_matches_dense_ordering() {
        // Sanity on the mixing algebra: all weight on one side must reproduce
        // that side's ranking, or the fusion arms mean nothing.
        let mut d = vec![("a".into(), 0.9), ("b".into(), 0.1)];
        let mut l = vec![("a".into(), 0.0), ("b".into(), 9.9)];
        normalise(&mut d);
        normalise(&mut l);
        let w = 1.0;
        let mut acc: HashMap<String, f64> = HashMap::new();
        for (id, s) in d {
            *acc.entry(id).or_insert(0.0) += w * s;
        }
        for (id, s) in l {
            *acc.entry(id).or_insert(0.0) += (1.0 - w) * s;
        }
        let mut v: Vec<(String, f64)> = acc.into_iter().collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then_with(|| a.0.cmp(&b.0)));
        assert_eq!(v[0].0, "a");
    }
}
