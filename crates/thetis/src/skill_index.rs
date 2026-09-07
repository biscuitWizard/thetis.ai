//! Ranking skills against a query.
//!
//! Two paths, one interface:
//!
//! - **Dense** — cosine similarity over cached 1536-d embeddings of each skill's
//!   card text. This is the intended path.
//! - **BM25** — a lexical fallback for when embeddings are unavailable: no API
//!   key, an HTTP failure, or a corpus that has never been indexed.
//!
//! The two are deliberately *not* fused. Benchmarked on a 35-skill corpus with
//! 28 non-echoing queries and deliberate near-miss distractors, reciprocal-rank
//! fusion lost to dense alone at every mixing weight tried (hit@1 0.43 vs 0.50
//! at w_dense=0.9). Skill cards are short, paraphrased, and semantically close
//! to one another, which is exactly the shape where lexical overlap misleads.
//! BM25 stays as insurance, not as a contributor.
//!
//! After ranking, two structural adjustments apply:
//!
//! - **Parent absorption** — when a parent and its own children both rank, only
//!   the parent is returned. Its card already indexes the children, so the agent
//!   can descend on its own; returning both spends the budget twice on one topic.
//! - **Child promotion** — when a child ranks well but its parent does not, the
//!   parent is pulled in beneath it, because a child's body often assumes the
//!   parent's framing.

use crate::skills::{Skill, SkillTree};
use std::collections::{HashMap, HashSet};

/// How many candidates the scoring stage keeps before structural adjustment.
const CANDIDATE_POOL: usize = 50;

/// Okapi saturation and length-normalisation constants. Standard values; the
/// corpus is too small for tuning them to mean anything.
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

/// One ranked skill.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    pub id: String,
    pub score: f64,
    pub how: How,
}

/// Why a skill is in the result, which the panel shows and the tests assert on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// Ranked directly by cosine similarity.
    Dense,
    /// Ranked directly by BM25, because dense was unavailable.
    Lexical,
    /// Pulled in as the parent of a skill that ranked.
    ParentOfMatch,
    /// Returned because the corpus is small enough to skip ranking entirely.
    WholeCorpus,
}

impl How {
    pub fn label(self) -> &'static str {
        match self {
            How::Dense => "dense",
            How::Lexical => "lexical",
            How::ParentOfMatch => "parent-of-match",
            How::WholeCorpus => "whole-corpus",
        }
    }
}

/// A skill's card text and its embedding, if one is cached.
pub struct Indexed<'a> {
    pub skill: &'a Skill,
    pub vector: Option<&'a [f32]>,
}

/// Ranks the corpus against `query`.
///
/// `query_vector` is the embedded query; pass `None` to force the lexical path.
/// A skill with no cached vector is invisible to the dense stage, so a partially
/// embedded corpus degrades per-skill rather than all at once.
pub fn rank(
    tree: &SkillTree,
    corpus: &[Indexed<'_>],
    query: &str,
    query_vector: Option<&[f32]>,
    limit: usize,
) -> Vec<Ranked> {
    if limit == 0 || corpus.is_empty() {
        return Vec::new();
    }

    // At or below the limit there is nothing to choose between: return
    // everything and skip the ranker, so a small corpus never fails to surface
    // a skill and a ranker bug cannot hide one.
    if corpus.len() <= limit {
        let mut all: Vec<Ranked> = corpus
            .iter()
            .map(|c| Ranked {
                id: c.skill.id.clone(),
                score: 1.0,
                how: How::WholeCorpus,
            })
            .collect();
        all.sort_by(|a, b| a.id.cmp(&b.id));
        return all;
    }

    let dense_available = query_vector.is_some() && corpus.iter().any(|c| c.vector.is_some());

    let scored = if dense_available {
        dense_scores(corpus, query_vector.unwrap())
    } else {
        bm25_scores(corpus, query)
    };
    let how = if dense_available {
        How::Dense
    } else {
        How::Lexical
    };

    // Keep a generous pool: absorption can collapse several entries into one,
    // and the pool is what refills the gap.
    let pool: Vec<(String, f64)> = scored.into_iter().take(CANDIDATE_POOL).collect();

    let absorbed = absorb_into_parents(tree, pool);
    let mut out: Vec<Ranked> = absorbed
        .into_iter()
        .map(|(id, score)| Ranked { id, score, how })
        .collect();

    out.truncate(limit);
    promote_parents(tree, &mut out, limit);
    out
}

/// Cosine similarity against every skill that has a vector, best first.
fn dense_scores(corpus: &[Indexed<'_>], query: &[f32]) -> Vec<(String, f64)> {
    let mut scored: Vec<(String, f64)> = corpus
        .iter()
        .filter_map(|c| {
            let v = c.vector?;
            // A width mismatch means the cache predates a model change.
            // Skipping is right: a truncated comparison would score nonsense.
            if v.len() != query.len() {
                tracing::warn!(
                    id = %c.skill.id,
                    cached = v.len(),
                    query = query.len(),
                    "skill vector has the wrong width; ignoring it"
                );
                return None;
            }
            Some((c.skill.id.clone(), cosine(v, query)))
        })
        .collect();

    sort_by_score(&mut scored);
    scored
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (x, y) in a.iter().zip(b) {
        dot += (*x as f64) * (*y as f64);
        na += (*x as f64) * (*x as f64);
        nb += (*y as f64) * (*y as f64);
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Okapi BM25 over the card text. Only reached when dense is unavailable.
fn bm25_scores(corpus: &[Indexed<'_>], query: &str) -> Vec<(String, f64)> {
    let docs: Vec<(String, Vec<String>)> = corpus
        .iter()
        .map(|c| (c.skill.id.clone(), tokenize(&c.skill.index_text())))
        .collect();

    let n = docs.len() as f64;
    let avg_len = if docs.is_empty() {
        0.0
    } else {
        docs.iter().map(|(_, t)| t.len() as f64).sum::<f64>() / n
    };

    let mut df: HashMap<&str, f64> = HashMap::new();
    for (_, terms) in &docs {
        for term in terms.iter().collect::<HashSet<_>>() {
            *df.entry(term.as_str()).or_insert(0.0) += 1.0;
        }
    }

    let q_terms = tokenize(query);
    let mut scored: Vec<(String, f64)> = docs
        .iter()
        .map(|(id, terms)| {
            let len = terms.len() as f64;
            let mut tf: HashMap<&str, f64> = HashMap::new();
            for t in terms {
                *tf.entry(t.as_str()).or_insert(0.0) += 1.0;
            }

            let score = q_terms
                .iter()
                .filter_map(|q| {
                    let f = *tf.get(q.as_str())?;
                    let n_q = df.get(q.as_str()).copied().unwrap_or(0.0);
                    // Okapi IDF, floored at zero so a term present in every
                    // document cannot push a score negative.
                    let idf = (((n - n_q + 0.5) / (n_q + 0.5)) + 1.0).ln().max(0.0);
                    let denom = f + BM25_K1 * (1.0 - BM25_B + BM25_B * len / avg_len.max(1.0));
                    Some(idf * (f * (BM25_K1 + 1.0)) / denom.max(f64::EPSILON))
                })
                .sum();

            (id.clone(), score)
        })
        .filter(|(_, s)| *s > 0.0)
        .collect();

    sort_by_score(&mut scored);
    scored
}

/// Lowercase alphanumeric runs. No stemming: skill cards are short enough that
/// a stemmer's false merges cost more than its recall gains.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1)
        .map(|t| t.to_lowercase())
        .collect()
}

/// Descending by score, ties broken by id so results are reproducible.
fn sort_by_score(scored: &mut [(String, f64)]) {
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
}

/// Drops any skill whose ancestor is also in the list, crediting the ancestor
/// with the better of the two scores.
///
/// A parent's card already lists its children, so keeping both spends two aspects
/// describing one topic. The parent inherits the child's score when the child
/// scored higher, so absorbing never demotes the topic as a whole.
fn absorb_into_parents(tree: &SkillTree, scored: Vec<(String, f64)>) -> Vec<(String, f64)> {
    let present: HashMap<&str, f64> = scored.iter().map(|(id, s)| (id.as_str(), *s)).collect();

    let mut lifted: HashMap<String, f64> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for (id, score) in &scored {
        // Walk up to the highest ancestor that is also in the pool.
        let mut target = id.as_str();
        let mut cursor = id.as_str();
        while let Some(skill) = tree.get(cursor) {
            if skill.parent.is_empty() {
                break;
            }
            if present.contains_key(skill.parent.as_str()) {
                target = skill.parent.as_str();
            }
            cursor = skill.parent.as_str();
        }

        match lifted.entry(target.to_string()) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                if *score > *e.get() {
                    e.insert(*score);
                }
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(*score);
                order.push(target.to_string());
            }
        }
    }

    let mut out: Vec<(String, f64)> = order
        .into_iter()
        .map(|id| (id.clone(), lifted[&id]))
        .collect();
    sort_by_score(&mut out);
    out
}

/// Adds the parent of any matched child that has no ancestor in the result.
///
/// A child body is usually written assuming its parent's framing, so a child
/// alone can be hard to act on. The parent is appended rather than inserted, so
/// it never outranks a direct match, and `limit` is still respected.
fn promote_parents(tree: &SkillTree, out: &mut Vec<Ranked>, limit: usize) {
    let mut have: HashSet<String> = out.iter().map(|r| r.id.clone()).collect();
    let mut additions: Vec<Ranked> = Vec::new();

    for ranked in out.iter() {
        if out.len() + additions.len() >= limit {
            break;
        }
        let Some(skill) = tree.get(&ranked.id) else {
            continue;
        };
        if skill.parent.is_empty() || have.contains(&skill.parent) {
            continue;
        }
        // Slightly below the child so ordering stays meaningful.
        additions.push(Ranked {
            id: skill.parent.clone(),
            score: ranked.score * 0.99,
            how: How::ParentOfMatch,
        });
        have.insert(skill.parent.clone());
    }

    out.extend(additions);
    out.truncate(limit);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{SkillTree, discover};

    fn tree_from(files: &[(&str, &str)]) -> (tempfile::TempDir, SkillTree) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("skills");
        for (rel, body) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let tree = discover(&root).unwrap();
        (dir, tree)
    }

    fn skill_file(name: &str, brief: &str) -> String {
        format!("---\nname = \"{name}\"\nbrief = \"{brief}\"\n---\nBody of {name}.")
    }

    /// Corpus with no vectors, which forces the lexical path.
    fn lexical_corpus(tree: &SkillTree) -> Vec<Indexed<'_>> {
        tree.all()
            .into_iter()
            .map(|skill| Indexed {
                skill,
                vector: None,
            })
            .collect()
    }

    #[test]
    fn a_corpus_at_or_below_the_limit_is_returned_whole() {
        let (_d, tree) = tree_from(&[
            ("a.md", &skill_file("A", "Alpha things.")),
            ("b.md", &skill_file("B", "Beta things.")),
        ]);
        let corpus = lexical_corpus(&tree);
        let out = rank(&tree, &corpus, "anything at all", None, 10);

        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|r| r.how == How::WholeCorpus));
    }

    #[test]
    fn lexical_ranking_finds_the_obvious_match() {
        let files: Vec<(String, String)> = [
            ("rollback", "Restore a component to an earlier revision."),
            ("embeddings", "Vectorise text for semantic search."),
            ("terminal", "Run shell commands in a session."),
            ("gateway", "Serve the chat interface over HTTP."),
            ("config", "Read and write orchestrator settings."),
        ]
        .iter()
        .map(|(n, b)| (format!("{n}.md"), skill_file(n, b)))
        .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (_d, tree) = tree_from(&refs);

        let corpus = lexical_corpus(&tree);
        let out = rank(
            &tree,
            &corpus,
            "how do I restore an earlier revision",
            None,
            3,
        );

        assert_eq!(out[0].id, "rollback");
        assert_eq!(out[0].how, How::Lexical);
    }

    #[test]
    fn dense_ranking_is_preferred_when_vectors_exist() {
        let files: Vec<(String, String)> = (0..6)
            .map(|i| (format!("s{i}.md"), skill_file(&format!("S{i}"), "A skill.")))
            .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (_d, tree) = tree_from(&refs);

        let all = tree.all();
        // s3 is the only vector pointing the same way as the query.
        let vectors: Vec<Vec<f32>> = all
            .iter()
            .map(|s| {
                if s.id == "s3" {
                    vec![1.0, 0.0]
                } else {
                    vec![0.0, 1.0]
                }
            })
            .collect();
        let corpus: Vec<Indexed<'_>> = all
            .iter()
            .zip(&vectors)
            .map(|(skill, v)| Indexed {
                skill,
                vector: Some(v.as_slice()),
            })
            .collect();

        let out = rank(&tree, &corpus, "irrelevant text", Some(&[1.0, 0.0]), 3);
        assert_eq!(out[0].id, "s3");
        assert_eq!(out[0].how, How::Dense);
    }

    #[test]
    fn a_vector_of_the_wrong_width_is_ignored_not_compared() {
        let files: Vec<(String, String)> = (0..6)
            .map(|i| (format!("s{i}.md"), skill_file(&format!("S{i}"), "A skill.")))
            .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (_d, tree) = tree_from(&refs);

        let all = tree.all();
        let stale = vec![1.0f32, 0.0, 0.0, 0.0];
        let fresh = vec![0.9f32, 0.1];
        let corpus: Vec<Indexed<'_>> = all
            .iter()
            .map(|skill| Indexed {
                skill,
                vector: Some(if skill.id == "s0" {
                    stale.as_slice()
                } else {
                    fresh.as_slice()
                }),
            })
            .collect();

        let out = rank(&tree, &corpus, "q", Some(&[1.0, 0.0]), 5);
        assert!(
            !out.iter().any(|r| r.id == "s0"),
            "a mismatched vector must not be scored"
        );
    }

    #[test]
    fn no_vectors_at_all_falls_back_to_lexical() {
        let files: Vec<(String, String)> = [
            ("rollback", "Restore a component to an earlier revision."),
            ("embeddings", "Vectorise text for semantic search."),
            ("terminal", "Run shell commands in a session."),
            ("gateway", "Serve the chat interface over HTTP."),
        ]
        .iter()
        .map(|(n, b)| (format!("{n}.md"), skill_file(n, b)))
        .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (_d, tree) = tree_from(&refs);

        let corpus = lexical_corpus(&tree);
        // A query vector is offered, but nothing in the corpus can answer it.
        let out = rank(
            &tree,
            &corpus,
            "shell commands in a session",
            Some(&[1.0, 0.0]),
            2,
        );

        assert_eq!(out[0].id, "terminal");
        assert_eq!(out[0].how, How::Lexical);
    }

    #[test]
    fn a_parent_absorbs_its_own_matching_children() {
        let mut files = vec![
            (
                "p/SKILL.md".to_string(),
                skill_file("P", "Deployment topics."),
            ),
            (
                "p/a/SKILL.md".to_string(),
                skill_file("A", "Deployment step one."),
            ),
            (
                "p/b/SKILL.md".to_string(),
                skill_file("B", "Deployment step two."),
            ),
        ];
        for i in 0..5 {
            files.push((
                format!("filler{i}.md"),
                skill_file(&format!("F{i}"), "Unrelated filler."),
            ));
        }
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (_d, tree) = tree_from(&refs);

        let corpus = lexical_corpus(&tree);
        let out = rank(&tree, &corpus, "deployment", None, 5);
        let ids: Vec<&str> = out.iter().map(|r| r.id.as_str()).collect();

        assert!(ids.contains(&"p"), "the parent should be present");
        assert!(
            !ids.contains(&"p/a") && !ids.contains(&"p/b"),
            "children of a present parent should be absorbed, got {ids:?}"
        );
    }

    #[test]
    fn a_matching_child_pulls_in_its_parent() {
        let mut files = vec![
            ("p/SKILL.md".to_string(), skill_file("P", "Umbrella topic.")),
            (
                "p/kid/SKILL.md".to_string(),
                skill_file("Kid", "Zygomorphic flange calibration."),
            ),
        ];
        for i in 0..8 {
            files.push((
                format!("filler{i}.md"),
                skill_file(&format!("F{i}"), "Unrelated filler."),
            ));
        }
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (_d, tree) = tree_from(&refs);

        let corpus = lexical_corpus(&tree);
        let out = rank(&tree, &corpus, "zygomorphic flange", None, 4);

        assert_eq!(out[0].id, "p/kid");
        let parent = out.iter().find(|r| r.id == "p").expect("parent promoted");
        assert_eq!(parent.how, How::ParentOfMatch);
        assert!(parent.score < out[0].score);
    }

    #[test]
    fn promotion_never_exceeds_the_limit() {
        let mut files = vec![
            ("p/SKILL.md".to_string(), skill_file("P", "Umbrella.")),
            (
                "p/kid/SKILL.md".to_string(),
                skill_file("Kid", "Widget grinding."),
            ),
        ];
        for i in 0..8 {
            files.push((
                format!("f{i}.md"),
                skill_file(&format!("F{i}"), "Widget grinding also."),
            ));
        }
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (_d, tree) = tree_from(&refs);

        let corpus = lexical_corpus(&tree);
        let out = rank(&tree, &corpus, "widget grinding", None, 3);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn results_are_deterministic_when_scores_tie() {
        let files: Vec<(String, String)> = (0..8)
            .map(|i| {
                (
                    format!("s{i}.md"),
                    skill_file(&format!("S{i}"), "Identical brief text."),
                )
            })
            .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (_d, tree) = tree_from(&refs);

        let corpus = lexical_corpus(&tree);
        let first = rank(&tree, &corpus, "identical brief", None, 4);
        let second = rank(&tree, &corpus, "identical brief", None, 4);
        assert_eq!(first, second);
    }

    #[test]
    fn a_query_matching_nothing_returns_nothing() {
        let files: Vec<(String, String)> = (0..8)
            .map(|i| {
                (
                    format!("s{i}.md"),
                    skill_file(&format!("S{i}"), "Alpha beta."),
                )
            })
            .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (_d, tree) = tree_from(&refs);

        let corpus = lexical_corpus(&tree);
        let out = rank(&tree, &corpus, "zzzz qqqq", None, 5);
        assert!(out.is_empty());
    }

    #[test]
    fn a_zero_limit_returns_nothing() {
        let (_d, tree) = tree_from(&[("a.md", &skill_file("A", "Alpha."))]);
        let corpus = lexical_corpus(&tree);
        assert!(rank(&tree, &corpus, "alpha", None, 0).is_empty());
    }

    #[test]
    fn tokenizing_drops_punctuation_and_single_characters() {
        assert_eq!(
            tokenize("Roll-back: a component's revision! (v2)"),
            ["roll", "back", "component", "revision", "v2"]
        );
    }

    #[test]
    fn cosine_is_one_for_parallel_and_zero_for_orthogonal() {
        assert!((cosine(&[1.0, 2.0], &[2.0, 4.0]) - 1.0).abs() < 1e-9);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-9);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }
}
