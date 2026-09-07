//! Load the ToolRet corpus straight into the ranker's own data structures.
//!
//! No files are written. `SkillTree`'s fields are public, so 10,000 documents
//! become 10,000 `Skill` values in memory — which matters, because the
//! alternative (writing 10,000 `SKILL.md` files so `discover()` can read them
//! back) would add minutes of disk churn per run and a temp tree to clean up,
//! for no gain in fidelity. `rank()` only ever sees a `SkillTree`, and this
//! builds exactly the one `discover()` would have built.
//!
//! One structural note. ToolRet is flat: tools have no parent/child relation, so
//! every document is a root at depth 0. Parent absorption and child promotion
//! therefore have nothing to act on in this corpus, and their ablation arms will
//! correctly show no movement. That is a real property of flat tool corpora, not
//! a harness failure — those two mechanisms exist for *our* nested skill tree,
//! and the small hand-written gold set remains the place they are measured.
//! `--group-tree` exists to give them something to bite on: it reparents each
//! tool under its synthesised bucket, turning the flat list into a two-level
//! tree of the shape our own corpus has.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::ablate::Case;
use crate::lifted::skills::{self, ChildSpec, Skill, SkillTree};

/// One case from `gold/skillret.json`.
///
/// `relevant` is a map of id to graded relevance (2 = the skill the user needed,
/// 1 = genuinely useful), which nDCG uses directly as gain. Separate from
/// `QueryDoc` because ToolRet stores its labels in a different shape.
#[derive(Deserialize)]
struct SkillGoldCase {
    query: String,
    #[serde(default)]
    relevant: HashMap<String, f64>,
}

#[derive(Deserialize)]
struct SkillGoldFile {
    cases: Vec<SkillGoldCase>,
}

/// Load a real Thetis skills tree and the hand-written SkillRet gold set.
///
/// Uses the lifted `discover()` — the same function the running system calls —
/// so the tree's parents, depths and card text are built by shipped code rather
/// than reconstructed here.
///
/// Gold ids that do not exist in the tree are left in place deliberately: they
/// score zero and `unresolvable()` reports them, which is honest, whereas
/// dropping them would quietly inflate every arm equally.
pub fn load_skills(dir: &Path, gold_path: &Path) -> Result<(ToolRetFile, SkillTree, Vec<Case>)> {
    let tree = skills::discover(dir)
        .with_context(|| format!("discovering skills in {}", dir.display()))?;

    let raw = std::fs::read_to_string(gold_path)
        .with_context(|| format!("reading gold set {}", gold_path.display()))?;
    let gold: SkillGoldFile = serde_json::from_str(&raw)
        .with_context(|| format!("parsing gold set {}", gold_path.display()))?;

    let cases: Vec<Case> = gold
        .cases
        .iter()
        .enumerate()
        .map(|(i, c)| Case {
            // The gold file carries no ids, but the score cache is keyed by one,
            // so index the cases. Stable as long as the file order is stable.
            id: format!("skillret-{i}"),
            query: c.query.clone(),
            gold: c.relevant.clone(),
        })
        .collect();

    let file = ToolRetFile {
        source: format!(
            "{} + {}",
            dir.display(),
            gold_path.file_name().unwrap_or_default().to_string_lossy()
        ),
        note: "real Thetis skills tree, hand-written gold set".into(),
        tools: Vec::new(),
        queries: Vec::new(),
        groups: Vec::new(),
    };

    Ok((file, tree, cases))
}

#[derive(Deserialize)]
pub struct ToolRetFile {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub note: String,
    pub tools: Vec<ToolDoc>,
    pub queries: Vec<QueryDoc>,
    #[serde(default)]
    pub groups: Vec<GroupDoc>,
}

#[derive(Deserialize)]
pub struct ToolDoc {
    pub id: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub brief: String,
    #[serde(default)]
    pub when_to_use: String,
    #[serde(default)]
    pub subset: String,
}

#[derive(Deserialize)]
pub struct QueryDoc {
    pub id: String,
    pub query: String,
    #[serde(default)]
    pub subset: String,
    pub relevant: HashMap<String, f64>,
}

#[derive(Deserialize)]
pub struct GroupDoc {
    pub id: String,
    #[serde(default)]
    pub brief: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub members: Vec<String>,
}

/// A tool document as the ranker sees it.
///
/// `index_text()` is `name \n brief \n when_to_use [\n tags]`, so the fields are
/// filled to put the same information in the same places a real skill card would
/// — otherwise the corpus would be testing a different text layout, not a
/// different corpus.
fn to_skill(t: &ToolDoc, parent: &str, depth: usize) -> Skill {
    let mut tags = Vec::new();
    if !t.group.is_empty() {
        tags.push(t.group.clone());
    }
    if !t.subset.is_empty() && t.subset != t.group {
        tags.push(t.subset.clone());
    }

    Skill {
        id: t.id.clone(),
        path: Path::new(&t.id).to_path_buf(),
        parent: parent.to_string(),
        depth,
        name: if t.name.is_empty() {
            t.id.clone()
        } else {
            t.name.clone()
        },
        brief: t.brief.clone(),
        when_to_use: t.when_to_use.clone(),
        universal: false,
        tags,
        children: ChildSpec::Auto,
        related: Vec::new(),
        status: String::new(),
        superseded_by: String::new(),
        version: 1,
        // Bodies are never ranked -- index_text() covers name, brief,
        // when_to_use and tags only -- so leaving them empty costs no fidelity
        // and saves holding 10k JSON schemas in memory twice.
        body: String::new(),
        resources: Vec::new(),
        // The real content_hash keys the production vector cache. This harness
        // keys its own cache on the card text it actually embeds, so this field
        // is unused here and left empty rather than faked.
        content_hash: String::new(),
        owns_dir: true,
    }
}

/// Build the tree. With `group_tree`, tools are nested under their bucket so the
/// structural stages have real parents to work with.
pub fn load(path: &Path, group_tree: bool) -> Result<(ToolRetFile, SkillTree, Vec<Case>)> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading corpus {}", path.display()))?;
    let file: ToolRetFile =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;

    let mut skills: HashMap<String, Skill> = HashMap::new();
    let mut roots: Vec<String> = Vec::new();

    if group_tree {
        // A synthetic parent per bucket. Its card text is the bucket's own brief
        // and tags, which is what our real parent skills look like.
        for g in &file.groups {
            let holder = ToolDoc {
                id: g.id.clone(),
                group: String::new(),
                name: g.id.clone(),
                brief: g.brief.clone(),
                when_to_use: String::new(),
                subset: String::new(),
            };
            let mut s = to_skill(&holder, "", 0);
            s.tags = g.tags.clone();
            skills.insert(g.id.clone(), s);
            roots.push(g.id.clone());
        }
    }

    for t in &file.tools {
        let (parent, depth) = if group_tree && !t.group.is_empty() {
            // A nested id is what makes the parent link real: absorption walks
            // ids, so "web/foo" is what ties foo to web.
            (t.group.clone(), 1)
        } else {
            (String::new(), 0)
        };

        let mut s = to_skill(t, &parent, depth);
        if group_tree && depth == 1 {
            s.id = format!("{}/{}", t.group, t.id);
        }
        if depth == 0 {
            roots.push(s.id.clone());
        }
        skills.insert(s.id.clone(), s);
    }

    // Queries carry bare tool ids; under --group-tree the corpus ids gained a
    // bucket prefix, so gold ids must be remapped or every case scores zero.
    let mut remap: HashMap<String, String> = HashMap::new();
    if group_tree {
        for t in &file.tools {
            if !t.group.is_empty() {
                remap.insert(t.id.clone(), format!("{}/{}", t.group, t.id));
            }
        }
    }

    let cases: Vec<Case> = file
        .queries
        .iter()
        .map(|q| Case {
            id: q.id.clone(),
            query: q.query.clone(),
            gold: q
                .relevant
                .iter()
                .map(|(k, v)| (remap.get(k).cloned().unwrap_or_else(|| k.clone()), *v))
                .collect(),
        })
        .collect();

    roots.sort();
    let tree = SkillTree { skills, roots };
    Ok((file, tree, cases))
}

/// Gold ids that no longer resolve. Should be empty; a non-zero count means
/// every affected case is silently unanswerable and dragging the mean down.
pub fn unresolvable(tree: &SkillTree, cases: &[Case]) -> Vec<String> {
    let mut bad: Vec<String> = cases
        .iter()
        .flat_map(|c| c.gold.keys())
        .filter(|id| tree.get(id).is_none())
        .cloned()
        .collect();
    bad.sort();
    bad.dedup();
    bad
}
