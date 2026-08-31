//! Skills: progressively-disclosed instruction sets that shape how the agent works.
//!
//! Object model:
//!
//! ```text
//! skills/
//!   concise.md                      # bare file = leaf skill (back-compat)
//!   self-modification/
//!     SKILL.md                      # parent: L0/L1 frontmatter + L2 dispatch table
//!     references/contract.md        # L3 resource
//!     rollback/
//!       SKILL.md                    # nested child
//! ```
//!
//! Frontmatter is TOML between `---` fences:
//!
//! ```toml
//! name = "Self-modification"
//! brief = "Change your own loop, gateways and tools safely."   # L0, <=200 chars
//! when_to_use = "Use whenever the user asks to edit Thetis."   # L1, <=1024 chars
//! universal = false                # true => always in the system prompt
//! tags = ["self-mod", "devkit"]
//! children = "auto"                # "auto" | "none" | ["explicit", "list"]
//! related = ["careful-surgery"]
//! version = 1
//! ```
//!
//! Progressive disclosure levels:
//!
//! - **L0 brief** (<=200 chars) - every `universal = true` skill, in the base prompt
//! - **L1 card** - the retrieved top-k: name, brief, when_to_use, tags, child index
//! - **L2 body** - the SKILL.md body, fetched by id
//! - **L3 resources** - sibling files under `references/`, `scripts/`, `assets/`
//!
//! Files are read on demand rather than cached, so editing a skill takes effect
//! on the next turn without a restart.

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Deepest nesting allowed: parent -> child -> grandchild.
pub const MAX_DEPTH: usize = 3;

/// Limits enforced by [`lint`].
pub const MAX_BRIEF_CHARS: usize = 200;
pub const MAX_WHEN_TO_USE_CHARS: usize = 1024;
pub const MAX_UNIVERSAL_COUNT: usize = 20;

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    /// Path-derived: `"rollback"` for a root, `"self-mod/rollback"` when nested.
    pub id: String,
    /// The SKILL.md (or bare `<id>.md`) this was read from.
    pub path: PathBuf,
    /// Parent skill id, empty for a root.
    pub parent: String,
    /// 0 for a root.
    pub depth: usize,

    pub name: String,
    pub brief: String,
    pub when_to_use: String,
    pub universal: bool,
    pub tags: Vec<String>,
    pub children: ChildSpec,
    /// Cross-references, not hierarchy.
    pub related: Vec<String>,
    /// Lifecycle: empty or `"active"` for a live skill, `"retired"` for one kept
    /// only so its id still resolves. A retired skill is still fetchable — the
    /// point is that anything linking to it gets told.
    pub status: String,
    /// The skill that replaced this one, when `status` is `"retired"`.
    pub superseded_by: String,
    pub version: u32,

    pub body: String,
    /// Sibling files, one level deep, as `"references/contract.md"`.
    pub resources: Vec<String>,
    /// Hash of the retrieval text, for keying the embedding cache.
    pub content_hash: String,
    /// True when this came from a `SKILL.md`, so the directory is its own and
    /// subdirectories are its children. A bare `<id>.md` sits in a directory it
    /// shares with unrelated skills, so it can never adopt anything.
    pub owns_dir: bool,
}

impl Skill {
    /// The text the retriever indexes. Body is deliberately excluded: a card is
    /// what gets injected, so the card is what should be matched against.
    pub fn index_text(&self) -> String {
        let mut t = format!("{}\n{}\n{}", self.name, self.brief, self.when_to_use);
        if !self.tags.is_empty() {
            t.push('\n');
            t.push_str(&self.tags.join(" "));
        }
        t
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChildSpec {
    /// Adopt every subdirectory holding a SKILL.md.
    Auto,
    /// Adopt only these subdirectory names, in this order.
    Explicit(Vec<String>),
    /// A leaf.
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub id: String,
    pub severity: Severity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }
}

/// Every skill found under one directory, plus which of them are roots.
#[derive(Debug, Clone, Default)]
pub struct SkillTree {
    pub skills: HashMap<String, Skill>,
    pub roots: Vec<String>,
}

impl SkillTree {
    pub fn get(&self, id: &str) -> Option<&Skill> {
        self.skills.get(id)
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Direct children of `parent_id`, ordered by the parent's `children` spec
    /// when it is explicit and by id otherwise.
    pub fn children(&self, parent_id: &str) -> Vec<&Skill> {
        let mut kids: Vec<&Skill> = self
            .skills
            .values()
            .filter(|s| s.parent == parent_id)
            .collect();

        match self.skills.get(parent_id).map(|p| &p.children) {
            Some(ChildSpec::Explicit(order)) => {
                let rank = |s: &Skill| {
                    let leaf = s.id.rsplit('/').next().unwrap_or(&s.id);
                    order.iter().position(|o| o == leaf).unwrap_or(usize::MAX)
                };
                kids.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.id.cmp(&b.id)));
            }
            _ => kids.sort_by(|a, b| a.id.cmp(&b.id)),
        }
        kids
    }

    /// Skills marked `universal`, ordered by id so the L0 block is byte-stable
    /// across turns and the prompt cache keeps its prefix.
    pub fn universal(&self) -> Vec<&Skill> {
        let mut u: Vec<&Skill> = self.skills.values().filter(|s| s.universal).collect();
        u.sort_by(|a, b| a.id.cmp(&b.id));
        u
    }

    /// All skills, ordered by id.
    pub fn all(&self) -> Vec<&Skill> {
        let mut all: Vec<&Skill> = self.skills.values().collect();
        all.sort_by(|a, b| a.id.cmp(&b.id));
        all
    }
}

/// Reads the whole skill tree under `dir`.
///
/// A malformed skill is skipped with a warning rather than failing the walk, so
/// one bad file cannot hide every other skill. Depth is capped at [`MAX_DEPTH`]
/// and a duplicate id keeps whichever copy was reached first.
pub fn discover(dir: &Path) -> Result<SkillTree> {
    let mut tree = SkillTree::default();
    if !dir.exists() {
        return Ok(tree);
    }

    // (file to read, parent id, depth). A queue rather than recursion so the
    // depth cap is enforced in exactly one place.
    let mut queue: Vec<(PathBuf, String, usize)> = Vec::new();

    let mut seeds: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .flatten()
    {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_file() && name.ends_with(".md") && name != "SKILL.md" {
            seeds.push(path);
        } else if path.is_dir() && path.join("SKILL.md").is_file() {
            seeds.push(path.join("SKILL.md"));
        }
    }
    // Sorted so discovery order, and therefore duplicate resolution, is stable.
    seeds.sort();
    for path in seeds {
        queue.push((path, String::new(), 0));
    }

    let mut cursor = 0;
    while cursor < queue.len() {
        let (path, parent, depth) = queue[cursor].clone();
        cursor += 1;

        if depth >= MAX_DEPTH {
            tracing::warn!(path = %path.display(), "skill nested deeper than {MAX_DEPTH}; skipped");
            continue;
        }

        let skill = match load(&path, &parent, depth) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skipping malformed skill");
                continue;
            }
        };

        if tree.skills.contains_key(&skill.id) {
            tracing::warn!(id = %skill.id, "duplicate skill id; keeping the first");
            continue;
        }

        // Only a directory-owning skill has children to queue: a bare `.md`
        // shares its directory with unrelated skills.
        if depth + 1 < MAX_DEPTH && skill.owns_dir {
            let dir = skill.path.parent().unwrap_or(Path::new(""));
            for child_dir in child_dirs(dir, &skill.children) {
                queue.push((child_dir.join("SKILL.md"), skill.id.clone(), depth + 1));
            }
        }

        if skill.parent.is_empty() {
            tree.roots.push(skill.id.clone());
        }
        tree.skills.insert(skill.id.clone(), skill);
    }

    tree.roots.sort();
    Ok(tree)
}

/// Subdirectories of `dir` that a parent with this `spec` adopts.
fn child_dirs(dir: &Path, spec: &ChildSpec) -> Vec<PathBuf> {
    match spec {
        ChildSpec::None => Vec::new(),
        ChildSpec::Explicit(names) => names
            .iter()
            .map(|n| dir.join(n))
            .filter(|p| p.join("SKILL.md").is_file())
            .collect(),
        ChildSpec::Auto => {
            let mut found: Vec<PathBuf> = match std::fs::read_dir(dir) {
                Ok(entries) => entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir() && p.join("SKILL.md").is_file())
                    .collect(),
                Err(_) => Vec::new(),
            };
            found.sort();
            found
        }
    }
}

fn load(path: &Path, parent: &str, depth: usize) -> Result<Skill> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let id = derive_id(path, parent)?;
    let owns_dir = path.file_name().and_then(|n| n.to_str()) == Some("SKILL.md");
    let (fm, body) = parse_frontmatter(&text)?;
    let resources = if owns_dir {
        discover_resources(path)
    } else {
        // A bare file has no directory of its own, so `references/` beside it
        // belongs to whichever skill does own that directory, not to this one.
        Vec::new()
    };

    let mut skill = Skill {
        id,
        path: path.to_path_buf(),
        parent: parent.to_string(),
        depth,
        // A skill with no `name` still needs a label; the id reads better than a blank.
        name: if fm.name.is_empty() {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("skill")
                .to_string()
        } else {
            fm.name
        },
        brief: fm.brief,
        when_to_use: fm.when_to_use,
        universal: fm.universal,
        tags: fm.tags,
        children: fm.children,
        related: fm.related,
        status: fm.status,
        superseded_by: fm.superseded_by,
        version: fm.version,
        body,
        resources,
        content_hash: String::new(),
        owns_dir,
    };
    skill.content_hash = hash(&skill.index_text());
    Ok(skill)
}

/// `skills/foo/SKILL.md` -> `foo`; `skills/a/b/SKILL.md` with parent `a` -> `a/b`;
/// `skills/concise.md` -> `concise`.
fn derive_id(path: &Path, parent: &str) -> Result<String> {
    let is_skill_md = path.file_name().and_then(|n| n.to_str()) == Some("SKILL.md");

    let leaf = if is_skill_md {
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("cannot derive an id from {}", path.display()))?
    } else {
        path.file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("cannot derive an id from {}", path.display()))?
    };

    if leaf.is_empty() {
        return Err(anyhow!("empty skill id from {}", path.display()));
    }

    Ok(if parent.is_empty() {
        leaf.to_string()
    } else {
        format!("{parent}/{leaf}")
    })
}

struct Frontmatter {
    name: String,
    brief: String,
    when_to_use: String,
    universal: bool,
    tags: Vec<String>,
    children: ChildSpec,
    related: Vec<String>,
    status: String,
    superseded_by: String,
    version: u32,
}

impl Default for Frontmatter {
    fn default() -> Self {
        Self {
            name: String::new(),
            brief: String::new(),
            when_to_use: String::new(),
            universal: false,
            tags: Vec::new(),
            // Absent `children` means "adopt whatever subdirectories exist",
            // which is what a directory layout already implies.
            children: ChildSpec::Auto,
            related: Vec::new(),
            status: String::new(),
            superseded_by: String::new(),
            version: 1,
        }
    }
}

/// Splits `---`-fenced TOML frontmatter from the body.
///
/// A file with no frontmatter is not an error: it becomes a skill whose metadata
/// is empty, which `lint` then complains about by id.
fn parse_frontmatter(source: &str) -> Result<(Frontmatter, String)> {
    let text = source.replace("\r\n", "\n");
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text).to_string();

    let (toml_text, body) = match text.strip_prefix("---\n") {
        Some(rest) => match rest.split_once("\n---") {
            Some((toml, after)) => {
                let body = after.strip_prefix('\n').unwrap_or(after);
                (toml.to_string(), body.trim().to_string())
            }
            // An opening fence with no close is a truncated file, not a body.
            None => return Err(anyhow!("frontmatter is missing its closing `---`")),
        },
        None => (String::new(), text.trim().to_string()),
    };

    if toml_text.trim().is_empty() {
        return Ok((Frontmatter::default(), body));
    }

    let table: toml::Table = toml::from_str(&toml_text).context("frontmatter is not valid TOML")?;

    let strings = |key: &str| -> Vec<String> {
        table
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    };
    let string = |key: &str| -> String {
        table
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string()
    };

    let children = match table.get("children") {
        None => ChildSpec::Auto,
        Some(v) => match v.as_str() {
            Some("auto") => ChildSpec::Auto,
            Some("none") => ChildSpec::None,
            Some(other) => {
                return Err(anyhow!(
                    "children: expected \"auto\", \"none\" or a list, got \"{other}\""
                ))
            }
            None if v.is_array() => ChildSpec::Explicit(strings("children")),
            None => return Err(anyhow!("children: expected a string or a list")),
        },
    };

    Ok((
        Frontmatter {
            name: string("name"),
            brief: string("brief"),
            when_to_use: string("when_to_use"),
            universal: table
                .get("universal")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            tags: strings("tags"),
            children,
            related: strings("related"),
            status: string("status").to_ascii_lowercase(),
            superseded_by: string("superseded_by"),
            version: table
                .get("version")
                .and_then(|v| v.as_integer())
                .filter(|n| *n > 0)
                .unwrap_or(1) as u32,
        },
        body,
    ))
}

/// The directory names a skill's L3 resources live in.
///
/// These are reserved: a nested skill may not take one of these names, or
/// `skill-creator/references` would be both a skill and the directory holding
/// `skill-creator`'s own references, and a fetch could not say which was meant.
pub const RESOURCE_DIRS: [&str; 3] = ["references", "scripts", "assets"];

/// Files sitting beside a skill under `references/`, `scripts/` or `assets/`.
/// One level only: a skill's resources should be listable in a single glance.
fn discover_resources(skill_path: &Path) -> Vec<String> {
    let Some(dir) = skill_path.parent() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for sub in RESOURCE_DIRS {
        let Ok(entries) = std::fs::read_dir(dir.join(sub)) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.path().is_file() {
                if let Some(name) = entry.file_name().to_str() {
                    out.push(format!("{sub}/{name}"));
                }
            }
        }
    }
    out.sort();
    out
}

fn hash(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Checks one skill and reports what is wrong with it.
///
/// Errors are things that make a skill unusable or unroutable; warnings are
/// things that will quietly degrade retrieval.
pub fn lint(tree: &SkillTree, id: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut push = |severity: Severity, message: String| {
        out.push(Diagnostic {
            id: id.to_string(),
            severity,
            message,
        })
    };

    let Some(skill) = tree.get(id) else {
        push(Severity::Error, format!("no skill with id `{id}`"));
        return out;
    };

    if skill.name.trim().is_empty() {
        push(Severity::Error, "name is empty".into());
    }

    let brief_len = skill.brief.chars().count();
    if brief_len == 0 {
        push(
            Severity::Error,
            "brief is empty; without it the skill can never be retrieved".into(),
        );
    } else if brief_len > MAX_BRIEF_CHARS {
        push(
            Severity::Error,
            format!("brief is {brief_len} chars, over the {MAX_BRIEF_CHARS} limit"),
        );
    }

    let when_len = skill.when_to_use.chars().count();
    if when_len == 0 {
        push(
            Severity::Warning,
            "when_to_use is empty; retrieval has only the brief to match on".into(),
        );
    } else if when_len > MAX_WHEN_TO_USE_CHARS {
        push(
            Severity::Warning,
            format!("when_to_use is {when_len} chars, over the {MAX_WHEN_TO_USE_CHARS} guideline"),
        );
    }

    if skill.body.trim().is_empty() && tree.children(id).is_empty() {
        push(
            Severity::Warning,
            "no body and no children: nothing to disclose past the card".into(),
        );
    }

    if skill.universal {
        let n = tree.universal().len();
        if n > MAX_UNIVERSAL_COUNT {
            push(
                Severity::Error,
                format!("{n} skills are universal, over the hard limit of {MAX_UNIVERSAL_COUNT}"),
            );
        }
    }

    for rel in &skill.related {
        if !tree.skills.contains_key(rel) {
            push(
                Severity::Warning,
                format!("related skill `{rel}` does not exist"),
            );
        }
    }

    if let ChildSpec::Explicit(names) = &skill.children {
        let present: Vec<String> = tree
            .children(id)
            .iter()
            .map(|c| c.id.rsplit('/').next().unwrap_or(&c.id).to_string())
            .collect();
        for want in names {
            if !present.contains(want) {
                push(
                    Severity::Warning,
                    format!("children lists `{want}`, but no such subdirectory has a SKILL.md"),
                );
            }
        }
    }

    if !skill.owns_dir {
        if let ChildSpec::Explicit(_) = &skill.children {
            push(
                Severity::Warning,
                "children is set, but a bare `.md` file cannot have children; \
                 move it to `<id>/SKILL.md` first"
                    .into(),
            );
        }
    }

    if skill.depth + 1 >= MAX_DEPTH && !tree.children(id).is_empty() {
        push(
            Severity::Warning,
            format!("at depth {}, children below this are not loaded", skill.depth),
        );
    }

    if !skill.status.is_empty() && !["active", "retired"].contains(&skill.status.as_str()) {
        push(
            Severity::Error,
            format!(
                "status is `{}`; expected \"active\" or \"retired\"",
                skill.status
            ),
        );
    }

    drop(push);
    // Card checks above, body and link checks below. Kept in a separate module
    // because they are the half that needs the whole tree to resolve references.
    out.extend(crate::skill_lint::body_diagnostics(tree, skill));

    out
}

/// Lints every skill in the tree, plus tree-wide checks.
pub fn lint_all(tree: &SkillTree) -> Vec<Diagnostic> {
    let mut out: Vec<Diagnostic> = tree.all().iter().flat_map(|s| lint(tree, &s.id)).collect();

    let universal = tree.universal();
    if universal.len() > MAX_UNIVERSAL_COUNT {
        out.push(Diagnostic {
            id: String::new(),
            severity: Severity::Error,
            message: format!(
                "{} universal skills, over the hard limit of {}: {}",
                universal.len(),
                MAX_UNIVERSAL_COUNT,
                universal
                    .iter()
                    .map(|s| s.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    out
}

/// The L0 block: one line per universal skill, always in the system prompt.
pub fn l0_block(skills: &[&Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Always-available skills\n");
    out.push_str("Fetch one by id with `skill_fetch` when it applies.\n\n");
    for s in skills {
        out.push_str(&format!("- `{}` — {}\n", s.id, s.brief));
    }
    out
}

/// The L1 card: what a retrieved skill contributes to the prompt.
pub fn l1_card(skill: &Skill, children: &[&Skill]) -> String {
    let mut card = format!("### `{}` — {}\n{}\n", skill.id, skill.name, skill.brief);

    // A retired skill still ranks — it describes a system that existed, and a
    // reader who arrives from old code needs it. Say so on the card, though, so
    // the body is read as history rather than as instruction.
    if skill.status == "retired" {
        card.push_str("\n**Retired.**");
        if skill.superseded_by.is_empty() {
            card.push_str(" Describes a system no longer in use.\n");
        } else {
            card.push_str(&format!(
                " Superseded by `{}`; prefer that unless you need the history.\n",
                skill.superseded_by
            ));
        }
    }
    if !skill.when_to_use.is_empty() {
        card.push_str(&format!("\n**When to use:** {}\n", skill.when_to_use));
    }
    if !skill.tags.is_empty() {
        card.push_str(&format!("**Tags:** {}\n", skill.tags.join(", ")));
    }
    if !children.is_empty() {
        card.push_str("\n**Nested skills** (fetch by id):\n");
        for c in children {
            card.push_str(&format!("- `{}` — {}\n", c.id, c.brief));
        }
    }
    // `related` is a machine-readable sibling pointer. It was parsed and
    // lint-checked long before anything rendered it, which is why bodies grew
    // hand-written "See also" prose instead. Render it, and the prose is
    // redundant.
    if !skill.related.is_empty() {
        card.push_str(&format!(
            "**Related:** {}\n",
            skill
                .related
                .iter()
                .map(|r| format!("`{r}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !skill.resources.is_empty() {
        card.push_str(&format!(
            "\n**Resources:** {}\n",
            skill
                .resources
                .iter()
                .map(|r| format!("`{r}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    card
}

/// The L1 section: every retrieved card under one heading.
pub fn l1_block(cards: &[String]) -> String {
    if cards.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Skills relevant to this conversation\n");
    out.push_str(
        "Selected for the opening message. Read the body with `skill_fetch` before relying on one.\n\n",
    );
    out.push_str(&cards.join("\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a skill directory from `(relative path, contents)` pairs.
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

    #[test]
    fn reads_every_frontmatter_field() {
        let (_d, tree) = tree_from(&[(
            "example/SKILL.md",
            r#"---
name = "Example"
brief = "A brief."
when_to_use = "When testing."
universal = true
tags = ["a", "b"]
children = "none"
related = ["other"]
version = 3
---

The body.
"#,
        )]);

        let s = tree.get("example").unwrap();
        assert_eq!(s.name, "Example");
        assert_eq!(s.brief, "A brief.");
        assert_eq!(s.when_to_use, "When testing.");
        assert!(s.universal);
        assert_eq!(s.tags, ["a", "b"]);
        assert_eq!(s.children, ChildSpec::None);
        assert_eq!(s.related, ["other"]);
        assert_eq!(s.version, 3);
        assert_eq!(s.body, "The body.");
    }

    #[test]
    fn a_file_without_frontmatter_is_still_a_skill() {
        let (_d, tree) = tree_from(&[("plain.md", "Just instructions.")]);
        let s = tree.get("plain").unwrap();
        assert_eq!(s.body, "Just instructions.");
        assert_eq!(s.name, "plain");
        assert_eq!(s.version, 1);
    }

    #[test]
    fn an_unclosed_fence_is_rejected_rather_than_read_as_body() {
        assert!(parse_frontmatter("---\nname = \"x\"\nno close here\n").is_err());
    }

    #[test]
    fn a_malformed_skill_does_not_hide_its_siblings() {
        let (_d, tree) = tree_from(&[
            (
                "good.md",
                "---\nname = \"Good\"\nbrief = \"Fine.\"\n---\nBody.",
            ),
            ("bad.md", "---\nname = = broken\n---\nBody."),
        ]);
        assert!(tree.get("good").is_some());
        assert!(tree.get("bad").is_none());
    }

    #[test]
    fn ids_come_from_the_path() {
        let (_d, tree) = tree_from(&[
            ("bare.md", "---\nbrief = \"b\"\n---\nx"),
            ("parent/SKILL.md", "---\nbrief = \"b\"\n---\nx"),
            ("parent/child/SKILL.md", "---\nbrief = \"b\"\n---\nx"),
        ]);
        assert!(tree.get("bare").is_some());
        assert!(tree.get("parent").is_some());
        let child = tree.get("parent/child").unwrap();
        assert_eq!(child.parent, "parent");
        assert_eq!(child.depth, 1);
        assert_eq!(tree.roots, ["bare", "parent"]);
    }

    #[test]
    fn a_bare_file_does_not_adopt_the_directories_beside_it() {
        // Regression: a bare `foo.md` sits in the skills root, so treating its
        // parent directory as its own made it adopt every top-level skill
        // directory as a child, mangling their ids.
        let (_d, tree) = tree_from(&[
            ("loose.md", "---\nbrief = \"A bare file.\"\n---\nx"),
            (
                "owned/SKILL.md",
                "---\nbrief = \"Owns its directory.\"\n---\nx",
            ),
        ]);

        assert!(tree.get("loose").is_some());
        assert!(tree.get("owned").is_some());
        assert_eq!(tree.get("owned").unwrap().parent, "");
        assert!(tree.get("loose/owned").is_none());
        assert!(tree.children("loose").is_empty());
        assert_eq!(tree.roots, ["loose", "owned"]);
    }

    #[test]
    fn a_bare_file_claims_no_resources() {
        // `references/` in the skills root belongs to whichever skill owns that
        // directory, which a bare file never does.
        let (_d, tree) = tree_from(&[
            ("loose.md", "---\nbrief = \"b\"\n---\nx"),
            ("references/shared.md", "not this skill's"),
        ]);
        assert!(tree.get("loose").unwrap().resources.is_empty());
    }

    #[test]
    fn nesting_stops_at_the_depth_cap() {
        let (_d, tree) = tree_from(&[
            ("a/SKILL.md", "---\nbrief = \"b\"\n---\nx"),
            ("a/b/SKILL.md", "---\nbrief = \"b\"\n---\nx"),
            ("a/b/c/SKILL.md", "---\nbrief = \"b\"\n---\nx"),
            ("a/b/c/d/SKILL.md", "---\nbrief = \"b\"\n---\nx"),
        ]);
        assert!(tree.get("a/b/c").is_some(), "depth 2 should load");
        assert!(tree.get("a/b/c/d").is_none(), "depth 3 is past the cap");
    }

    #[test]
    fn children_none_makes_a_leaf_of_a_populated_directory() {
        let (_d, tree) = tree_from(&[
            (
                "p/SKILL.md",
                "---\nbrief = \"b\"\nchildren = \"none\"\n---\nx",
            ),
            ("p/kid/SKILL.md", "---\nbrief = \"b\"\n---\nx"),
        ]);
        assert!(tree.get("p").is_some());
        assert!(tree.get("p/kid").is_none());
    }

    #[test]
    fn explicit_children_set_both_membership_and_order() {
        let (_d, tree) = tree_from(&[
            (
                "p/SKILL.md",
                "---\nbrief = \"b\"\nchildren = [\"second\", \"first\"]\n---\nx",
            ),
            ("p/first/SKILL.md", "---\nbrief = \"b\"\n---\nx"),
            ("p/second/SKILL.md", "---\nbrief = \"b\"\n---\nx"),
            ("p/ignored/SKILL.md", "---\nbrief = \"b\"\n---\nx"),
        ]);
        let ids: Vec<&str> = tree.children("p").iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["p/second", "p/first"]);
    }

    #[test]
    fn resources_are_found_one_level_down() {
        let (_d, tree) = tree_from(&[
            ("s/SKILL.md", "---\nbrief = \"b\"\n---\nx"),
            ("s/references/contract.md", "notes"),
            ("s/scripts/run.sh", "#!/bin/sh"),
            ("s/references/deep/buried.md", "not listed"),
        ]);
        let s = tree.get("s").unwrap();
        assert_eq!(s.resources, ["references/contract.md", "scripts/run.sh"]);
    }

    #[test]
    fn universal_skills_come_back_in_a_stable_order() {
        let (_d, tree) = tree_from(&[
            ("z.md", "---\nbrief = \"b\"\nuniversal = true\n---\nx"),
            ("a.md", "---\nbrief = \"b\"\nuniversal = true\n---\nx"),
            ("m.md", "---\nbrief = \"b\"\n---\nx"),
        ]);
        let ids: Vec<&str> = tree.universal().iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["a", "z"]);
    }

    #[test]
    fn the_index_text_covers_the_card_but_not_the_body() {
        let (_d, tree) = tree_from(&[(
            "s.md",
            "---\nname = \"N\"\nbrief = \"B\"\nwhen_to_use = \"W\"\ntags = [\"T\"]\n---\nBODY",
        )]);
        let text = tree.get("s").unwrap().index_text();
        for part in ["N", "B", "W", "T"] {
            assert!(text.contains(part), "index text should contain {part}");
        }
        assert!(!text.contains("BODY"));
    }

    #[test]
    fn the_content_hash_tracks_the_card_only() {
        let (_d, a) = tree_from(&[("s.md", "---\nbrief = \"same\"\n---\nbody one")]);
        let (_d2, b) = tree_from(&[("s.md", "---\nbrief = \"same\"\n---\nbody two")]);
        let (_d3, c) = tree_from(&[("s.md", "---\nbrief = \"other\"\n---\nbody one")]);

        let h = |t: &SkillTree| t.get("s").unwrap().content_hash.clone();
        assert_eq!(h(&a), h(&b), "body changes must not invalidate embeddings");
        assert_ne!(h(&a), h(&c), "brief changes must invalidate embeddings");
    }

    #[test]
    fn lint_reports_an_oversized_brief() {
        let brief = "x".repeat(MAX_BRIEF_CHARS + 1);
        let (_d, tree) = tree_from(&[(
            "s.md",
            &format!("---\nname = \"N\"\nbrief = \"{brief}\"\n---\nBody."),
        )]);
        let diags = lint(&tree, "s");
        assert!(diags
            .iter()
            .any(|d| d.severity == Severity::Error && d.message.contains("over the 200 limit")));
    }

    #[test]
    fn lint_reports_a_missing_brief_as_an_error() {
        let (_d, tree) = tree_from(&[("s.md", "---\nname = \"N\"\n---\nBody.")]);
        let diags = lint(&tree, "s");
        assert!(diags
            .iter()
            .any(|d| d.severity == Severity::Error && d.message.contains("brief is empty")));
    }

    #[test]
    fn lint_reports_a_dangling_related_id() {
        let (_d, tree) = tree_from(&[(
            "s.md",
            "---\nname = \"N\"\nbrief = \"b\"\nrelated = [\"ghost\"]\n---\nBody.",
        )]);
        assert!(lint(&tree, "s")
            .iter()
            .any(|d| d.message.contains("`ghost` does not exist")));
    }

    #[test]
    fn lint_reports_an_empty_leaf() {
        let (_d, tree) = tree_from(&[("s.md", "---\nname = \"N\"\nbrief = \"b\"\n---\n")]);
        assert!(lint(&tree, "s")
            .iter()
            .any(|d| d.message.contains("nothing to disclose")));
    }

    #[test]
    fn lint_all_reports_too_many_universal_skills() {
        let files: Vec<(String, String)> = (0..=MAX_UNIVERSAL_COUNT)
            .map(|i| {
                (
                    format!("s{i:02}.md"),
                    format!("---\nname = \"S{i}\"\nbrief = \"b\"\nuniversal = true\n---\nBody."),
                )
            })
            .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (_d, tree) = tree_from(&refs);

        assert!(lint_all(&tree)
            .iter()
            .any(|d| d.id.is_empty() && d.message.contains("over the hard limit")));
    }

    #[test]
    fn lint_names_a_skill_that_does_not_exist() {
        let tree = SkillTree::default();
        let diags = lint(&tree, "ghost");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Error);
    }

    #[test]
    fn the_l0_block_lists_ids_and_briefs() {
        let (_d, tree) = tree_from(&[(
            "s.md",
            "---\nname = \"N\"\nbrief = \"Do the thing.\"\nuniversal = true\n---\nx",
        )]);
        let block = l0_block(&tree.universal());
        assert!(block.contains("Always-available skills"));
        assert!(block.contains("`s` — Do the thing."));
    }

    #[test]
    fn an_l1_card_indexes_children_and_resources() {
        let (_d, tree) = tree_from(&[
            (
                "p/SKILL.md",
                "---\nname = \"P\"\nbrief = \"Parent.\"\nwhen_to_use = \"When routing.\"\ntags = [\"meta\"]\n---\nx",
            ),
            ("p/references/notes.md", "notes"),
            ("p/kid/SKILL.md", "---\nname = \"K\"\nbrief = \"Child.\"\n---\nx"),
        ]);

        let card = l1_card(tree.get("p").unwrap(), &tree.children("p"));
        assert!(card.contains("### `p` — P"));
        assert!(card.contains("**When to use:** When routing."));
        assert!(card.contains("**Tags:** meta"));
        assert!(card.contains("- `p/kid` — Child."));
        assert!(card.contains("`references/notes.md`"));
    }

    #[test]
    fn empty_blocks_render_as_nothing() {
        assert!(l0_block(&[]).is_empty());
        assert!(l1_block(&[]).is_empty());
    }

    #[test]
    fn a_missing_skills_directory_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let tree = discover(&dir.path().join("absent")).unwrap();
        assert!(tree.is_empty());
    }
}
