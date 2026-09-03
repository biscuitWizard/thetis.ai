//! The service layer behind the `skills` host interface.
//!
//! Everything the guest can do to skills goes through here: discovery is cached
//! so a turn does not re-walk the tree on every call, retrieval results are
//! pinned per session so the system prompt stays byte-stable across turns, and
//! every write is confined to the configured skills directory.
//!
//! This module deliberately knows nothing about WIT. It returns plain structs
//! and `host_api` maps them, which keeps the whole thing testable without
//! standing up a component.

use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::config::Config;
use crate::embeddings::Embedder;
use crate::skill_index::{self, How, Indexed};
use crate::skills::{self, Skill, SkillTree};

/// Where a session's pinned skill ids live. Session-scoped, so a conversation
/// keeps the skills it was given even as the corpus changes underneath it.
const PINNED_KEY: &str = "__skills_pinned";

/// One pinned skill as it is stored.
///
/// The score and the ranking method are kept alongside the id so the inspector
/// can show *why* a skill is in this conversation's prompt. Storing ids alone
/// meant every card read back as score 0.0 "pinned", which is true but useless.
#[derive(serde::Serialize, serde::Deserialize)]
struct PinnedEntry {
    id: String,
    #[serde(default)]
    score: f64,
    #[serde(default)]
    how: String,
}

/// Hard ceiling on a single `fetch`, below the agent's tool-output cap so a body
/// is truncated here, with a resumable offset, rather than clipped downstream
/// where the agent cannot tell how much it lost.
const MAX_FETCH_CHARS: usize = 24_000;

/// A skill as the agent sees it before reading the body.
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub id: String,
    pub parent: String,
    pub name: String,
    pub brief: String,
    pub when_to_use: String,
    pub tags: Vec<String>,
    /// Ids of nested skills, so the agent can descend without a second call.
    pub children: Vec<String>,
    pub universal: bool,
    pub resources: Vec<String>,
    /// Ids of neighbouring skills, from frontmatter `related`.
    pub related: Vec<String>,
    /// "active" or "retired". Empty means active and unstated.
    pub status: String,
    /// For a retired skill, the id that replaced it. Empty otherwise.
    pub superseded_by: String,
    /// Retrieval score; 0.0 when the card was not produced by ranking.
    pub score: f64,
    /// How this card was selected: dense, lexical, parent-of-match,
    /// whole-corpus, universal, pinned, or direct.
    pub how: String,
}

/// A skill's instructions, or one of its resource files.
#[derive(Debug, Clone, PartialEq)]
pub struct Body {
    pub id: String,
    pub name: String,
    /// Which file this is: empty for the skill's own body, else the resource
    /// path such as `references/format.md`.
    pub resource: String,
    pub content: String,
    pub resources: Vec<String>,
    pub children: Vec<String>,
    /// Where this slice started, in characters.
    pub offset: usize,
    /// Total length of the file, so the agent can tell how much remains.
    pub total: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Diag {
    pub id: String,
    pub severity: String,
    pub message: String,
}

/// What a write did, plus whatever linting found once it landed.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteOutcome {
    pub id: String,
    pub path: String,
    pub created: bool,
    pub diagnostics: Vec<Diag>,
}

pub struct SkillManager {
    cfg: Arc<Config>,
    persist: crate::persist::Persist,
    embedder: Embedder,
    /// Discovery is a directory walk plus a parse per file, and the agent may
    /// call several times in one turn, so the tree is cached until something
    /// writes to it.
    tree: RwLock<Option<Arc<SkillTree>>>,
}

impl SkillManager {
    pub fn new(cfg: Arc<Config>, persist: crate::persist::Persist) -> Result<Self> {
        let embedder = Embedder::new(cfg.clone(), persist.clone())?;
        Ok(Self {
            cfg,
            persist,
            embedder,
            tree: RwLock::new(None),
        })
    }

    /// The current tree, discovering it if the cache is cold.
    ///
    /// A discovery failure yields an empty tree rather than an error: a broken
    /// skills directory should cost the agent its skills, not its turn.
    pub fn tree(&self) -> Arc<SkillTree> {
        if let Some(t) = self.tree.read().ok().and_then(|g| g.clone()) {
            return t;
        }

        let fresh = Arc::new(
            skills::discover(&self.cfg.paths.skills).unwrap_or_else(|e| {
                tracing::warn!(error = %e, dir = %self.cfg.paths.skills.display(),
                    "skill discovery failed; serving an empty tree");
                SkillTree::default()
            }),
        );

        if let Ok(mut g) = self.tree.write() {
            *g = Some(fresh.clone());
        }
        fresh
    }

    /// Drops the cached tree. Called after any write, and by the watcher when
    /// the skills directory changes underneath us.
    pub fn invalidate(&self) {
        if let Ok(mut g) = self.tree.write() {
            *g = None;
        }
    }

    // --- reading ----------------------------------------------------------

    /// Skills marked universal, capped by configuration.
    ///
    /// The cap is enforced here as well as in the linter, because a lint warning
    /// is advice while this is the thing that actually protects the prompt.
    pub fn universal(&self) -> Vec<Card> {
        let tree = self.tree();
        let max = self
            .cfg
            .skills
            .max_universal
            .min(skills::MAX_UNIVERSAL_COUNT);

        let all = tree.universal();
        if all.len() > max {
            tracing::warn!(
                found = all.len(),
                cap = max,
                "more universal skills than the cap allows; ignoring the excess"
            );
        }

        all.into_iter()
            .take(max)
            .map(|s| self.card(s, &tree, 0.0, "universal"))
            .collect()
    }

    /// Every skill, parents before children, for a tree view.
    ///
    /// Ordered by id, which for path-derived ids puts each parent immediately
    /// before its own children, so a renderer can indent by depth without
    /// building the tree itself.
    pub fn all(&self) -> Vec<Card> {
        let tree = self.tree();
        let mut out: Vec<Card> = tree
            .all()
            .into_iter()
            .map(|s| self.card(s, &tree, 0.0, "tree"))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Ranks the corpus against `query` without touching session state.
    ///
    /// May return fewer than `limit`. The dense path scores every skill, so it
    /// fills; the lexical fallback drops skills sharing no term with the query,
    /// which means a short result rather than a padded one.
    pub async fn search(&self, query: &str, limit: usize) -> Vec<Card> {
        // A limit of 0 means the configured default. This is resolved here,
        // the lowest point every caller passes through, because when only
        // `retrieve` did it a direct `search(q, 0)` silently returned nothing.
        let limit = if limit == 0 {
            self.cfg.skills.retrieve_limit
        } else {
            limit
        };

        let tree = self.tree();
        if tree.is_empty() || limit == 0 {
            return Vec::new();
        }

        let all = tree.all();
        let (vectors, stats) = self.embedder.vectors_for(&all).await;
        let query_vector = match self.embedder.embed_query(query).await {
            Ok(v) => Some(v),
            Err(e) => {
                // Not fatal: the ranker falls back to BM25 over the same corpus.
                tracing::warn!(error = %e, "query embedding failed; ranking lexically");
                None
            }
        };

        tracing::debug!(
            skills = all.len(),
            cached = stats.hits,
            fetched = stats.fetched,
            missing = stats.missing,
            dense = query_vector.is_some(),
            "ranking skills"
        );

        let corpus: Vec<Indexed<'_>> = all
            .iter()
            .zip(&vectors)
            .map(|(skill, v)| Indexed {
                skill,
                vector: v.as_deref(),
            })
            .collect();

        let ranked = skill_index::rank(&tree, &corpus, query, query_vector.as_deref(), limit);

        ranked
            .into_iter()
            .filter_map(|r| {
                tree.get(&r.id)
                    .map(|s| self.card(s, &tree, r.score, r.how.label()))
            })
            .collect()
    }

    /// Retrieves for a session and pins the result, so later turns rebuild the
    /// same prompt prefix instead of re-ranking and breaking the cache.
    pub async fn retrieve(&self, session_id: &str, query: &str, limit: usize) -> Vec<Card> {
        // `search` resolves a limit of 0 to the configured default.
        let cards = self.search(query, limit).await;
        // Keep the scores: this is the one moment they are known, and the
        // inspector's whole value is showing why a skill was chosen.
        let entries: Vec<PinnedEntry> = cards
            .iter()
            .map(|c| PinnedEntry {
                id: c.id.clone(),
                score: c.score,
                how: c.how.clone(),
            })
            .collect();
        if let Err(e) = self.write_pinned_entries(session_id, &entries).await {
            // The cards are still usable; only the stickiness is lost.
            tracing::warn!(error = %e, session = session_id, "could not pin retrieved skills");
        }
        cards
    }

    /// The cards pinned for a session, in the order they were pinned.
    ///
    /// Ids that no longer resolve are dropped silently: a skill deleted
    /// mid-conversation should not wedge every later turn.
    pub async fn pinned(&self, session_id: &str) -> Vec<Card> {
        let tree = self.tree();
        self.read_pinned_entries(session_id)
            .await
            .into_iter()
            .filter_map(|e| {
                tree.get(&e.id)
                    .map(|s| self.card(s, &tree, e.score, &e.how))
            })
            .collect()
    }

    /// Replaces a session's pinned set. Unknown ids are refused rather than
    /// stored, so the pin list cannot fill with ids that never resolve.
    pub async fn pin(&self, session_id: &str, ids: &[String]) -> Result<Vec<Card>> {
        let tree = self.tree();
        let mut unknown = Vec::new();
        for id in ids {
            if tree.get(id).is_none() {
                unknown.push(id.clone());
            }
        }
        if !unknown.is_empty() {
            return Err(anyhow!("no such skill: {}", unknown.join(", ")));
        }

        self.write_pinned(session_id, ids).await?;
        Ok(ids
            .iter()
            .filter_map(|id| tree.get(id).map(|s| self.card(s, &tree, 0.0, "pinned")))
            .collect())
    }

    /// One skill's body, or one of its resource files.
    pub fn fetch(&self, id: &str, resource: &str, offset: usize, limit: usize) -> Result<Body> {
        let tree = self.tree();
        let skill = tree.get(id).ok_or_else(|| anyhow!("no such skill: {id}"))?;

        let (name, text) = if resource.is_empty() {
            (String::new(), skill.body.clone())
        } else {
            let path = self.resource_path(skill, resource)?;
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            (resource.to_string(), text)
        };

        // Slice by character, not byte, so a multi-byte character never gets cut
        // in half and handed to the model as broken UTF-8.
        let chars: Vec<char> = text.chars().collect();
        let total = chars.len();
        let start = offset.min(total);
        let want = if limit == 0 { MAX_FETCH_CHARS } else { limit }.min(MAX_FETCH_CHARS);
        let end = (start + want).min(total);

        Ok(Body {
            id: skill.id.clone(),
            name: skill.name.clone(),
            resource: name,
            content: chars[start..end].iter().collect(),
            resources: skill.resources.clone(),
            children: tree
                .children(&skill.id)
                .into_iter()
                .map(|c| c.id.clone())
                .collect(),
            offset: start,
            total,
            truncated: end < total,
        })
    }

    pub fn lint(&self, id: &str) -> Vec<Diag> {
        let tree = self.tree();
        let found = if id.is_empty() {
            skills::lint_all(&tree)
        } else {
            skills::lint(&tree, id)
        };
        found
            .into_iter()
            .map(|d| Diag {
                id: d.id,
                severity: d.severity.label().to_string(),
                message: d.message,
            })
            .collect()
    }

    // --- writing ----------------------------------------------------------

    /// Creates or replaces a skill's `SKILL.md`, or one of its resources.
    ///
    /// The whole file is replaced, matching how the agent edits code, and the
    /// result is linted immediately so a malformed frontmatter comes back in the
    /// same call rather than at the next retrieval.
    pub fn upsert(&self, id: &str, resource: &str, contents: &str) -> Result<WriteOutcome> {
        let id = normalize_id(id)?;

        let path = if resource.is_empty() {
            self.skill_dir(&id)?.join("SKILL.md")
        } else {
            let dir = self.skill_dir(&id)?;
            let p = safe_join(&dir, resource)?;
            check_resource_shape(&id, resource)?;
            p
        };

        let created = !path.exists();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;

        self.invalidate();

        // Lint after the write so the diagnostics describe what is now on disk.
        let tree = self.tree();
        let diagnostics = if tree.get(&id).is_some() {
            self.lint(&id)
        } else {
            vec![Diag {
                id: id.clone(),
                severity: "error".into(),
                message: format!(
                    "wrote {} but no skill with id '{id}' was discovered afterwards; \
                     check the frontmatter parses as TOML between --- fences",
                    display_relative(&path, &self.cfg.paths.skills)
                ),
            }]
        };

        Ok(WriteOutcome {
            id,
            path: display_relative(&path, &self.cfg.paths.skills),
            created,
            diagnostics,
        })
    }

    /// Deletes a skill. Refuses a skill with children unless `recursive`, so
    /// removing a parent cannot silently orphan a subtree.
    pub fn remove(&self, id: &str, recursive: bool) -> Result<String> {
        let id = normalize_id(id)?;
        let tree = self.tree();
        let skill = tree
            .get(&id)
            .ok_or_else(|| anyhow!("no such skill: {id}"))?;

        let kids = tree.children(&id);
        if !kids.is_empty() && !recursive {
            let names: Vec<&str> = kids.iter().map(|k| k.id.as_str()).collect();
            return Err(anyhow!(
                "'{id}' has nested skills ({}); pass recursive to delete them too",
                names.join(", ")
            ));
        }

        // Only a skill that owns its directory may take the directory with it.
        // A bare `<id>.md` shares its parent with unrelated skills.
        let target = if skill.owns_dir {
            skill
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| skill.path.clone())
        } else {
            skill.path.clone()
        };

        let confined = self.confine(&target)?;
        if confined == self.cfg.paths.skills {
            return Err(anyhow!("refusing to delete the skills directory itself"));
        }

        if confined.is_dir() {
            std::fs::remove_dir_all(&confined)
                .with_context(|| format!("removing {}", confined.display()))?;
        } else {
            std::fs::remove_file(&confined)
                .with_context(|| format!("removing {}", confined.display()))?;
        }

        self.invalidate();
        Ok(display_relative(&confined, &self.cfg.paths.skills))
    }

    /// Drops cached vectors for skills that no longer exist.
    pub async fn prune_vectors(&self) -> Result<usize> {
        let tree = self.tree();
        let all = tree.all();
        self.embedder.prune(&all).await
    }

    // --- internals --------------------------------------------------------

    fn card(&self, s: &Skill, tree: &SkillTree, score: f64, how: &str) -> Card {
        Card {
            id: s.id.clone(),
            parent: s.parent.clone(),
            name: s.name.clone(),
            brief: s.brief.clone(),
            when_to_use: s.when_to_use.clone(),
            tags: s.tags.clone(),
            children: tree
                .children(&s.id)
                .into_iter()
                .map(|c| c.id.clone())
                .collect(),
            universal: s.universal,
            resources: s.resources.clone(),
            related: s.related.clone(),
            status: s.status.clone(),
            superseded_by: s.superseded_by.clone(),
            score,
            how: how.to_string(),
        }
    }

    /// The pinned set with scores, tolerating the id-only format.
    ///
    /// Sessions pinned before scores were stored hold a bare JSON array of
    /// strings. Those are still readable - they just come back with no score -
    /// so an existing conversation does not lose its skills on upgrade.
    async fn read_pinned_entries(&self, session_id: &str) -> Vec<PinnedEntry> {
        let raw = match self.persist.kv_get(session_id, PINNED_KEY).await {
            Ok(Some(raw)) => raw,
            Ok(None) => return Vec::new(),
            Err(e) => {
                tracing::warn!(error = %e, "could not read pinned skills");
                return Vec::new();
            }
        };

        if let Ok(entries) = serde_json::from_str::<Vec<PinnedEntry>>(&raw) {
            return entries;
        }
        // The older shape: ids only.
        match serde_json::from_str::<Vec<String>>(&raw) {
            Ok(ids) => ids
                .into_iter()
                .map(|id| PinnedEntry {
                    id,
                    score: 0.0,
                    how: "pinned".to_string(),
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "pinned skills are unreadable; ignoring them");
                Vec::new()
            }
        }
    }

    async fn write_pinned(&self, session_id: &str, ids: &[String]) -> Result<()> {
        let entries: Vec<PinnedEntry> = ids
            .iter()
            .map(|id| PinnedEntry {
                id: id.clone(),
                score: 0.0,
                how: "pinned".to_string(),
            })
            .collect();
        self.write_pinned_entries(session_id, &entries).await
    }

    /// Stores the pinned set with each card's score and ranking method.
    async fn write_pinned_entries(&self, session_id: &str, entries: &[PinnedEntry]) -> Result<()> {
        let raw = serde_json::to_string(entries)?;
        self.persist.kv_put(session_id, PINNED_KEY, &raw).await
    }

    /// The directory a skill's files live in, from its id.
    fn skill_dir(&self, id: &str) -> Result<PathBuf> {
        let mut dir = self.cfg.paths.skills.clone();
        for segment in id.split('/') {
            dir.push(segment);
        }
        // The id is already validated by `normalize_id`, but the confinement
        // check is what actually holds, so it runs regardless.
        self.confine_unchecked_parent(&dir)
    }

    fn resource_path(&self, skill: &Skill, resource: &str) -> Result<PathBuf> {
        if !skill.resources.iter().any(|r| r == resource) {
            return Err(anyhow!(
                "'{}' has no resource '{resource}'; it has: {}",
                skill.id,
                if skill.resources.is_empty() {
                    "none".to_string()
                } else {
                    skill.resources.join(", ")
                }
            ));
        }
        let base = skill
            .path
            .parent()
            .ok_or_else(|| anyhow!("skill '{}' has no directory", skill.id))?;
        let joined = safe_join(base, resource)?;
        self.confine(&joined)
    }

    /// Rejects a path that escapes the skills directory.
    ///
    /// Canonicalizes so symlinks and `..` cannot be used to reach outside, which
    /// means the path has to exist. Use it for reads and deletes.
    fn confine(&self, path: &Path) -> Result<PathBuf> {
        let root = self
            .cfg
            .paths
            .skills
            .canonicalize()
            .unwrap_or_else(|_| self.cfg.paths.skills.clone());
        let real = path
            .canonicalize()
            .with_context(|| format!("resolving {}", path.display()))?;
        if !real.starts_with(&root) {
            return Err(anyhow!(
                "{} is outside the skills directory",
                path.display()
            ));
        }
        Ok(real)
    }

    /// Confinement for a path that does not exist yet: check the nearest
    /// ancestor that does, since a path being created cannot be canonicalized.
    fn confine_unchecked_parent(&self, path: &Path) -> Result<PathBuf> {
        let root = self
            .cfg
            .paths
            .skills
            .canonicalize()
            .unwrap_or_else(|_| self.cfg.paths.skills.clone());

        let mut probe = path.to_path_buf();
        loop {
            if probe.exists() {
                let real = probe.canonicalize()?;
                if !real.starts_with(&root) {
                    return Err(anyhow!(
                        "{} is outside the skills directory",
                        path.display()
                    ));
                }
                return Ok(path.to_path_buf());
            }
            match probe.parent() {
                Some(p) if p != probe => probe = p.to_path_buf(),
                _ => {
                    return Err(anyhow!(
                        "{} has no existing ancestor to check",
                        path.display()
                    ));
                }
            }
        }
    }
}

/// Validates a skill id and returns it in canonical form.
///
/// Ids are path fragments, so this is the boundary that keeps a crafted id from
/// becoming a write outside the skills directory. Restrictive on purpose: the
/// set of legal ids is small and easy to state.
fn normalize_id(id: &str) -> Result<String> {
    let id = id.trim();
    // A leading slash is refused rather than trimmed. Trimming it would quietly
    // reinterpret "/etc/passwd" as the skill "etc/passwd", which is confined and
    // therefore safe, but silently means something other than what was asked.
    if id.starts_with('/') || id.starts_with('\\') {
        return Err(anyhow!("'{id}' must be a relative skill id, not a path"));
    }
    let id = id.trim_end_matches('/');
    if id.is_empty() {
        return Err(anyhow!("skill id is empty"));
    }

    let segments: Vec<&str> = id.split('/').collect();
    if segments.len() > skills::MAX_DEPTH {
        return Err(anyhow!(
            "'{id}' nests {} deep; the limit is {}",
            segments.len(),
            skills::MAX_DEPTH
        ));
    }

    for s in &segments {
        if s.is_empty() {
            return Err(anyhow!("'{id}' has an empty path segment"));
        }
        if *s == "." || *s == ".." {
            return Err(anyhow!("'{id}' may not contain '.' or '..'"));
        }
        if s.starts_with('.') {
            return Err(anyhow!("'{id}' may not have a segment starting with '.'"));
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(anyhow!(
                "'{id}' may only use letters, digits, '-' and '_' in each segment"
            ));
        }
        // A skill called `references` would occupy the same directory as its
        // parent's resources, so a fetch could not tell the two apart.
        if skills::RESOURCE_DIRS.contains(s) {
            return Err(anyhow!(
                "'{s}' is a reserved resource directory name, so it cannot be a skill id"
            ));
        }
    }

    Ok(segments.join("/"))
}

/// Joins a relative path onto a base, refusing anything that could escape.
fn safe_join(base: &Path, rel: &str) -> Result<PathBuf> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Err(anyhow!("empty path"));
    }
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(anyhow!("'{rel}' must be relative"));
    }
    for c in p.components() {
        use std::path::Component;
        match c {
            Component::Normal(part) => {
                let s = part.to_string_lossy();
                if s.starts_with('.') {
                    return Err(anyhow!("'{rel}' may not contain hidden segments"));
                }
            }
            Component::CurDir => return Err(anyhow!("'{rel}' may not contain '.'")),
            Component::ParentDir => return Err(anyhow!("'{rel}' may not contain '..'")),
            _ => return Err(anyhow!("'{rel}' must be a plain relative path")),
        }
    }
    Ok(base.join(p))
}

/// Resources live in one of three known subdirectories, one level deep. Keeping
/// that shape enforced on write is what lets discovery find them on read.
fn check_resource_shape(id: &str, resource: &str) -> Result<()> {
    let parts: Vec<&str> = resource.split('/').collect();
    if parts.len() != 2 {
        return Err(anyhow!(
            "resource '{resource}' for '{id}' must be <references|scripts|assets>/<file>"
        ));
    }
    const DIRS: [&str; 3] = ["references", "scripts", "assets"];
    if !DIRS.contains(&parts[0]) {
        return Err(anyhow!(
            "resource '{resource}' for '{id}' must start with one of: {}",
            DIRS.join(", ")
        ));
    }
    Ok(())
}

fn display_relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Unused today, but the ranker's labels are part of the guest-visible contract,
/// so keep the mapping in one place rather than stringifying at each call site.
pub fn how_label(how: How) -> &'static str {
    how.label()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    /// A manager over a scratch skills directory and a scratch database.
    fn fixture() -> (SkillManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        let mut cfg = Config::load().unwrap();
        cfg.paths.skills = skills_dir;
        // No key: every test here stays off the network, so retrieval takes the
        // lexical path and no test can be billed or made flaky by the provider.
        cfg.openrouter_api_key = None;

        let db = Arc::new(Store::open(&dir.path().join("t.redb")).unwrap());
        let mgr = SkillManager::new(Arc::new(cfg), crate::persist::Persist::Local(db)).unwrap();
        (mgr, dir)
    }

    fn skill_md(name: &str, brief: &str) -> String {
        format!(
            "---\nname = \"{name}\"\nbrief = \"{brief}\"\nwhen_to_use = \"When testing.\"\n---\n\n# {name}\n\nBody of {name}.\n"
        )
    }

    #[tokio::test]
    async fn a_zero_limit_means_the_configured_default() {
        // `search` used to return nothing for limit 0 while `retrieve` resolved
        // it, so the documented default held only when entering via `retrieve`.
        // A direct `search(q, 0)` - what the agent's skill_search tool sends
        // when the model omits the argument - came back empty.
        let (mgr, _d) = fixture();
        mgr.upsert("alpha", "", &skill_md("Alpha", "Alpha things."))
            .unwrap();
        mgr.upsert("beta", "", &skill_md("Beta", "Beta things."))
            .unwrap();

        // No API key in the fixture, so this is the lexical path; the point is
        // only that a zero limit does not zero the result.
        let hits = mgr.search("alpha", 0).await;
        assert!(
            !hits.is_empty(),
            "a zero limit should mean the configured default, got nothing"
        );
    }

    #[test]
    fn a_reserved_resource_name_cannot_be_a_skill() {
        // `skill-creator/references` would be both a nested skill and the
        // directory holding skill-creator's own reference files, so a fetch
        // could not say which was meant. upsert used to allow it.
        let (mgr, _d) = fixture();
        mgr.upsert("host", "", &skill_md("Host", "A parent skill."))
            .unwrap();

        for name in skills::RESOURCE_DIRS {
            let err = mgr
                .upsert(&format!("host/{name}"), "", &skill_md("X", "Nope."))
                .expect_err(&format!("'{name}' should be refused as a skill id"));
            assert!(
                err.to_string().contains("reserved"),
                "the error should say why: {err}"
            );
        }

        // The same name is still fine as an actual resource path.
        mgr.upsert("host", "references/notes.md", "some notes")
            .expect("a real resource file is still allowed");
    }

    #[tokio::test]
    async fn an_id_only_pin_list_still_reads() {
        // Sessions pinned before scores were stored hold a bare array of ids.
        // Those conversations must not lose their skills on upgrade.
        let (mgr, _d) = fixture();
        mgr.upsert("alpha", "", &skill_md("Alpha", "Alpha things."))
            .unwrap();

        // The old on-disk shape, written directly.
        mgr.persist
            .kv_put("s1", PINNED_KEY, r#"["alpha"]"#)
            .await
            .unwrap();

        let cards = mgr.pinned("s1").await;
        assert_eq!(cards.len(), 1, "the old shape should still resolve");
        assert_eq!(cards[0].id, "alpha");
        assert_eq!(cards[0].score, 0.0, "no score was stored back then");

        // And the new shape round-trips with its score intact.
        mgr.write_pinned_entries(
            "s2",
            &[PinnedEntry {
                id: "alpha".to_string(),
                score: 0.42,
                how: "dense".to_string(),
            }],
        )
        .await
        .unwrap();

        let cards = mgr.pinned("s2").await;
        assert_eq!(cards.len(), 1);
        assert!((cards[0].score - 0.42).abs() < 1e-6, "score survives");
        assert_eq!(cards[0].how, "dense", "so does how it was found");
    }

    #[tokio::test]
    async fn upsert_creates_then_replaces_and_reports_which() {
        let (mgr, _d) = fixture();

        let first = mgr
            .upsert("demo", "", &skill_md("Demo", "A demo skill."))
            .unwrap();
        assert!(first.created, "the first write creates");
        assert_eq!(first.id, "demo");
        assert_eq!(first.path, "demo/SKILL.md");
        assert!(
            first.diagnostics.is_empty(),
            "clean skill lints clean: {:?}",
            first.diagnostics
        );

        let second = mgr
            .upsert("demo", "", &skill_md("Demo", "Reworded brief."))
            .unwrap();
        assert!(!second.created, "the second write replaces");

        // The cache was invalidated, so the new brief is what is served.
        let cards = mgr.pin("s1", &["demo".to_string()]).await.unwrap();
        assert_eq!(cards[0].brief, "Reworded brief.");
    }

    #[test]
    fn upsert_reports_a_skill_that_fails_to_parse() {
        let (mgr, _d) = fixture();
        // Frontmatter that is not TOML: the file lands but nothing discovers it.
        let out = mgr
            .upsert("broken", "", "---\nthis: is: yaml\n---\nbody")
            .unwrap();
        assert!(
            out.diagnostics.iter().any(|d| d.severity == "error"),
            "a skill that does not parse must come back as an error: {:?}",
            out.diagnostics
        );
    }

    #[test]
    fn upsert_refuses_to_write_outside_the_skills_directory() {
        let (mgr, _d) = fixture();
        for bad in ["../escape", "/etc/nope", "a/../../b"] {
            assert!(
                mgr.upsert(bad, "", "x").is_err(),
                "{bad:?} should be refused"
            );
        }
    }

    #[tokio::test]
    async fn a_resource_round_trips_through_upsert_and_fetch() {
        let (mgr, _d) = fixture();
        mgr.upsert("demo", "", &skill_md("Demo", "A demo."))
            .unwrap();
        mgr.upsert("demo", "references/format.md", "# Format\n\nDetail.\n")
            .unwrap();

        let body = mgr.fetch("demo", "references/format.md", 0, 0).unwrap();
        assert!(body.content.contains("Detail."));
        assert_eq!(body.resource, "references/format.md");

        // Discovery found it, so it is advertised on the card.
        let card = mgr.pin("s1", &["demo".to_string()]).await.unwrap();
        assert_eq!(card[0].resources, vec!["references/format.md".to_string()]);
    }

    #[test]
    fn fetching_an_unknown_skill_or_resource_is_an_error() {
        let (mgr, _d) = fixture();
        mgr.upsert("demo", "", &skill_md("Demo", "A demo."))
            .unwrap();

        assert!(mgr.fetch("nope", "", 0, 0).is_err());
        // Refused even though the traversal would resolve, because the resource
        // is not one discovery listed.
        assert!(mgr.fetch("demo", "../../../etc/passwd", 0, 0).is_err());
        assert!(mgr.fetch("demo", "references/absent.md", 0, 0).is_err());
    }

    #[test]
    fn fetch_slices_a_long_body_and_says_how_much_is_left() {
        let (mgr, _d) = fixture();
        let long = "x".repeat(500);
        let text = format!("---\nname = \"L\"\nbrief = \"Long.\"\n---\n\n{long}");
        mgr.upsert("long", "", &text).unwrap();

        let head = mgr.fetch("long", "", 0, 100).unwrap();
        assert_eq!(head.content.chars().count(), 100);
        assert!(head.truncated);
        assert_eq!(head.offset, 0);

        let tail = mgr.fetch("long", "", head.offset + 100, 1000).unwrap();
        assert!(!tail.truncated, "the rest fits in one slice");
        assert_eq!(head.total, tail.total);

        // An offset past the end is empty rather than an error, so a loop that
        // reads until it runs out terminates instead of trapping.
        let past = mgr.fetch("long", "", 10_000, 100).unwrap();
        assert!(past.content.is_empty());
        assert!(!past.truncated);
    }

    #[test]
    fn fetch_never_splits_a_multibyte_character() {
        let (mgr, _d) = fixture();
        // Four-byte characters: slicing by byte here would produce invalid UTF-8.
        let body = "\u{1f600}".repeat(50);
        let text = format!("---\nname = \"E\"\nbrief = \"Emoji.\"\n---\n\n{body}");
        mgr.upsert("emoji", "", &text).unwrap();

        let slice = mgr.fetch("emoji", "", 0, 10).unwrap();
        assert_eq!(slice.content.chars().count(), 10);
        assert!(slice.content.chars().all(|c| c == '\u{1f600}'));
    }

    #[test]
    fn removing_a_parent_needs_the_recursive_flag() {
        let (mgr, _d) = fixture();
        mgr.upsert("parent", "", &skill_md("Parent", "A parent."))
            .unwrap();
        mgr.upsert("parent/child", "", &skill_md("Child", "A child."))
            .unwrap();

        let refused = mgr.remove("parent", false);
        assert!(refused.is_err(), "a parent with children needs the flag");
        assert!(
            refused.unwrap_err().to_string().contains("parent/child"),
            "the error should name what would be lost"
        );

        // The child alone goes without ceremony.
        mgr.remove("parent/child", false).unwrap();
        assert!(mgr.tree().get("parent/child").is_none());
        // And now the parent is a leaf.
        mgr.remove("parent", false).unwrap();
        assert!(mgr.tree().get("parent").is_none());
    }

    #[test]
    fn removing_recursively_takes_the_subtree() {
        let (mgr, _d) = fixture();
        mgr.upsert("parent", "", &skill_md("Parent", "A parent."))
            .unwrap();
        mgr.upsert("parent/child", "", &skill_md("Child", "A child."))
            .unwrap();

        mgr.remove("parent", true).unwrap();
        assert!(mgr.tree().get("parent").is_none());
        assert!(mgr.tree().get("parent/child").is_none());
    }

    #[test]
    fn remove_refuses_unknown_ids_and_the_root() {
        let (mgr, _d) = fixture();
        assert!(mgr.remove("absent", false).is_err());
        assert!(mgr.remove("..", true).is_err());
        assert!(mgr.remove("", true).is_err());
    }

    #[tokio::test]
    async fn pinning_is_per_session_and_survives_rediscovery() {
        let (mgr, _d) = fixture();
        mgr.upsert("a", "", &skill_md("A", "First.")).unwrap();
        mgr.upsert("b", "", &skill_md("B", "Second.")).unwrap();

        mgr.pin("s1", &["b".to_string(), "a".to_string()])
            .await
            .unwrap();
        let ids: Vec<String> = mgr.pinned("s1").await.into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["b", "a"], "pin order is preserved");

        // A different session is unaffected.
        assert!(mgr.pinned("s2").await.is_empty());

        // Invalidating the tree does not lose the pins.
        mgr.invalidate();
        assert_eq!(mgr.pinned("s1").await.len(), 2);
    }

    #[tokio::test]
    async fn pinning_refuses_unknown_ids_wholesale() {
        let (mgr, _d) = fixture();
        mgr.upsert("a", "", &skill_md("A", "First.")).unwrap();

        assert!(
            mgr.pin("s1", &["a".to_string(), "ghost".to_string()])
                .await
                .is_err()
        );
        // Nothing was stored: a partial pin would be worse than none.
        assert!(mgr.pinned("s1").await.is_empty());
    }

    #[tokio::test]
    async fn a_pin_to_a_deleted_skill_is_dropped_not_fatal() {
        let (mgr, _d) = fixture();
        mgr.upsert("a", "", &skill_md("A", "First.")).unwrap();
        mgr.upsert("b", "", &skill_md("B", "Second.")).unwrap();
        mgr.pin("s1", &["a".to_string(), "b".to_string()])
            .await
            .unwrap();

        mgr.remove("b", false).unwrap();

        let ids: Vec<String> = mgr.pinned("s1").await.into_iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            vec!["a"],
            "the dead pin is skipped, the live one stays"
        );
    }

    #[tokio::test]
    async fn universals_are_capped_however_many_are_marked() {
        let (mgr, dir) = fixture();
        for i in 0..4 {
            let text = format!(
                "---\nname = \"U{i}\"\nbrief = \"Universal {i}.\"\nuniversal = true\n---\n\nBody.\n"
            );
            mgr.upsert(&format!("u{i}"), "", &text).unwrap();
        }
        assert_eq!(mgr.universal().len(), 4);
        assert!(mgr.universal().iter().all(|c| c.how == "universal"));

        // Lower the cap and the excess is ignored rather than trusted.
        let mut cfg = Config::load().unwrap();
        cfg.paths.skills = dir.path().join("skills");
        cfg.openrouter_api_key = None;
        cfg.skills.max_universal = 2;
        let db = Arc::new(Store::open(&dir.path().join("t2.redb")).unwrap());
        let capped = SkillManager::new(Arc::new(cfg), crate::persist::Persist::Local(db)).unwrap();
        assert_eq!(capped.universal().len(), 2);
    }

    #[tokio::test]
    async fn search_falls_back_to_lexical_without_a_provider() {
        let (mgr, _d) = fixture();
        mgr.upsert(
            "rollback",
            "",
            &skill_md("Rolling back", "Undo a bad revision with rollback."),
        )
        .unwrap();
        mgr.upsert(
            "brevity",
            "",
            &skill_md("Being brief", "Answer in as few words as possible."),
        )
        .unwrap();
        mgr.upsert("third", "", &skill_md("Third", "Unrelated filler."))
            .unwrap();

        // Limit below the corpus size, or the ranker short-circuits.
        let hits = mgr.search("how do I undo a bad revision", 2).await;
        assert_eq!(hits[0].id, "rollback", "lexical overlap should win here");
        assert_eq!(hits[0].how, "lexical", "no key means no dense path");

        // Fewer than the limit on purpose: BM25 drops documents that share no
        // term with the query, so the lexical path returns real overlaps only
        // instead of padding the block with unrelated cards.
        assert!(
            hits.len() < 2,
            "only one card shares any term with this query, got {:?}",
            hits.iter().map(|h| &h.id).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn retrieve_pins_what_it_found() {
        let (mgr, _d) = fixture();
        mgr.upsert("a", "", &skill_md("Alpha", "Rollback a revision."))
            .unwrap();
        mgr.upsert("b", "", &skill_md("Beta", "Something else entirely."))
            .unwrap();
        mgr.upsert("c", "", &skill_md("Gamma", "More filler text."))
            .unwrap();

        let found = mgr.retrieve("s1", "rollback a revision", 2).await;
        assert!(!found.is_empty(), "the matching card should be found");

        let pinned: Vec<String> = mgr.pinned("s1").await.into_iter().map(|c| c.id).collect();
        let found_ids: Vec<String> = found.iter().map(|c| c.id.clone()).collect();
        assert_eq!(pinned, found_ids, "what was retrieved is what is pinned");
    }

    #[tokio::test]
    async fn an_empty_corpus_retrieves_nothing_without_erroring() {
        let (mgr, _d) = fixture();
        assert!(mgr.search("anything", 5).await.is_empty());
        assert!(mgr.retrieve("s1", "anything", 5).await.is_empty());
        assert!(mgr.universal().is_empty());
        assert!(mgr.lint("").is_empty());
    }

    #[tokio::test]
    async fn a_child_card_names_its_parent_and_a_parent_names_its_children() {
        let (mgr, _d) = fixture();
        mgr.upsert("p", "", &skill_md("P", "Parent.")).unwrap();
        mgr.upsert("p/c", "", &skill_md("C", "Child.")).unwrap();

        let cards = mgr
            .pin("s1", &["p".to_string(), "p/c".to_string()])
            .await
            .unwrap();
        assert_eq!(cards[0].children, vec!["p/c".to_string()]);
        assert_eq!(cards[0].parent, "");
        assert_eq!(cards[1].parent, "p");
        assert!(cards[1].children.is_empty());
    }

    #[test]
    fn all_lists_each_parent_immediately_before_its_children() {
        let (mgr, _d) = fixture();
        mgr.upsert("beta", "", &skill_md("Beta", "Second root."))
            .unwrap();
        mgr.upsert("alpha", "", &skill_md("Alpha", "First root."))
            .unwrap();
        mgr.upsert("alpha/two", "", &skill_md("Two", "Second child."))
            .unwrap();
        mgr.upsert("alpha/one", "", &skill_md("One", "First child."))
            .unwrap();

        let ids: Vec<String> = mgr.all().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["alpha", "alpha/one", "alpha/two", "beta"]);
        assert!(mgr.all().iter().all(|c| c.how == "tree"));
    }

    #[test]
    fn an_id_may_nest_but_not_escape() {
        assert_eq!(normalize_id("rollback").unwrap(), "rollback");
        assert_eq!(
            normalize_id("self-mod/rollback").unwrap(),
            "self-mod/rollback"
        );
        assert_eq!(normalize_id("trailing/").unwrap(), "trailing");
        // Absolute-looking ids are refused, not reinterpreted.
        assert!(normalize_id("/leading").is_err());

        for bad in [
            "",
            "..",
            "../etc",
            "a/../b",
            "./a",
            ".hidden",
            "a//b",
            "one/two/three/four",
            "has space",
            "semi;colon",
            "sub/../../escape",
        ] {
            assert!(normalize_id(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn joining_refuses_traversal_and_absolutes() {
        let base = Path::new("/tmp/skills/demo");
        assert_eq!(
            safe_join(base, "references/format.md").unwrap(),
            base.join("references/format.md")
        );
        for bad in [
            "../outside.md",
            "/etc/passwd",
            ".",
            "./x",
            ".git/config",
            "",
        ] {
            assert!(safe_join(base, bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn a_resource_must_sit_in_a_known_subdirectory() {
        assert!(check_resource_shape("d", "references/a.md").is_ok());
        assert!(check_resource_shape("d", "scripts/run.py").is_ok());
        assert!(check_resource_shape("d", "assets/t.md").is_ok());

        // Depth and directory name are both part of the shape.
        assert!(check_resource_shape("d", "a.md").is_err());
        assert!(check_resource_shape("d", "references/deep/a.md").is_err());
        assert!(check_resource_shape("d", "elsewhere/a.md").is_err());
    }

    #[test]
    fn relative_display_strips_the_root() {
        let root = Path::new("/tmp/skills");
        assert_eq!(
            display_relative(&root.join("demo/SKILL.md"), root),
            "demo/SKILL.md"
        );
        // A path outside the root is shown whole rather than mangled.
        assert_eq!(
            display_relative(Path::new("/other/x.md"), root),
            "/other/x.md"
        );
    }
}
