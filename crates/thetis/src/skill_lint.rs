//! Style and link checks over a skill's body.
//!
//! [`crate::skills::lint`] checks the *card*: is there a brief, does `related`
//! resolve, is the nesting legal. This module checks the *body*, which is where
//! a corpus actually rots:
//!
//! - **Links between skills are data, not prose.** A reference is written
//!   `[action](skill:torchship/world-simulation/action)`. That resolves against
//!   the tree, so a renamed or deleted skill produces a diagnostic instead of a
//!   dangling English sentence. A bare `[[action]]`-style name is not accepted,
//!   because names like `action`, `checks`, `effect` and `area` recur across
//!   topics and a reader cannot tell which was meant.
//! - **A parent must name its children.** An umbrella that omits a leaf hides it
//!   from every reader who does not already know it exists.
//! - **A retired skill must not be linked as though it were live.** This is the
//!   one failure that actively misleads rather than merely omitting.
//!
//! Severity follows one rule: **structural problems are errors, taste is a
//! warning.** An error means a reader can be sent somewhere that does not exist
//! or cannot be found; a warning means the skill works but reads badly.

use crate::skills::{Diagnostic, Severity, Skill, SkillTree};

/// Briefs longer than this still pass the hard cap but stop being one line, and
/// truncate in tool output where the brief is the only thing shown.
pub const BRIEF_SOFT_CHARS: usize = 160;
/// `when_to_use` past this has stopped being a trigger and become a summary.
pub const WHEN_TO_USE_SOFT_CHARS: usize = 400;

/// The prefix that marks a markdown link target as a skill reference.
pub const LINK_SCHEME: &str = "skill:";

/// How a link target resolved against the tree.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// Exactly one skill, by full id or an unambiguous leaf name.
    Found(String),
    /// Nothing in the tree matches.
    Unknown,
    /// A bare leaf name shared by several skills, so the reference is unreadable.
    Ambiguous(Vec<String>),
}

/// Resolves a link target, accepting a full id and diagnosing a bare name.
///
/// A full id is looked up directly. A bare name is matched against every
/// skill's last path segment: unique is usable but worth flagging, shared is an
/// error, because the whole point of the scheme is that a reader can follow the
/// reference without knowing the topic already.
pub fn resolve(tree: &SkillTree, target: &str) -> Resolution {
    let target = target.trim();
    if tree.get(target).is_some() {
        return Resolution::Found(target.to_string());
    }
    if target.contains('/') {
        // Spelled as a path, so it was meant as one; no leaf fallback.
        return Resolution::Unknown;
    }

    let mut hits: Vec<String> = tree
        .all()
        .into_iter()
        .filter(|s| s.id.rsplit('/').next().unwrap_or(&s.id) == target)
        .map(|s| s.id.clone())
        .collect();
    hits.sort();

    match hits.len() {
        0 => Resolution::Unknown,
        1 => Resolution::Found(hits.remove(0)),
        _ => Resolution::Ambiguous(hits),
    }
}

/// One `[text](skill:id)` reference found in a body.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub target: String,
    /// 1-based line in the body, so a diagnostic can point at it.
    pub line: usize,
}

/// Every skill link in `body`, ignoring anything inside code.
///
/// Code is stripped first: a fenced block showing this scheme as an example, or
/// an inline span quoting a link, is documentation about links rather than a
/// link, and flagging it would make the rule impossible to write about.
pub fn links(body: &str) -> Vec<Link> {
    let mut out = Vec::new();
    for (idx, (line, fenced)) in body.lines().zip(fenced_lines(body)).enumerate() {
        if fenced {
            continue;
        }
        for target in line_links(line) {
            out.push(Link {
                target,
                line: idx + 1,
            });
        }
    }
    out
}

/// Lines that are inside a fenced code block, by 0-based index.
fn fenced_lines(body: &str) -> Vec<bool> {
    let mut inside = false;
    body.lines()
        .map(|line| {
            if is_fence(line) {
                inside = !inside;
                // The fence line itself is code, not prose.
                true
            } else {
                inside
            }
        })
        .collect()
}

fn is_fence(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

/// Skill link targets in one line of prose, skipping inline code spans.
fn line_links(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in outside_code_spans(line) {
        let mut rest = chunk.as_str();
        while let Some(open) = rest.find("](") {
            let after = &rest[open + 2..];
            match after.find(')') {
                Some(close) => {
                    let target = &after[..close];
                    if let Some(id) = target.strip_prefix(LINK_SCHEME) {
                        out.push(id.trim().to_string());
                    }
                    rest = &after[close + 1..];
                }
                None => break,
            }
        }
    }
    out
}

/// The parts of a line that are not inside backticks.
fn outside_code_spans(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_span = false;
    for ch in line.chars() {
        if ch == '`' {
            if !in_span {
                out.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
            in_span = !in_span;
            continue;
        }
        if !in_span {
            current.push(ch);
        }
    }
    if !in_span {
        out.push(current);
    }
    out
}

/// All body and style checks for one skill.
pub fn body_diagnostics(tree: &SkillTree, skill: &Skill) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut push = |severity: Severity, message: String| {
        out.push(Diagnostic {
            id: skill.id.clone(),
            severity,
            message,
        })
    };

    check_heading(skill, &mut push);
    check_fences(skill, &mut push);
    check_links(tree, skill, &mut push);
    check_children_named(tree, skill, &mut push);
    check_retirement(tree, skill, &mut push);
    check_shouty_notes(skill, &mut push);
    check_card_length(skill, &mut push);
    check_legacy_see_also(skill, &mut push);

    out
}

/// The body opens with a single `# Title` line.
///
/// Structural, not cosmetic: the body is pasted into a conversation under no
/// heading of its own, so without an H1 the reader cannot see where one skill
/// ends and the surrounding text begins.
fn check_heading(skill: &Skill, push: &mut impl FnMut(Severity, String)) {
    let body = skill.body.trim();
    if body.is_empty() {
        return;
    }

    let first = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    if !first.trim_start().starts_with("# ") {
        let shown: String = first.chars().take(48).collect();
        push(
            Severity::Error,
            format!("body should open with a `# Title` heading, not `{shown}`"),
        );
    }

    let h1s = body
        .lines()
        .zip(fenced_lines(body))
        .filter(|(l, fenced)| !fenced && l.trim_start().starts_with("# "))
        .count();
    if h1s > 1 {
        push(
            Severity::Warning,
            format!("{h1s} `#` headings; a skill body should have exactly one, with `##` below it"),
        );
    }
}

/// An unclosed fence swallows the rest of the body when rendered.
fn check_fences(skill: &Skill, push: &mut impl FnMut(Severity, String)) {
    let fences = skill.body.lines().filter(|l| is_fence(l)).count();
    if fences % 2 != 0 {
        push(
            Severity::Error,
            "a code fence is never closed; everything after it renders as code".into(),
        );
    }
}

fn check_links(tree: &SkillTree, skill: &Skill, push: &mut impl FnMut(Severity, String)) {
    for link in links(&skill.body) {
        match resolve(tree, &link.target) {
            Resolution::Found(id) if id == link.target => {}
            Resolution::Found(id) => push(
                Severity::Warning,
                format!(
                    "line {}: `{}` resolves to `{id}`, but write the full id so the \
                     reference is readable without knowing the topic",
                    link.line, link.target
                ),
            ),
            Resolution::Unknown => push(
                Severity::Error,
                format!(
                    "line {}: link to `{}` matches no skill",
                    link.line, link.target
                ),
            ),
            Resolution::Ambiguous(hits) => push(
                Severity::Error,
                format!(
                    "line {}: `{}` is ambiguous across {}; use the full id",
                    link.line,
                    link.target,
                    hits.join(", ")
                ),
            ),
        }
    }
}

/// A parent links to every child it has.
///
/// The generated child index on the card lists them, but the card is a hundred
/// characters per child and the body is where a reader decides which to open. A
/// leaf missing from the body is a leaf nobody fetches.
fn check_children_named(tree: &SkillTree, skill: &Skill, push: &mut impl FnMut(Severity, String)) {
    let kids = tree.children(&skill.id);
    if kids.is_empty() || skill.body.trim().is_empty() {
        return;
    }

    let linked: Vec<String> = links(&skill.body)
        .into_iter()
        .filter_map(|l| match resolve(tree, &l.target) {
            Resolution::Found(id) => Some(id),
            _ => None,
        })
        .collect();

    let missing: Vec<&str> = kids
        .iter()
        .filter(|k| !linked.iter().any(|l| l == &k.id))
        .map(|k| k.id.as_str())
        .collect();

    if !missing.is_empty() {
        push(
            Severity::Error,
            format!(
                "body does not link {} of {} nested skills: {}",
                missing.len(),
                kids.len(),
                missing.join(", ")
            ),
        );
    }
}

/// A retired skill says what replaced it, and nobody links to it as if it were
/// live.
fn check_retirement(tree: &SkillTree, skill: &Skill, push: &mut impl FnMut(Severity, String)) {
    if skill.status == "retired" {
        if skill.superseded_by.is_empty() {
            push(
                Severity::Warning,
                "status is retired but superseded_by is empty; say where a reader should go".into(),
            );
        }
    }

    if !skill.superseded_by.is_empty() {
        match resolve(tree, &skill.superseded_by) {
            Resolution::Found(id) if id == skill.superseded_by => {}
            Resolution::Found(id) => push(
                Severity::Warning,
                format!(
                    "superseded_by `{}` resolves to `{id}`; use the full id",
                    skill.superseded_by
                ),
            ),
            _ => push(
                Severity::Error,
                format!("superseded_by `{}` matches no skill", skill.superseded_by),
            ),
        }
    }

    // Linking to something retired is how a contradiction survives: the target
    // announces its own retirement while the referrer still presents it as the
    // live system.
    for link in links(&skill.body) {
        if let Resolution::Found(id) = resolve(tree, &link.target) {
            if id == skill.id {
                continue;
            }
            let Some(target) = tree.get(&id) else {
                continue;
            };
            if target.status != "retired" {
                continue;
            }
            // A parent is *required* to link every child, so scolding it for
            // linking a retired one asks for two rules at once and cannot be
            // satisfied. The dispatch entry is the right place to say a child is
            // retired, not a reason to drop it from the index.
            if target.parent == skill.id {
                continue;
            }
            let hint = if target.superseded_by.is_empty() {
                String::new()
            } else {
                format!("; point at `{}` instead", target.superseded_by)
            };
            push(
                Severity::Warning,
                format!("line {}: `{id}` is retired{hint}", link.line),
            );
        }
    }
}

/// Shouty inline labels. The tree grew out of these; the survivors read as a
/// different author rather than as emphasis.
fn check_shouty_notes(skill: &Skill, push: &mut impl FnMut(Severity, String)) {
    const LABELS: [&str; 6] = [
        "RUNTIME NOTE:",
        "NOTE:",
        "WARNING:",
        "IMPORTANT:",
        "CAUTION:",
        "TODO:",
    ];

    let mut found: Vec<&str> = Vec::new();
    for (line, fenced) in skill.body.lines().zip(fenced_lines(&skill.body)) {
        if fenced {
            continue;
        }
        for label in LABELS {
            if line.contains(label) && !found.contains(&label) {
                found.push(label);
            }
        }
    }

    if !found.is_empty() {
        push(
            Severity::Warning,
            format!(
                "shouty label(s) {}: state the caveat as a sentence or a bold lead-in instead",
                found.join(", ")
            ),
        );
    }
}

/// The brief is one line and `when_to_use` is a trigger, not a summary. Both
/// have hard caps in `skills::lint`; these are the soft targets below them,
/// because a brief that merely fits is still unreadable in a list of forty.
fn check_card_length(skill: &Skill, push: &mut impl FnMut(Severity, String)) {
    let brief = skill.brief.chars().count();
    if brief > BRIEF_SOFT_CHARS {
        push(
            Severity::Warning,
            format!(
                "brief is {brief} chars; aim under {BRIEF_SOFT_CHARS} so it reads as one line \
                 and survives truncation in tool output"
            ),
        );
    }

    let when = skill.when_to_use.chars().count();
    if when > WHEN_TO_USE_SOFT_CHARS {
        push(
            Severity::Warning,
            format!(
                "when_to_use is {when} chars; aim under {WHEN_TO_USE_SOFT_CHARS}: \
                 one or two sentences of trigger plus a boundary"
            ),
        );
    }
}

/// A prose "See also" list of bare names is the thing linked references replace.
fn check_legacy_see_also(skill: &Skill, push: &mut impl FnMut(Severity, String)) {
    let has_see_also = skill
        .body
        .lines()
        .zip(fenced_lines(&skill.body))
        .any(|(l, fenced)| {
            !fenced && {
                let lower = l.to_ascii_lowercase();
                lower.contains("see also")
            }
        });

    if has_see_also && links(&skill.body).is_empty() {
        push(
            Severity::Warning,
            format!(
                "a `See also` line with no linked references; write them as \
                 `[name]({LINK_SCHEME}<full-id>)` so they resolve and get linted"
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{ChildSpec, discover};

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

    fn diags(tree: &SkillTree, id: &str) -> Vec<Diagnostic> {
        body_diagnostics(tree, tree.get(id).unwrap())
    }

    fn says(ds: &[Diagnostic], needle: &str) -> bool {
        ds.iter().any(|d| d.message.contains(needle))
    }

    #[test]
    fn finds_links_and_ignores_code() {
        let body = "See [a](skill:topic/a).\n\n```\n[b](skill:topic/b)\n```\n\nAnd `[c](skill:topic/c)` is quoted.";
        let found: Vec<String> = links(body).into_iter().map(|l| l.target).collect();
        assert_eq!(found, ["topic/a"]);
    }

    #[test]
    fn a_bare_name_shared_across_topics_is_ambiguous() {
        let (_d, tree) = tree_from(&[
            (
                "one/SKILL.md",
                "---\nbrief = \"b\"\n---\n# One\n[x](skill:action)",
            ),
            ("one/action/SKILL.md", "---\nbrief = \"b\"\n---\n# A"),
            ("two/SKILL.md", "---\nbrief = \"b\"\n---\n# Two"),
            ("two/action/SKILL.md", "---\nbrief = \"b\"\n---\n# A"),
        ]);
        assert_eq!(
            resolve(&tree, "action"),
            Resolution::Ambiguous(vec!["one/action".into(), "two/action".into()])
        );
        assert!(says(&diags(&tree, "one"), "is ambiguous across"));
    }

    #[test]
    fn a_full_id_resolves_and_a_ghost_is_an_error() {
        let (_d, tree) = tree_from(&[
            (
                "p/SKILL.md",
                "---\nbrief = \"b\"\n---\n# P\n[k](skill:p/kid) and [g](skill:p/ghost)",
            ),
            ("p/kid/SKILL.md", "---\nbrief = \"b\"\n---\n# K"),
        ]);
        let ds = diags(&tree, "p");
        assert!(says(&ds, "link to `p/ghost` matches no skill"));
        assert!(
            !says(&ds, "`p/kid`"),
            "a link to a real child should say nothing: {ds:?}"
        );
    }

    #[test]
    fn a_parent_must_link_every_child() {
        let (_d, tree) = tree_from(&[
            (
                "p/SKILL.md",
                "---\nbrief = \"b\"\n---\n# P\nOnly [one](skill:p/one).",
            ),
            ("p/one/SKILL.md", "---\nbrief = \"b\"\n---\n# One"),
            ("p/two/SKILL.md", "---\nbrief = \"b\"\n---\n# Two"),
        ]);
        let ds = diags(&tree, "p");
        assert!(says(&ds, "does not link 1 of 2 nested skills: p/two"));
    }

    #[test]
    fn linking_a_retired_skill_is_flagged_with_its_replacement() {
        let (_d, tree) = tree_from(&[
            (
                "live/SKILL.md",
                "---\nbrief = \"b\"\n---\n# Live\nUse [old](skill:old).",
            ),
            (
                "old/SKILL.md",
                "---\nbrief = \"b\"\nstatus = \"retired\"\nsuperseded_by = \"new\"\n---\n# Old",
            ),
            ("new/SKILL.md", "---\nbrief = \"b\"\n---\n# New"),
        ]);
        assert!(says(
            &diags(&tree, "live"),
            "`old` is retired; point at `new`"
        ));
    }

    #[test]
    fn a_parent_may_link_its_own_retired_child() {
        // The two rules would otherwise contradict: the parent must index every
        // child, and must not link anything retired.
        let (_d, tree) = tree_from(&[
            (
                "p/SKILL.md",
                "---\nbrief = \"b\"\n---\n# P\nLegacy: [old](skill:p/old), now [new](skill:p/new).",
            ),
            (
                "p/old/SKILL.md",
                "---\nbrief = \"b\"\nstatus = \"retired\"\nsuperseded_by = \"p/new\"\n---\n# Old",
            ),
            ("p/new/SKILL.md", "---\nbrief = \"b\"\n---\n# New"),
        ]);
        let ds = diags(&tree, "p");
        assert!(!says(&ds, "is retired"), "got: {ds:?}");
        assert!(!says(&ds, "does not link"), "got: {ds:?}");
    }

    #[test]
    fn retirement_target_must_exist() {
        let (_d, tree) = tree_from(&[(
            "old/SKILL.md",
            "---\nbrief = \"b\"\nstatus = \"retired\"\nsuperseded_by = \"nowhere\"\n---\n# Old",
        )]);
        assert!(says(
            &diags(&tree, "old"),
            "superseded_by `nowhere` matches no skill"
        ));
    }

    #[test]
    fn a_body_without_an_h1_is_an_error() {
        let (_d, tree) = tree_from(&[("s.md", "---\nbrief = \"b\"\n---\n## Sub\ntext")]);
        assert!(says(&diags(&tree, "s"), "should open with a `# Title`"));
    }

    #[test]
    fn shouty_labels_and_long_cards_warn() {
        let brief = "x".repeat(BRIEF_SOFT_CHARS + 1);
        let (_d, tree) = tree_from(&[(
            "s.md",
            &format!("---\nbrief = \"{brief}\"\n---\n# S\nRUNTIME NOTE: careful."),
        )]);
        let ds = diags(&tree, "s");
        assert!(says(&ds, "RUNTIME NOTE:"));
        assert!(says(&ds, "aim under"));
    }

    #[test]
    fn an_unclosed_fence_is_an_error() {
        let (_d, tree) = tree_from(&[("s.md", "---\nbrief = \"b\"\n---\n# S\n```moo\ncode")]);
        assert!(says(&diags(&tree, "s"), "never closed"));
    }

    #[test]
    fn a_prose_see_also_warns_until_it_is_linked() {
        let (_d, tree) = tree_from(&[
            ("s.md", "---\nbrief = \"b\"\n---\n# S\nSee also `other`."),
            ("other.md", "---\nbrief = \"b\"\n---\n# O"),
        ]);
        assert!(says(&diags(&tree, "s"), "no linked references"));
        assert_eq!(tree.get("s").unwrap().children, ChildSpec::Auto);
    }

    #[test]
    fn a_clean_skill_produces_nothing() {
        let (_d, tree) = tree_from(&[
            (
                "p/SKILL.md",
                "---\nbrief = \"Short.\"\nwhen_to_use = \"When testing.\"\n---\n# P\n\nSee [kid](skill:p/kid).",
            ),
            (
                "p/kid/SKILL.md",
                "---\nbrief = \"Short.\"\nwhen_to_use = \"When testing.\"\n---\n# Kid\n\nBody.",
            ),
        ]);
        assert_eq!(diags(&tree, "p"), Vec::new());
        assert_eq!(diags(&tree, "p/kid"), Vec::new());
    }
}
