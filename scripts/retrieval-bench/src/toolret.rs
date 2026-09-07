//! ToolRet: does group routing attach the tools a task needs, and withhold the
//! rest?
//!
//! Scores the real `score`/`tag_present` from the shipping router, lifted by
//! extract.py, against the real threshold. `route_once` itself cannot be called
//! -- it reads and writes a pinned session through host imports that do not
//! exist outside wasm -- so its *decision* is reproduced here from the same
//! parts it uses: always-on, plus the skill edge, plus tags over threshold.
//!
//! That reproduction is the one soft spot in this benchmark, and it is confined
//! to `attach` below, which is a dozen lines and no arithmetic: every number it
//! compares comes out of lifted code.

use crate::lifted::table;
use crate::metrics::{self, SetScore};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Deserialize)]
struct GoldFile {
    cases: Vec<GoldCase>,
}

#[derive(Deserialize)]
struct GoldCase {
    query: String,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    want: Vec<String>,
    #[serde(default)]
    unwanted: Vec<String>,
}

#[derive(Serialize)]
pub struct CaseResult {
    pub query: String,
    pub attached: Vec<String>,
    pub missing: Vec<String>,
    pub spurious: Vec<String>,
    pub recall: f64,
    pub specificity: f64,
    pub f1: f64,
}

#[derive(Serialize)]
pub struct ToolRet {
    pub threshold: f64,
    pub groups: usize,
    pub cases: usize,
    /// The headline: both sides at once, with the skill edge in play, which is
    /// how routing actually runs.
    pub f1: f64,
    pub recall: f64,
    pub specificity: f64,
    /// The same score with skill edges suppressed, isolating what tags alone
    /// achieve. The gap between the two is the value the skill edge adds, and it
    /// is the number to look at before editing a tag list -- most apparent tag
    /// problems are really missing `tool-group:` tags on a skill.
    pub f1_tags_only: f64,
    pub recall_tags_only: f64,
    /// Skills in the gold set carrying a `tool-group:` tag that names a group
    /// that does not exist, and group ids in the gold set that are not in the
    /// table. Either makes cases unwinnable for reasons unrelated to ranking.
    pub unknown_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_case: Option<Vec<CaseResult>>,
}

/// Which groups routing would attach: always-on, plus those the retrieved
/// skills point at, plus those whose tags clear the threshold.
///
/// Mirrors `groups::route_once` minus the pin, the configured always-on list
/// (empty in the shipped config) and `tool_search` admission, none of which are
/// part of the opening-message decision this measures.
fn attach(query: &str, skill_groups: &[String]) -> Vec<String> {
    let tokens = table::tokens(query);
    let threshold = threshold();

    let mut out: Vec<String> = Vec::new();
    for group in table::all() {
        let admit = group.always_on
            || skill_groups.iter().any(|g| g == group.id)
            || table::score(group, &tokens) >= threshold;
        if admit && !out.iter().any(|g| g == group.id) {
            out.push(group.id.to_string());
        }
    }
    out
}

/// The shipped default. Overridable so a threshold sweep does not need a rebuild.
fn threshold() -> f64 {
    std::env::var("THETIS_ROUTE_THRESHOLD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.15)
}

/// Group ids the named skills point at, read from the live corpus rather than
/// the gold file, so a `tool-group:` tag added or removed in a skill shows up
/// here as a routing change.
fn groups_from_skills(
    tree: &crate::lifted::skills::SkillTree,
    ids: &[String],
    unknown: &mut Vec<String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for id in ids {
        let Some(skill) = tree.get(id) else {
            unknown.push(format!("skill:{id}"));
            continue;
        };
        for tag in &skill.tags {
            if let Some(group) = tag.strip_prefix(table::SKILL_TAG_PREFIX) {
                let group = group.trim();
                if table::find(group).is_none() {
                    unknown.push(format!("group:{group} (from skill {id})"));
                } else if !out.iter().any(|g| g == group) {
                    out.push(group.to_string());
                }
            }
        }
    }
    out
}

pub fn run(
    skills_dir: &Path,
    gold_path: &Path,
    per_case: bool,
) -> Result<ToolRet> {
    let tree = crate::lifted::skills::discover(skills_dir)
        .with_context(|| format!("discovering skills in {}", skills_dir.display()))?;

    let gold: GoldFile = serde_json::from_str(
        &std::fs::read_to_string(gold_path)
            .with_context(|| format!("reading gold set {}", gold_path.display()))?,
    )
    .context("gold set is not valid JSON")?;

    let mut unknown: Vec<String> = Vec::new();
    let known: Vec<String> = table::all().iter().map(|g| g.id.to_string()).collect();

    let mut results = Vec::new();
    let mut tags_only = Vec::new();

    for case in &gold.cases {
        for id in case.want.iter().chain(&case.unwanted) {
            if !known.contains(id) {
                unknown.push(format!("gold group:{id}"));
            }
        }

        let skill_groups = groups_from_skills(&tree, &case.skills, &mut unknown);

        let attached = attach(&case.query, &skill_groups);
        results.push(score_case(case, attached));

        let attached_tags = attach(&case.query, &[]);
        tags_only.push(score(case, &attached_tags));
    }

    unknown.sort();
    unknown.dedup();

    let full: Vec<SetScore> = gold
        .cases
        .iter()
        .zip(&results)
        .map(|(case, r)| score(case, &r.attached))
        .collect();

    Ok(ToolRet {
        threshold: threshold(),
        groups: table::all().len(),
        cases: results.len(),
        f1: metrics::mean(&full.iter().map(|s| s.f1()).collect::<Vec<_>>()),
        recall: metrics::mean(&full.iter().map(|s| s.recall()).collect::<Vec<_>>()),
        specificity: metrics::mean(&full.iter().map(|s| s.specificity()).collect::<Vec<_>>()),
        f1_tags_only: metrics::mean(&tags_only.iter().map(|s| s.f1()).collect::<Vec<_>>()),
        recall_tags_only: metrics::mean(
            &tags_only.iter().map(|s| s.recall()).collect::<Vec<_>>(),
        ),
        unknown_ids: unknown,
        per_case: per_case.then_some(results),
    })
}

fn score(case: &GoldCase, attached: &[String]) -> SetScore {
    SetScore {
        wanted_found: case.want.iter().filter(|w| attached.contains(w)).count(),
        wanted_total: case.want.len(),
        unwanted_attached: case.unwanted.iter().filter(|u| attached.contains(u)).count(),
        unwanted_total: case.unwanted.len(),
    }
}

fn score_case(case: &GoldCase, attached: Vec<String>) -> CaseResult {
    let s = score(case, &attached);
    CaseResult {
        query: case.query.clone(),
        missing: case
            .want
            .iter()
            .filter(|w| !attached.contains(w))
            .cloned()
            .collect(),
        spurious: case
            .unwanted
            .iter()
            .filter(|u| attached.contains(u))
            .cloned()
            .collect(),
        recall: s.recall(),
        specificity: s.specificity(),
        f1: s.f1(),
        attached,
    }
}
