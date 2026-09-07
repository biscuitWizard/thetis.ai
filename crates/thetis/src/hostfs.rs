//! Filesystem access on the host.
//!
//! Distinct from `sandbox`, which runs code in a container: these operations
//! touch the machine the orchestrator runs on, so the whole interface is off
//! unless configuration turns it on.
//!
//! The real boundary is the configured roots: every path is resolved and must
//! land inside one of them, checked after symlinks are followed so a link
//! cannot be used to step outside. The `protected` list is a smaller thing —
//! it stops the system from deleting its own database by accident, and is not
//! a security control, because a terminal session can reach those paths anyway.

use anyhow::{Result, anyhow};
use std::path::{Component, Path, PathBuf};

use crate::bindings::types::FsEntry;
use crate::config::Config;

/// A Windows drive prefix (`C:/...`). On Windows this parses as a path
/// prefix component; on this platform it would silently become a literal
/// directory named `C:`, so it is rejected up front to keep guest-facing
/// path semantics identical everywhere.
pub(crate) fn has_drive_prefix(path: &str) -> bool {
    let first = path.split(['/', '\\']).next().unwrap_or("");
    first.len() == 2 && first.as_bytes()[1] == b':' && first.as_bytes()[0].is_ascii_alphabetic()
}

/// Maps a guest-facing preopen path (`/workspace/...`) onto the real directory
/// behind it.
///
/// A guest sees each WASI preopen under its own last path segment, so the
/// shared workspace is `/workspace` no matter where it lives on this machine.
/// Anything that is not such a path comes back untouched, and the result still
/// goes through the ordinary root check afterwards — this only fixes the
/// spelling, it grants nothing.
fn rewrite_workspace_prefix(cfg: &Config, raw: &str) -> String {
    for dir in &cfg.wasi.dirs {
        let Some(name) = dir.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        let prefix = format!("/{name}");
        if raw == prefix {
            return dir.to_string_lossy().to_string();
        }
        if let Some(rest) = raw.strip_prefix(&format!("{prefix}/")) {
            return dir.join(rest).to_string_lossy().to_string();
        }
    }
    raw.to_string()
}

/// Resolves a guest-supplied path against the configured roots.
///
/// Relative paths are taken against the first root, which is the project root
/// unless configuration says otherwise.
pub fn resolve(cfg: &Config, raw: &str) -> Result<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(anyhow!("path is empty"));
    }
    if has_drive_prefix(raw) {
        return Err(anyhow!("{raw} is outside the allowed roots"));
    }

    let first_root = cfg
        .filesystem
        .roots
        .first()
        .ok_or_else(|| anyhow!("no filesystem roots are configured"))?;

    // `/workspace` is what every guest sees the shared preopen as, so it is the
    // name an agent reaches for — and taken literally it names a directory at
    // the filesystem root that does not exist. Rewrite it to the real workspace
    // path before anything else looks at it, so the spelling the agent knows and
    // the spelling on disk are the same place.
    let raw = &rewrite_workspace_prefix(cfg, raw);

    let candidate = Path::new(raw.as_str());
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        first_root.join(candidate)
    };

    let normalised = normalise(&joined);

    // Compare against the roots using whichever form exists on disk: an
    // existing path is canonicalised so symlinks cannot escape, while a path
    // being created is checked by its parent.
    let probe = if normalised.exists() {
        canonical(&normalised)
    } else {
        match normalised.parent() {
            Some(parent) if parent.exists() => canonical(parent).join(
                normalised
                    .file_name()
                    .map(Path::new)
                    .unwrap_or_else(|| Path::new("")),
            ),
            _ => normalised.clone(),
        }
    };

    let inside = cfg
        .filesystem
        .roots
        .iter()
        .any(|root| probe.starts_with(canonical(root)));

    if !inside {
        return Err(anyhow!(
            "{} is outside the allowed roots ({})",
            normalised.display(),
            cfg.filesystem
                .roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    Ok(normalised)
}

/// Removes `.` and `..` without touching the disk, so a path that does not
/// exist yet is still checked properly.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `std::fs::canonicalize` returns `\\?\` paths on Windows, which do not
/// compare cleanly against ordinary ones.
fn canonical(path: &Path) -> PathBuf {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => resolved,
    }
}

/// Whether any part of the path is on the protected list.
fn is_protected(cfg: &Config, path: &Path) -> Option<String> {
    for root in &cfg.filesystem.roots {
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        for part in relative.components() {
            let name = part.as_os_str().to_string_lossy().to_string();
            if cfg
                .filesystem
                .protected
                .iter()
                .any(|p| p.eq_ignore_ascii_case(&name))
            {
                return Some(name);
            }
        }
    }
    None
}

fn require_enabled(cfg: &Config) -> Result<()> {
    if cfg.filesystem.enabled {
        Ok(())
    } else {
        Err(anyhow!(
            "filesystem access is off; set filesystem.enabled in thetis.toml to turn it on"
        ))
    }
}

/// Shows a path relative to its root, so messages stay readable.
///
/// A path inside a WASI preopen is rendered with its guest prefix — a workspace
/// file reads as `/workspace/notes.md`, not `notes.md`. That is not cosmetic:
/// relative paths resolve against the *first* root, so a bare `notes.md`
/// handed back from the workspace would be read again from the project root and
/// either miss or, worse, hit a different file of the same name. Preopens are
/// checked before the plain roots for exactly that reason.
fn display(cfg: &Config, path: &Path) -> String {
    for dir in &cfg.wasi.dirs {
        let Ok(relative) = path.strip_prefix(dir) else {
            continue;
        };
        let Some(name) = dir.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        return if relative.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name}/{relative}")
        };
    }
    for root in &cfg.filesystem.roots {
        if let Ok(relative) = path.strip_prefix(root) {
            return relative.to_string_lossy().replace('\\', "/");
        }
    }
    path.display().to_string()
}

// --- operations -------------------------------------------------------------

pub fn read_file(cfg: &Config, raw: &str) -> Result<String> {
    require_enabled(cfg)?;
    let path = resolve(cfg, raw)?;

    let size = std::fs::metadata(&path)
        .map_err(|e| anyhow!("cannot read {}: {e}", display(cfg, &path)))?
        .len() as usize;
    if size > cfg.filesystem.max_read_bytes {
        return Err(anyhow!(
            "{} is {size} bytes, over the {} byte read limit",
            display(cfg, &path),
            cfg.filesystem.max_read_bytes
        ));
    }

    std::fs::read_to_string(&path).map_err(|e| anyhow!("cannot read {}: {e}", display(cfg, &path)))
}

pub fn write_file(cfg: &Config, raw: &str, contents: &str) -> Result<String> {
    require_enabled(cfg)?;
    let path = resolve(cfg, raw)?;

    if let Some(name) = is_protected(cfg, &path) {
        return Err(anyhow!("{name} is protected and cannot be written"));
    }
    if path.is_dir() {
        return Err(anyhow!("{} is a directory", display(cfg, &path)));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("cannot create {}: {e}", display(cfg, parent)))?;
    }
    std::fs::write(&path, contents)
        .map_err(|e| anyhow!("cannot write {}: {e}", display(cfg, &path)))?;

    Ok(format!(
        "wrote {} ({} bytes)",
        display(cfg, &path),
        contents.len()
    ))
}

pub fn list_dir(cfg: &Config, raw: &str) -> Result<Vec<FsEntry>> {
    require_enabled(cfg)?;
    let path = resolve(cfg, raw)?;

    let entries = std::fs::read_dir(&path)
        .map_err(|e| anyhow!("cannot list {}: {e}", display(cfg, &path)))?;

    let mut out: Vec<FsEntry> = entries
        .flatten()
        .map(|entry| {
            let meta = entry.metadata().ok();
            let full = entry.path();
            FsEntry {
                name: entry.file_name().to_string_lossy().to_string(),
                path: display(cfg, &full),
                is_dir: meta.as_ref().is_some_and(|m| m.is_dir()),
                size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
            }
        })
        .collect();

    // Directories first, then by name: the order someone reading a listing expects.
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    Ok(out)
}

pub fn delete_path(cfg: &Config, raw: &str, recursive: bool) -> Result<String> {
    require_enabled(cfg)?;
    if !cfg.filesystem.allow_delete {
        return Err(anyhow!(
            "deleting is off; set filesystem.allow_delete to turn it on"
        ));
    }

    let path = resolve(cfg, raw)?;
    if let Some(name) = is_protected(cfg, &path) {
        return Err(anyhow!("{name} is protected and cannot be deleted"));
    }
    // Deleting a root would take the workspace with it.
    if cfg
        .filesystem
        .roots
        .iter()
        .any(|r| canonical(r) == canonical(&path))
    {
        return Err(anyhow!("refusing to delete a configured root"));
    }
    if !path.exists() {
        return Err(anyhow!("{} does not exist", display(cfg, &path)));
    }

    if path.is_dir() {
        if !recursive {
            let count = std::fs::read_dir(&path).map(|e| e.count()).unwrap_or(0);
            if count > 0 {
                return Err(anyhow!(
                    "{} is a directory with {count} entries; pass recursive to delete it",
                    display(cfg, &path)
                ));
            }
        }
        std::fs::remove_dir_all(&path)
            .map_err(|e| anyhow!("cannot delete {}: {e}", display(cfg, &path)))?;
    } else {
        std::fs::remove_file(&path)
            .map_err(|e| anyhow!("cannot delete {}: {e}", display(cfg, &path)))?;
    }

    Ok(format!("deleted {}", display(cfg, &path)))
}

// --- navigation: search, glob, windowed reads, in-place edits ----------------
//
// These exist because without them the agent does the same work through the
// terminal — `grep`, `sed -n`, `sed -i`, heredocs — which costs far more tokens,
// fails on quoting and whitespace, and (because a shell write is invisible to
// the dev kit) gets recorded as a human edit rather than an agent one.

/// Directories never worth walking: build output, version-control internals,
/// vendored dependencies. Searching them buries a real hit under thousands of
/// artefacts, and on this project `target/` alone is larger than the source by
/// orders of magnitude.
const SKIPPED_DIRS: &[&str] = &[
    ".git",
    "target",
    "target-wasm",
    "worktrees",
    "node_modules",
    "artifacts",
    "data",
    "vendor",
    "dist",
    ".cache",
    ".venv",
    "__pycache__",
];

/// How many files a single walk will look at before giving up. A bound here
/// turns a runaway search into an honest partial answer instead of a hang.
const MAX_FILES_SCANNED: usize = 20_000;

/// Longest line echoed back in a search result. Minified files and embedded
/// data would otherwise spend the whole output budget on one match.
const MAX_LINE_LEN: usize = 300;

/// Whether a file looks like text worth searching.
///
/// A NUL byte in the first block is the same heuristic `grep` uses, and it is
/// right often enough that the alternative — encoding sniffing — is not worth
/// the complexity.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

/// Translates a shell-style glob into a regex.
///
/// A pattern with no `/` is matched against the file name alone, so `*.rs`
/// finds Rust files anywhere without the caller having to write `**/*.rs`.
/// That is the shape people reach for first, and refusing it just costs a
/// round trip.
fn glob_to_regex(glob: &str) -> Result<regex::Regex> {
    let mut out = String::from("^");
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    // `**/` spans directories including none at all, so
                    // `src/**/*.rs` still matches `src/lib.rs`.
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        out.push_str("(?:.*/)?");
                    } else {
                        out.push_str(".*");
                    }
                } else {
                    out.push_str("[^/]*");
                }
            }
            '?' => out.push_str("[^/]"),
            '[' => {
                out.push('[');
                if chars.peek() == Some(&'!') {
                    chars.next();
                    out.push('^');
                }
                for c in chars.by_ref() {
                    if c == ']' {
                        break;
                    }
                    out.push(c);
                }
                out.push(']');
            }
            other => out.push_str(&regex::escape(&other.to_string())),
        }
    }
    out.push('$');
    regex::Regex::new(&out).map_err(|e| anyhow!("bad glob `{glob}`: {e}"))
}

/// Whether a path matches a glob, using the name-only rule described above.
fn glob_matches(re: &regex::Regex, relative: &str) -> bool {
    re.is_match(relative)
}

fn glob_target<'a>(glob: &str, relative: &'a str) -> &'a str {
    if glob.contains('/') {
        relative
    } else {
        relative.rsplit('/').next().unwrap_or(relative)
    }
}

/// Walks a directory, depth first, handing every file to `visit`.
///
/// Stops early when `visit` says so or when the scan bound is hit, and reports
/// which of the two happened so the caller can tell the agent the truth about
/// completeness.
fn walk(root: &Path, mut visit: impl FnMut(&Path) -> bool) -> bool {
    let mut stack = vec![root.to_path_buf()];
    let mut scanned = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                if SKIPPED_DIRS.contains(&name.as_str()) {
                    continue;
                }
                stack.push(path);
            } else {
                children.push(path);
            }
        }
        // Sorted so results are stable between runs; an agent comparing two
        // searches should not see the order change underneath it.
        children.sort();
        for path in children {
            scanned += 1;
            if scanned > MAX_FILES_SCANNED {
                return false;
            }
            if !visit(&path) {
                return false;
            }
        }
    }
    true
}

/// Where a walk starts, and whether it is confined to a single file.
///
/// `search_files` and `find_files` both take a `path`. Callers very often pass
/// a *file* there: it reads naturally, and the alternative spelling — `path`
/// set to the directory and `glob` to the file's name — is not the obvious one.
/// Refusing it used to be a dead end. The error said what was wrong but not
/// what to write instead, and a caller with no memory of its previous attempt
/// would try another spelling of the same thing and get the same rejection, so
/// the mistake cost several turns rather than one. A file is therefore a legal
/// scope now, meaning "look in just this one".
struct WalkScope {
    /// The directory relative paths are reported against. For a single file
    /// this is its parent, so a `glob` still matches against the file's name
    /// rather than against the empty string.
    root: PathBuf,
    /// The only file the walk may visit, when the scope named a file.
    only: Option<PathBuf>,
}

impl WalkScope {
    /// Hands every file in the scope to `visit`; reports whether the walk ran
    /// to completion, as [`walk`] does.
    fn walk(&self, mut visit: impl FnMut(&Path) -> bool) -> bool {
        match &self.only {
            // Not `walk` over the parent with a filter: the parent may hold
            // thousands of files, and scanning them to discard all but one
            // would burn the scan bound and then claim the result was partial.
            Some(one) => {
                visit(one);
                true
            }
            None => walk(&self.root, visit),
        }
    }

    /// What to name in a message about this scope.
    fn target(&self) -> &Path {
        self.only.as_deref().unwrap_or(&self.root)
    }
}

/// The scope a walk covers, defaulting to the first configured root.
fn walk_root(cfg: &Config, path: Option<&str>) -> Result<WalkScope> {
    match path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => {
            let resolved = resolve(cfg, p)?;
            if resolved.is_dir() {
                return Ok(WalkScope {
                    root: resolved,
                    only: None,
                });
            }
            if resolved.is_file() {
                // Fall back to the file itself if it somehow has no parent, so
                // a relative path stays computable either way.
                let root = resolved
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| resolved.clone());
                return Ok(WalkScope {
                    root,
                    only: Some(resolved),
                });
            }
            Err(anyhow!("{} does not exist", display(cfg, &resolved)))
        }
        None => cfg
            .filesystem
            .roots
            .first()
            .cloned()
            .map(|root| WalkScope { root, only: None })
            .ok_or_else(|| anyhow!("no filesystem roots are configured")),
    }
}

fn clip(line: &str) -> String {
    let line = line.trim_end();
    if line.chars().count() <= MAX_LINE_LEN {
        return line.to_string();
    }
    let cut: String = line.chars().take(MAX_LINE_LEN).collect();
    format!("{cut}… [line truncated]")
}

/// Reads a window of a file, numbered from 1.
///
/// Line numbers are not decoration: they are what makes a later `edit_file`
/// anchorable and what lets a stack trace be matched to a source line without
/// re-reading the whole file.
pub fn read_file_range(cfg: &Config, raw: &str, offset: u32, limit: u32) -> Result<String> {
    require_enabled(cfg)?;
    let path = resolve(cfg, raw)?;

    let bytes =
        std::fs::read(&path).map_err(|e| anyhow!("cannot read {}: {e}", display(cfg, &path)))?;
    if looks_binary(&bytes) {
        return Err(anyhow!(
            "{} looks like a binary file, not text",
            display(cfg, &path)
        ));
    }
    let text = String::from_utf8_lossy(&bytes);

    let total = text.lines().count();
    // A zero offset is what a caller means by "from the beginning"; treating it
    // as line zero would silently drop the first line.
    let start = offset.max(1) as usize;
    let limit = if limit == 0 { 2000 } else { limit as usize };

    if start > total && total > 0 {
        return Err(anyhow!(
            "{} has {total} lines; offset {start} is past the end",
            display(cfg, &path)
        ));
    }

    let mut out = String::new();
    for (i, line) in text.lines().enumerate().skip(start - 1).take(limit) {
        out.push_str(&format!("{:>6}\t{}\n", i + 1, clip(line)));
    }

    let shown_end = (start + limit - 1).min(total);
    if shown_end < total {
        out.push_str(&format!(
            "\n[lines {start}-{shown_end} of {total}; read on with offset {}]\n",
            shown_end + 1
        ));
    } else if start > 1 {
        out.push_str(&format!("\n[lines {start}-{total} of {total}]\n"));
    }
    Ok(out)
}

/// Replaces an exact snippet in a file on the host.
///
/// The whole-file `write_file` is the wrong instrument for a small change: it
/// spends the file's length in tokens twice over and risks losing everything
/// the agent did not think to echo back. This changes only what was named, and
/// refuses ambiguity rather than guessing which occurrence was meant.
pub fn edit_file(
    cfg: &Config,
    raw: &str,
    old_text: &str,
    new_text: &str,
    replace_all: bool,
) -> Result<String> {
    require_enabled(cfg)?;
    let path = resolve(cfg, raw)?;

    if let Some(name) = is_protected(cfg, &path) {
        return Err(anyhow!("{name} is protected and cannot be written"));
    }
    if old_text.is_empty() {
        return Err(anyhow!(
            "old_text is empty; use write_path to create a file"
        ));
    }
    if old_text == new_text {
        return Err(anyhow!(
            "old_text and new_text are identical, so there is nothing to change"
        ));
    }

    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow!("cannot read {}: {e}", display(cfg, &path)))?;

    let count = text.matches(old_text).count();
    // Both failures are recoverable, so each says what to do next rather than
    // just what went wrong.
    if count == 0 {
        return Err(anyhow!(
            "old_text was not found in {}. Read the file first — whitespace and indentation \
             must match exactly.",
            display(cfg, &path)
        ));
    }
    if count > 1 && !replace_all {
        return Err(anyhow!(
            "old_text appears {count} times in {}. Include enough surrounding lines to make it \
             unique, or pass replace_all to change every occurrence.",
            display(cfg, &path)
        ));
    }

    let updated = if replace_all {
        text.replace(old_text, new_text)
    } else {
        text.replacen(old_text, new_text, 1)
    };
    std::fs::write(&path, &updated)
        .map_err(|e| anyhow!("cannot write {}: {e}", display(cfg, &path)))?;

    // The line number lets the agent read back the neighbourhood it just
    // changed without hunting for it.
    let line = text[..text.find(old_text).unwrap_or(0)].lines().count() + 1;
    Ok(format!(
        "edited {} — replaced {count} occurrence{} starting at line {line}",
        display(cfg, &path),
        if count == 1 || !replace_all { "" } else { "s" }
    ))
}

/// How much of a search result to return.
pub enum SearchMode {
    /// `path:line: text` for every match.
    Content,
    /// Just the paths that contain a match.
    Files,
    /// A count per file.
    Count,
}

impl SearchMode {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim() {
            "" | "content" => Ok(Self::Content),
            "files" => Ok(Self::Files),
            "count" => Ok(Self::Count),
            other => Err(anyhow!(
                "unknown mode `{other}`; expected content, files, or count"
            )),
        }
    }
}

/// Searches file contents for a regular expression.
///
/// Returns a rendered report rather than a structure: the agent reads this, and
/// a flat `path:line: text` listing is both the most compact form and the one
/// every model has already seen a million times in `grep` output.
pub fn search_files(
    cfg: &Config,
    pattern: &str,
    path: Option<&str>,
    glob: Option<&str>,
    mode: &str,
    max_results: u32,
) -> Result<String> {
    require_enabled(cfg)?;
    let mode = SearchMode::parse(mode)?;
    let root = walk_root(cfg, path)?;

    let re = regex::RegexBuilder::new(pattern)
        .size_limit(10 << 20)
        .build()
        .map_err(|e| anyhow!("bad pattern `{pattern}`: {e}"))?;
    let glob_re = glob.map(glob_to_regex).transpose()?;
    let cap = if max_results == 0 {
        100
    } else {
        max_results as usize
    };

    let mut lines: Vec<String> = Vec::new();
    let mut files_with_matches = 0usize;
    let mut total_matches = 0usize;
    let mut hit_cap = false;

    let complete = root.walk(|file| {
        if let (Some(g), Some(rel)) = (&glob_re, path_relative(&root.root, file)) {
            if !glob_matches(g, glob_target(glob.unwrap_or(""), &rel)) {
                return true;
            }
        }
        let Ok(bytes) = std::fs::read(file) else {
            return true;
        };
        if looks_binary(&bytes) {
            return true;
        }
        let text = String::from_utf8_lossy(&bytes);
        let shown = display(cfg, file);

        let mut in_file = 0usize;
        for (i, line) in text.lines().enumerate() {
            if !re.is_match(line) {
                continue;
            }
            in_file += 1;
            total_matches += 1;
            if matches!(mode, SearchMode::Content) {
                if lines.len() < cap {
                    lines.push(format!("{shown}:{}: {}", i + 1, clip(line)));
                } else {
                    hit_cap = true;
                    return false;
                }
            }
        }
        if in_file > 0 {
            files_with_matches += 1;
            match mode {
                SearchMode::Files => {
                    if lines.len() < cap {
                        lines.push(shown);
                    } else {
                        hit_cap = true;
                        return false;
                    }
                }
                SearchMode::Count => {
                    if lines.len() < cap {
                        lines.push(format!("{shown}: {in_file}"));
                    } else {
                        hit_cap = true;
                        return false;
                    }
                }
                SearchMode::Content => {}
            }
        }
        true
    });

    if lines.is_empty() {
        return Ok(format!(
            "no matches for /{pattern}/ under {}{}",
            display(cfg, root.target()),
            glob.map(|g| format!(" matching {g}")).unwrap_or_default()
        ));
    }

    let mut out = lines.join("\n");
    out.push_str(&format!(
        "\n\n{total_matches} match{} in {files_with_matches} file{}",
        if total_matches == 1 { "" } else { "es" },
        if files_with_matches == 1 { "" } else { "s" },
    ));
    // Say plainly when the answer is partial, and say what to do about it —
    // a silently truncated search reads as a complete one and gets trusted.
    if hit_cap {
        out.push_str(&format!(
            " (stopped at the first {cap}). Narrow with a tighter pattern, a `glob` such as \
             '*.rs', or a `path` deeper in the tree; or use mode='files' to see just where."
        ));
    } else if !complete {
        out.push_str(
            " (the scan bound was reached before the tree was exhausted; narrow `path` to be sure).",
        );
    }
    out.push('\n');
    Ok(out)
}

/// Lists files whose path matches a glob, most recently modified first.
///
/// Recency is the useful order: when an agent asks which files match, it is
/// almost always looking for the ones somebody has been working in.
pub fn find_files(
    cfg: &Config,
    glob: &str,
    path: Option<&str>,
    max_results: u32,
) -> Result<String> {
    require_enabled(cfg)?;
    let root = walk_root(cfg, path)?;
    let re = glob_to_regex(glob)?;
    let cap = if max_results == 0 {
        200
    } else {
        max_results as usize
    };

    let mut found: Vec<(std::time::SystemTime, String)> = Vec::new();
    let complete = root.walk(|file| {
        let Some(rel) = path_relative(&root.root, file) else {
            return true;
        };
        if !glob_matches(&re, glob_target(glob, &rel)) {
            return true;
        }
        let modified = std::fs::metadata(file)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        found.push((modified, display(cfg, file)));
        true
    });

    if found.is_empty() {
        return Ok(format!(
            "nothing matching {glob} under {}",
            display(cfg, root.target())
        ));
    }

    found.sort_by(|a, b| b.0.cmp(&a.0));
    let total = found.len();
    let mut out: Vec<String> = found.into_iter().take(cap).map(|(_, p)| p).collect();
    let shown = out.len();
    out.push(String::new());
    out.push(format!(
        "{total} file{} matching {glob}{}{}",
        if total == 1 { "" } else { "s" },
        if shown < total {
            format!(" (newest {shown} shown)")
        } else {
            String::new()
        },
        if complete {
            ""
        } else {
            "; the scan bound was reached, so there may be more"
        }
    ));
    Ok(out.join("\n"))
}

/// A path relative to the walk root, in `/`-separated form.
fn path_relative(root: &Path, file: &Path) -> Option<String> {
    file.strip_prefix(root)
        .ok()
        .map(|r| r.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Config, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical(dir.path());
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("data/thetis.redb"), "state").unwrap();

        let mut cfg = Config::load().unwrap();
        cfg.root = root.clone();
        cfg.filesystem.roots = vec![root];
        cfg.filesystem.enabled = true;
        (cfg, dir)
    }

    #[test]
    fn reads_and_writes_inside_a_root() {
        let (cfg, _d) = fixture();
        assert_eq!(read_file(&cfg, "src/main.rs").unwrap(), "fn main() {}");

        write_file(&cfg, "notes/todo.md", "- ship it").unwrap();
        assert_eq!(read_file(&cfg, "notes/todo.md").unwrap(), "- ship it");
    }

    // --- navigation ---------------------------------------------------------

    /// A tree with something to find in it, plus the build output a search must
    /// not wade through.
    fn searchable() -> (Config, tempfile::TempDir) {
        let (cfg, dir) = fixture();
        let root = cfg.filesystem.roots[0].clone();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn parse() {}\npub fn render() {}\n// parse again\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::write(root.join("src/deep/mod.rs"), "fn parse() {}\n").unwrap();
        std::fs::write(root.join("README.md"), "how to parse\n").unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join("target/debug/huge.rs"), "fn parse() {}\n").unwrap();
        (cfg, dir)
    }

    #[test]
    fn search_reports_path_and_line_and_skips_build_output() {
        let (cfg, _d) = searchable();
        let out = search_files(&cfg, "fn parse", None, None, "content", 0).unwrap();

        assert!(out.contains("src/lib.rs:1:"), "{out}");
        assert!(out.contains("src/deep/mod.rs:1:"), "{out}");
        // `target/` is where a real search drowns; it must never be walked.
        assert!(!out.contains("huge.rs"), "{out}");
    }

    #[test]
    fn search_narrows_by_glob_and_by_path() {
        let (cfg, _d) = searchable();

        let rust_only = search_files(&cfg, "parse", None, Some("*.rs"), "files", 0).unwrap();
        assert!(!rust_only.contains("README.md"), "{rust_only}");
        assert!(rust_only.contains("src/lib.rs"), "{rust_only}");

        let deep = search_files(&cfg, "parse", Some("src/deep"), None, "content", 0).unwrap();
        assert!(deep.contains("src/deep/mod.rs"), "{deep}");
        assert!(!deep.contains("src/lib.rs"), "{deep}");
    }

    /// Passing a file as `path` is what callers actually write, so it has to
    /// work. It used to be refused with "is not a directory" — an error that
    /// named the problem but not the remedy, so a caller would rephrase the
    /// same mistake instead of correcting it.
    #[test]
    fn a_file_is_a_legal_search_scope() {
        let (cfg, _d) = searchable();

        let one = search_files(&cfg, "parse", Some("src/lib.rs"), None, "content", 0).unwrap();
        assert!(one.contains("src/lib.rs:1:"), "{one}");
        assert!(one.contains("src/lib.rs:3:"), "{one}");
        // Confinement is the point: a sibling that matches must not appear.
        assert!(!one.contains("deep/mod.rs"), "{one}");

        // And a glob still has a name to match against, not an empty string.
        let globbed = search_files(&cfg, "parse", Some("src/lib.rs"), Some("*.rs"), "files", 0);
        assert!(globbed.unwrap().contains("src/lib.rs"), "glob should match");

        let missed = search_files(&cfg, "parse", Some("README.md"), Some("*.rs"), "files", 0);
        assert!(
            missed.unwrap().starts_with("no matches"),
            "glob should filter"
        );
    }

    #[test]
    fn find_can_be_scoped_to_one_file() {
        let (cfg, _d) = searchable();
        let out = find_files(&cfg, "*.rs", Some("src/lib.rs"), 0).unwrap();
        assert!(out.contains("src/lib.rs"), "{out}");
        assert!(!out.contains("deep/mod.rs"), "{out}");
    }

    /// A path that is neither file nor directory is still an error — but one
    /// that says what is actually wrong.
    #[test]
    fn a_path_that_does_not_exist_says_that_much() {
        let (cfg, _d) = searchable();
        let err = search_files(&cfg, "parse", Some("src/nope.rs"), None, "content", 0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
        assert!(!err.contains("not a directory"), "{err}");
    }

    #[test]
    fn search_says_when_it_stopped_early() {
        let (cfg, _d) = searchable();
        let out = search_files(&cfg, "parse", None, None, "content", 1).unwrap();
        // A truncated search that reads as a complete one is worse than no
        // search at all, so the notice is part of the contract.
        assert!(out.contains("stopped at the first 1"), "{out}");
    }

    #[test]
    fn search_that_finds_nothing_says_so_plainly() {
        let (cfg, _d) = searchable();
        let out = search_files(&cfg, "nowhere_at_all", None, None, "content", 0).unwrap();
        assert!(out.starts_with("no matches for"), "{out}");
    }

    #[test]
    fn find_matches_a_bare_name_anywhere_and_a_path_glob_precisely() {
        let (cfg, _d) = searchable();

        let anywhere = find_files(&cfg, "*.rs", None, 0).unwrap();
        assert!(anywhere.contains("src/deep/mod.rs"), "{anywhere}");
        assert!(!anywhere.contains("README.md"), "{anywhere}");

        let scoped = find_files(&cfg, "src/*.rs", None, 0).unwrap();
        assert!(scoped.contains("src/lib.rs"), "{scoped}");
        // A single star does not cross a directory separator.
        assert!(!scoped.contains("deep/mod.rs"), "{scoped}");

        let recursive = find_files(&cfg, "src/**/*.rs", None, 0).unwrap();
        assert!(recursive.contains("src/deep/mod.rs"), "{recursive}");
        // `**/` spans no directories as readily as many.
        assert!(recursive.contains("src/lib.rs"), "{recursive}");
    }

    #[test]
    fn reads_a_numbered_window_and_says_how_to_read_on() {
        let (cfg, _d) = fixture();
        let body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        write_file(&cfg, "src/long.rs", &body).unwrap();

        let head = read_file_range(&cfg, "src/long.rs", 0, 3).unwrap();
        assert!(head.starts_with("     1\tline 1\n"), "{head}");
        assert!(head.contains("     3\tline 3"), "{head}");
        assert!(!head.contains("line 4"), "{head}");
        assert!(head.contains("read on with offset 4"), "{head}");

        let tail = read_file_range(&cfg, "src/long.rs", 9, 0).unwrap();
        assert!(tail.contains("     9\tline 9"), "{tail}");
        assert!(tail.contains("[lines 9-10 of 10]"), "{tail}");
    }

    #[test]
    fn editing_replaces_exactly_what_was_named() {
        let (cfg, _d) = fixture();
        write_file(&cfg, "src/edit.rs", "let a = 1;\nlet b = 2;\n").unwrap();

        let report = edit_file(&cfg, "src/edit.rs", "let b = 2;", "let b = 3;", false).unwrap();
        assert!(report.contains("line 2"), "{report}");
        assert_eq!(
            read_file(&cfg, "src/edit.rs").unwrap(),
            "let a = 1;\nlet b = 3;\n"
        );
    }

    #[test]
    fn editing_refuses_ambiguity_and_says_how_to_resolve_it() {
        let (cfg, _d) = fixture();
        write_file(&cfg, "src/dup.rs", "x = 1;\nx = 1;\n").unwrap();

        let err = edit_file(&cfg, "src/dup.rs", "x = 1;", "x = 2;", false).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("appears 2 times"), "{text}");
        assert!(text.contains("replace_all"), "{text}");
        // Nothing was written while the call was ambiguous.
        assert_eq!(read_file(&cfg, "src/dup.rs").unwrap(), "x = 1;\nx = 1;\n");

        edit_file(&cfg, "src/dup.rs", "x = 1;", "x = 2;", true).unwrap();
        assert_eq!(read_file(&cfg, "src/dup.rs").unwrap(), "x = 2;\nx = 2;\n");
    }

    #[test]
    fn editing_a_snippet_that_is_not_there_explains_why() {
        let (cfg, _d) = fixture();
        let err = edit_file(&cfg, "src/main.rs", "fn nope()", "fn yes()", false).unwrap_err();
        assert!(
            format!("{err:#}").contains("Read the file first"),
            "{err:#}"
        );
    }

    #[test]
    fn editing_honours_the_protected_list() {
        let (cfg, _d) = fixture();
        let err = edit_file(&cfg, "data/thetis.redb", "state", "wiped", false).unwrap_err();
        assert!(format!("{err:#}").contains("protected"), "{err:#}");
    }

    #[test]
    fn navigation_cannot_escape_the_roots_either() {
        let (cfg, _d) = fixture();
        for call in [
            search_files(&cfg, "x", Some("/etc"), None, "content", 0),
            find_files(&cfg, "*", Some("../.."), 0),
            read_file_range(&cfg, "/etc/passwd", 0, 5),
        ] {
            let err = call.unwrap_err();
            assert!(
                format!("{err:#}").contains("outside the allowed roots"),
                "{err:#}"
            );
        }
    }

    #[test]
    fn refuses_to_escape_the_roots() {
        let (cfg, _d) = fixture();
        for bad in [
            "../outside.txt",
            "src/../../outside.txt",
            "/etc/passwd",
            "C:/Windows/System32/drivers/etc/hosts",
        ] {
            let err = resolve(&cfg, bad).unwrap_err();
            assert!(
                format!("{err:#}").contains("outside the allowed roots"),
                "{bad} gave: {err:#}"
            );
        }
    }

    /// The shared workspace has to be reachable by the name the agent knows it
    /// by. A guest sees the preopen as `/workspace`; on this machine it is some
    /// other absolute path entirely, and taking the guest spelling literally
    /// used to produce "outside the allowed roots" for the one directory every
    /// agent is meant to share.
    #[test]
    fn the_workspace_is_reachable_by_its_guest_name() {
        let (mut cfg, _d) = fixture();
        let ws = cfg.filesystem.roots[0].join("shared-ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("note.md"), "shared").unwrap();
        cfg.wasi.dirs = vec![ws.clone()];
        cfg.filesystem.roots.push(ws.clone());

        assert_eq!(resolve(&cfg, "/shared-ws").unwrap(), ws);
        assert_eq!(
            resolve(&cfg, "/shared-ws/note.md").unwrap(),
            ws.join("note.md")
        );
        assert_eq!(read_file(&cfg, "/shared-ws/note.md").unwrap(), "shared");
        assert!(
            list_dir(&cfg, "/shared-ws")
                .unwrap()
                .iter()
                .any(|e| e.name == "note.md")
        );
    }

    /// Whatever a listing or a search result calls a file, feeding that name
    /// straight back to another call has to reach the same file. Making the
    /// workspace a root put that at risk: relative names resolve against the
    /// *first* root, so a workspace file rendered as a bare `note.md` would be
    /// read again from the project root.
    #[test]
    fn a_workspace_path_round_trips_through_display() {
        let (mut cfg, _d) = fixture();
        let project = cfg.filesystem.roots[0].clone();
        let ws = project.join("shared-ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("note.md"), "in the workspace").unwrap();
        // Same file name in the project root: a bare `note.md` would find this
        // one instead, which is the failure worth catching.
        std::fs::write(project.join("note.md"), "in the project").unwrap();
        cfg.wasi.dirs = vec![ws.clone()];
        cfg.filesystem.roots.push(ws.clone());

        let shown = display(&cfg, &ws.join("note.md"));
        assert_eq!(shown, "/shared-ws/note.md");
        assert_eq!(read_file(&cfg, &shown).unwrap(), "in the workspace");

        // And the listing agrees with itself.
        let entry = list_dir(&cfg, "/shared-ws")
            .unwrap()
            .into_iter()
            .find(|e| e.name == "note.md")
            .expect("note.md should be listed");
        assert_eq!(read_file(&cfg, &entry.path).unwrap(), "in the workspace");
    }

    /// Rewriting the prefix is a spelling fix, not a grant: `..` out of the
    /// workspace still has to land inside a root to be allowed.
    #[test]
    fn the_workspace_alias_still_cannot_escape() {
        let (mut cfg, _d) = fixture();
        let ws = cfg.filesystem.roots[0].join("shared-ws");
        std::fs::create_dir_all(&ws).unwrap();
        cfg.wasi.dirs = vec![ws];

        let err = resolve(&cfg, "/shared-ws/../../../../etc/passwd").unwrap_err();
        assert!(
            format!("{err:#}").contains("outside the allowed roots"),
            "{err:#}"
        );
    }

    #[test]
    fn traversal_that_stays_inside_is_allowed() {
        let (cfg, _d) = fixture();
        // Normalises to src/main.rs, which is within the root.
        assert_eq!(
            read_file(&cfg, "src/../src/main.rs").unwrap(),
            "fn main() {}"
        );
    }

    #[test]
    fn protected_paths_survive_writes_and_deletes() {
        let (cfg, _d) = fixture();
        let write = write_file(&cfg, "data/thetis.redb", "clobbered").unwrap_err();
        assert!(format!("{write:#}").contains("protected"), "{write:#}");

        let delete = delete_path(&cfg, "data", true).unwrap_err();
        assert!(format!("{delete:#}").contains("protected"), "{delete:#}");

        // The database is still intact.
        assert_eq!(read_file(&cfg, "data/thetis.redb").unwrap(), "state");
    }

    #[test]
    fn a_root_itself_cannot_be_deleted() {
        let (cfg, _d) = fixture();
        let err = delete_path(&cfg, ".", true).unwrap_err();
        assert!(format!("{err:#}").contains("configured root"), "{err:#}");
    }

    #[test]
    fn a_non_empty_directory_needs_recursive() {
        let (cfg, _d) = fixture();
        let err = delete_path(&cfg, "src", false).unwrap_err();
        assert!(format!("{err:#}").contains("recursive"), "{err:#}");

        delete_path(&cfg, "src", true).unwrap();
        assert!(read_file(&cfg, "src/main.rs").is_err());
    }

    #[test]
    fn listing_puts_directories_first() {
        let (cfg, _d) = fixture();
        let entries = list_dir(&cfg, ".").unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["data", "src"]);
        assert!(entries[0].is_dir);
    }

    #[test]
    fn everything_is_refused_when_turned_off() {
        let (mut cfg, _d) = fixture();
        cfg.filesystem.enabled = false;

        for result in [
            read_file(&cfg, "src/main.rs").err(),
            write_file(&cfg, "x.txt", "x").err(),
            delete_path(&cfg, "src/main.rs", false).err(),
            list_dir(&cfg, ".").err(),
        ] {
            let err = result.expect("should be refused");
            assert!(format!("{err:#}").contains("filesystem access is off"));
        }
    }

    #[test]
    fn deleting_can_be_turned_off_on_its_own() {
        let (mut cfg, _d) = fixture();
        cfg.filesystem.allow_delete = false;

        let err = delete_path(&cfg, "src/main.rs", false).unwrap_err();
        assert!(format!("{err:#}").contains("deleting is off"), "{err:#}");
        // Reading and writing still work.
        assert!(read_file(&cfg, "src/main.rs").is_ok());
    }

    #[test]
    fn a_read_over_the_limit_is_refused_rather_than_truncated() {
        let (mut cfg, _d) = fixture();
        cfg.filesystem.max_read_bytes = 4;
        let err = read_file(&cfg, "src/main.rs").unwrap_err();
        assert!(format!("{err:#}").contains("read limit"), "{err:#}");
    }
}
