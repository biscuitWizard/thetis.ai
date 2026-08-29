//! The shared workspace, as a protocol the browser can speak.
//!
//! `workspace-*` frames are handled here, in the gateway process, before the
//! UI guest ever sees them — for the same reason `branch_api` is: the gateway
//! component's world imports `sys`, `session` and `skills-view` and nothing
//! else, so it has no way to touch a file. The filesystem lives host-side.
//!
//! What this exposes is `wasi.dirs[0]` — the one directory every guest gets as
//! a preopen, shared by every conversation and every branch. That is the point
//! of it: it is the agents' common ground, and until now a human had no way to
//! look inside. Exposing it grants no guest any authority it did not already
//! have; it only lets the operator see and edit what the agents can already
//! see and edit.
//!
//! Two transports, chosen by what the payload is:
//!
//! * **WebSocket frames** for listing, text reads, text writes and the
//!   structural operations. These are small and belong in the same ordered
//!   stream as everything else the UI does.
//! * **HTTP** `GET`/`PUT /workspace/file/{path}` for raw bytes, so an image
//!   previews from a normal `<img>` tag, a download is a normal link, and an
//!   upload is the `File` object streamed straight from the browser. Pushing
//!   binaries through base64 in a JSON frame would cost a third more bytes and
//!   would have to be buffered twice.
//!
//! Every path from either transport goes through `resolve`, which is the whole
//! security boundary: normalise away `..`, join to the workspace root, then
//! confirm the result is still inside it *after* symlinks are followed.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::config::Config;

/// Largest file served as inline text in a frame. Past this the UI is told the
/// size and offers the download link instead of pretending to show it.
const MAX_TEXT_BYTES: u64 = 2 * 1024 * 1024;
/// Largest single upload accepted over HTTP.
pub const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;
/// Entries returned for one directory. A directory with more than this is
/// pathological, and truncating beats sending a frame nothing can render.
const MAX_ENTRIES: usize = 5_000;

/// True when this frame type belongs to the workspace protocol.
pub fn handles(frame_type: &str) -> bool {
    frame_type.starts_with("workspace-")
}

// --- the path index, for @-mention search ------------------------------------

/// Entries held in the search index. The real shared workspace here is tens of
/// thousands of files across several checkouts, so this is a ceiling against a
/// pathological tree rather than a limit anyone should meet.
const MAX_INDEX_ENTRIES: usize = 400_000;
/// Depth walked while indexing.
const MAX_INDEX_DEPTH: usize = 20;
/// How long an index is served before the next search rebuilds it. The agents
/// are writing into this directory continuously, so a stale menu is a real
/// hazard — but rebuilding per keystroke would walk 46k paths per character.
const INDEX_TTL: std::time::Duration = std::time::Duration::from_secs(45);
/// Matches returned for one search.
const MAX_MATCHES: usize = 40;

/// Directory names never walked. Every one of these is either machine-generated
/// or a vendored dependency tree: `moor/target` alone is larger than every
/// hand-written file in the workspace put together, and offering its contents in
/// a mention menu would bury the files someone actually meant.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    ".cargo",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".gradle",
    ".idea",
    ".terraform",
    "dist-newstyle",
    ".stack-work",
];

#[derive(Clone)]
struct IndexEntry {
    path: String,
    name_lower: String,
    path_lower: String,
    is_dir: bool,
    size: u64,
}

struct Index {
    root: PathBuf,
    built: std::time::Instant,
    entries: Vec<IndexEntry>,
    truncated: bool,
}

fn index_cell() -> &'static std::sync::Mutex<Option<std::sync::Arc<Index>>> {
    static CELL: std::sync::OnceLock<std::sync::Mutex<Option<std::sync::Arc<Index>>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| std::sync::Mutex::new(None))
}

/// Drops the cached index, so the next search rebuilds it. Called after any
/// mutation through this module, and by the HTTP upload route in `web.rs`,
/// which writes files without passing through `dispatch`.
pub fn invalidate_index() {
    if let Ok(mut slot) = index_cell().lock() {
        *slot = None;
    }
}

/// The path index, rebuilt when missing, stale, or pointing at another root.
fn path_index(cfg: &Config) -> Result<std::sync::Arc<Index>> {
    let root = root(cfg).map(|r| canonical(&r))?;

    if let Ok(slot) = index_cell().lock() {
        if let Some(index) = slot.as_ref() {
            if index.root == root && index.built.elapsed() < INDEX_TTL {
                return Ok(index.clone());
            }
        }
    }

    let index = std::sync::Arc::new(build_index(&root));
    if let Ok(mut slot) = index_cell().lock() {
        *slot = Some(index.clone());
    }
    Ok(index)
}

/// Breadth-first walk of the workspace, skipping the generated trees.
///
/// Breadth-first on purpose: if the ceiling is ever hit, what survives is the
/// shallow paths, which are the ones a person is likely to mean.
fn build_index(root: &Path) -> Index {
    let mut entries: Vec<IndexEntry> = Vec::new();
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> =
        std::collections::VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut truncated = false;

    while let Some((dir, depth)) = queue.pop_front() {
        if entries.len() >= MAX_INDEX_ENTRIES {
            truncated = true;
            break;
        }
        let Ok(reader) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in reader.flatten() {
            if entries.len() >= MAX_INDEX_ENTRIES {
                truncated = true;
                break;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            // `file_type` does not follow the link, which is what we want: a
            // symlink is listed but never descended into, since its target may
            // sit outside the workspace where opening it is refused anyway.
            let file_type = entry.file_type().ok();
            let is_link = file_type.as_ref().is_some_and(|t| t.is_symlink());
            let is_dir = file_type.as_ref().is_some_and(|t| t.is_dir());
            let size = if is_dir {
                0
            } else {
                entry.metadata().map(|m| m.len()).unwrap_or(0)
            };
            let path = relative(root, &entry.path());

            if is_dir && SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            entries.push(IndexEntry {
                name_lower: name.to_lowercase(),
                path_lower: path.to_lowercase(),
                path,
                is_dir,
                size,
            });
            if is_dir && !is_link && depth + 1 < MAX_INDEX_DEPTH {
                queue.push_back((entry.path(), depth + 1));
            }
        }
    }

    Index {
        root: root.to_path_buf(),
        built: std::time::Instant::now(),
        entries,
        truncated,
    }
}

/// How well an entry answers a query, or `None` for no match.
///
/// Ordered so that what was meant comes first: an exact name, then a name that
/// starts with the query, then a path prefix, then a substring, and finally a
/// subsequence — which is what makes "gwcomp" find `gateway-web/src/composer`.
/// Shallow paths win ties, and files beat directories, since attaching a file
/// is the common case.
fn score(entry: &IndexEntry, query: &str) -> Option<i32> {
    if query.is_empty() {
        // An `@` with nothing after it: offer the top of the tree rather than
        // an arbitrary forty thousand files.
        let depth = entry.path.split('/').count() as i32;
        return if depth <= 2 {
            Some(60 - depth * 4 + if entry.is_dir { 2 } else { 0 })
        } else {
            None
        };
    }

    let base = if entry.name_lower == query {
        1000
    } else if entry.name_lower.starts_with(query) {
        900
    } else if entry.path_lower.starts_with(query) {
        820
    } else if entry.name_lower.contains(query) {
        700
    } else if entry.path_lower.contains(query) {
        600
    } else if subsequence(&entry.path_lower, query) {
        400
    } else {
        return None;
    };

    let depth = entry.path.split('/').count() as i32;
    let length = entry.path.chars().count() as i32;
    Some(base - depth * 6 - length / 8 + if entry.is_dir { -4 } else { 0 })
}

/// True when every character of `needle` appears in `haystack` in order. The
/// query is lowercased by the caller, as is the haystack.
fn subsequence(haystack: &str, needle: &str) -> bool {
    let mut chars = needle.chars();
    let mut want = chars.next();
    for got in haystack.chars() {
        match want {
            Some(c) if c == got => want = chars.next(),
            Some(_) => {}
            None => return true,
        }
    }
    want.is_none()
}

/// Fuzzy path search for the composer's `@` menu.
///
/// This exists because the workspace is far too large to index in the browser:
/// crawling it over `workspace-list` frames would be thousands of round trips
/// and would still have to truncate. One frame, one bounded answer, and the
/// walk is cached here where every tab shares it.
fn find(cfg: &Config, query_raw: &str, dir: &str) -> Result<Value> {
    let index = path_index(cfg)?;
    let query = query_raw.trim().to_lowercase();

    // A query with a slash in it is a path fragment, so the whole path is what
    // it should be matched against; without one, matching the name first is
    // what makes short queries useful.
    let prefix = dir.trim_matches('/').to_lowercase();

    let mut scored: Vec<(i32, &IndexEntry)> = index
        .entries
        .iter()
        .filter(|entry| prefix.is_empty() || entry.path_lower.starts_with(&format!("{prefix}/")))
        .filter_map(|entry| score(entry, &query).map(|s| (s, entry)))
        .collect();

    // Descending score, then the shorter path, then alphabetical — so the order
    // is stable between keystrokes and the list does not jitter.
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(a.1.path.len().cmp(&b.1.path.len()))
            .then(a.1.path.cmp(&b.1.path))
    });

    let total = scored.len();
    let entries: Vec<Value> = scored
        .into_iter()
        .take(MAX_MATCHES)
        .map(|(_, entry)| {
            json!({
                "path": entry.path,
                "name": entry.path.rsplit('/').next().unwrap_or(&entry.path),
                "is_dir": entry.is_dir,
                "size": entry.size,
                "kind": kind_of(&entry.path, entry.is_dir),
            })
        })
        .collect();

    Ok(json!({
        "type": "workspace-find",
        // Echoed so a client can discard an answer to a query it has typed
        // past, which is most of them while someone is still typing.
        "query": query_raw,
        "dir": dir,
        "entries": entries,
        "total": total,
        "indexed": index.entries.len(),
        "truncated": index.truncated,
    }))
}

// --- path handling ----------------------------------------------------------

/// The shared workspace directory: the first WASI preopen, which is what every
/// guest sees as `/workspace`.
pub fn root(cfg: &Config) -> Result<PathBuf> {
    cfg
        .wasi
        .dirs
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("no workspace directory is configured (wasi.dirs is empty)"))
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

/// Turns a client-supplied relative path into an absolute one inside the
/// workspace, or an error.
///
/// This is the security boundary. Absolute paths and drive prefixes are
/// refused outright rather than reinterpreted, `..` is normalised away before
/// the join, and the result is compared against the root in whichever form
/// exists on disk — canonicalised when it exists, so a symlink cannot be used
/// to step outside; via its parent when it is about to be created.
pub fn resolve(cfg: &Config, raw: &str) -> Result<PathBuf> {
    let root = root(cfg)?;
    let root_real = canonical(&root);

    let raw_trimmed = raw.trim();

    // The workspace itself, however it is spelled.
    if matches!(raw_trimmed, "" | "." | "/" | "workspace" | "workspace/") {
        return Ok(root_real);
    }
    // Checked before any slash is stripped, or this could never fire: refuse
    // an absolute path rather than reinterpreting it, because reading
    // "/etc/passwd" as "workspace/etc/passwd" answers a question nobody asked.
    if crate::hostfs::has_drive_prefix(raw_trimmed) || Path::new(raw_trimmed).is_absolute() {
        return Err(anyhow!("'{raw}' is not a path inside the workspace"));
    }
    // The UI shows the root as "workspace/…"; accept that spelling back.
    let trimmed = raw_trimmed
        .strip_prefix("workspace/")
        .unwrap_or(raw_trimmed)
        .trim_start_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        return Ok(root_real);
    }
    if trimmed.split(['/', '\\']).any(|part| part == "..") {
        // Normalising would silently accept "a/../../etc"; refusing is clearer
        // than quietly resolving to something the user did not type.
        return Err(anyhow!("'{raw}' leaves the workspace"));
    }

    let joined = normalise(&root_real.join(trimmed));
    let probe = if joined.exists() {
        canonical(&joined)
    } else {
        match joined.parent() {
            Some(parent) if parent.exists() => canonical(parent).join(
                joined
                    .file_name()
                    .map(Path::new)
                    .unwrap_or_else(|| Path::new("")),
            ),
            _ => joined.clone(),
        }
    };

    if probe != root_real && !probe.starts_with(&root_real) {
        return Err(anyhow!("'{raw}' is outside the workspace"));
    }
    Ok(probe)
}

/// The path as the client speaks it: relative to the workspace root, with
/// forward slashes, empty for the root itself.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

/// The parent of a client path, or none when it is already the root.
fn parent_of(rel: &str) -> Option<String> {
    let rel = rel.trim_matches('/');
    if rel.is_empty() {
        return None;
    }
    Some(match rel.rsplit_once('/') {
        Some((head, _)) => head.to_string(),
        None => String::new(),
    })
}

fn modified_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A coarse kind, so the UI can pick an icon and decide how to preview.
///
/// Extension-based on purpose: the alternative is reading the head of every
/// file in a directory listing, and being wrong about an icon costs nothing.
fn kind_of(name: &str, is_dir: bool) -> &'static str {
    if is_dir {
        return "dir";
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref().unwrap_or("") {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "ico" | "avif" => "image",
        "pdf" => "pdf",
        "mp3" | "wav" | "ogg" | "flac" | "m4a" => "audio",
        "mp4" | "webm" | "mov" | "mkv" => "video",
        "zip" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "tar" | "7z" | "rar" => "archive",
        "rs" | "py" | "js" | "mjs" | "ts" | "jsx" | "tsx" | "go" | "rb" | "sh" | "bash" | "c"
        | "h" | "cpp" | "hpp" | "java" | "kt" | "swift" | "php" | "lua" | "sql" | "wit" => "code",
        "json" | "toml" | "yaml" | "yml" | "xml" | "ini" | "csv" | "tsv" | "lock" => "data",
        "md" | "markdown" | "txt" | "rst" | "log" => "text",
        "" => "text",
        _ => "file",
    }
}

/// Content type for the raw HTTP route. Text types carry a charset so a
/// preview in a new tab renders rather than downloading.
pub fn mime_of(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref().unwrap_or("") {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "json" => "application/json; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        _ => "text/plain; charset=utf-8",
    }
}

/// True for file types a browser will execute as a document if it renders them
/// inline (scripts run, same origin). These are forced to download rather than
/// previewed, so an uploaded page cannot become stored XSS on the gateway.
pub fn is_active_content(name: &str) -> bool {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    matches!(
        ext.as_deref().unwrap_or(""),
        "html" | "htm" | "xhtml" | "svg" | "xml" | "xsl" | "mathml"
    )
}

// --- operations --------------------------------------------------------------

/// One directory, as the explorer draws it.
fn list(cfg: &Config, rel_request: &str) -> Result<Value> {
    let root = root(cfg).map(|r| canonical(&r))?;
    let dir = resolve(cfg, rel_request)?;

    if !dir.exists() {
        // A folder the agents deleted under an open explorer. Say so plainly
        // rather than returning an empty listing that looks like an empty
        // folder.
        return Err(anyhow!(
            "'{}' no longer exists",
            relative(&root, &dir)
        ));
    }
    if !dir.is_dir() {
        return Err(anyhow!("'{}' is a file", relative(&root, &dir)));
    }

    let mut entries: Vec<Value> = Vec::new();
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut truncated = false;

    for entry in std::fs::read_dir(&dir)
        .with_context(|| format!("cannot list {}", relative(&root, &dir)))?
        .flatten()
    {
        if entries.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_dir {
            files += 1;
            bytes += size;
        }
        entries.push(json!({
            "name": name,
            "path": relative(&root, &entry.path()),
            "is_dir": is_dir,
            "size": size,
            "modified_ms": meta.as_ref().map(modified_ms).unwrap_or(0),
            "kind": kind_of(&name, is_dir),
            // A symlink is followed for reading but is worth flagging: it is
            // the one entry whose target may sit outside the workspace, in
            // which case opening it will be refused.
            "link": entry.file_type().map(|t| t.is_symlink()).unwrap_or(false),
        }));
    }

    // Directories first, then case-insensitively by name: the order someone
    // reading a listing expects.
    entries.sort_by(|a, b| {
        let dir_a = a["is_dir"].as_bool().unwrap_or(false);
        let dir_b = b["is_dir"].as_bool().unwrap_or(false);
        let name_a = a["name"].as_str().unwrap_or("").to_lowercase();
        let name_b = b["name"].as_str().unwrap_or("").to_lowercase();
        dir_b.cmp(&dir_a).then(name_a.cmp(&name_b))
    });

    let rel = relative(&root, &dir);
    Ok(json!({
        "type": "workspace-list",
        "path": rel,
        "parent": parent_of(&rel),
        "root": root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "workspace".into()),
        "entries": entries,
        "files": files,
        "bytes": bytes,
        "truncated": truncated,
    }))
}

/// One file, for the preview pane.
///
/// Text comes back inline; anything binary or oversized comes back as
/// metadata plus the raw URL, which is what the `<img>`, `<video>` and
/// download paths use anyway.
fn read(cfg: &Config, rel_request: &str) -> Result<Value> {
    let root = root(cfg).map(|r| canonical(&r))?;
    let path = resolve(cfg, rel_request)?;
    let meta = std::fs::metadata(&path)
        .with_context(|| format!("cannot read {}", relative(&root, &path)))?;
    if meta.is_dir() {
        return Err(anyhow!("'{}' is a directory", relative(&root, &path)));
    }

    let rel = relative(&root, &path);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let kind = kind_of(&name, false);
    let mut frame = json!({
        "type": "workspace-file",
        "path": rel,
        "name": name,
        "size": meta.len(),
        "modified_ms": modified_ms(&meta),
        "kind": kind,
        "mime": mime_of(&name),
        "url": raw_url(&rel),
    });

    let inline_candidate = meta.len() <= MAX_TEXT_BYTES
        && matches!(kind, "text" | "code" | "data" | "file");
    if !inline_candidate {
        frame["text_available"] = json!(false);
        return Ok(frame);
    }

    // UTF-8 decides it, not the extension: a `.log` of protobuf is binary and
    // a file with no extension at all is usually text.
    match std::fs::read(&path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => {
                frame["text_available"] = json!(true);
                frame["text"] = json!(text);
            }
            Err(_) => frame["text_available"] = json!(false),
        },
        Err(e) => return Err(anyhow!("cannot read {rel}: {e}")),
    }
    Ok(frame)
}

/// Writes text, creating parent directories. Used by the editor and by "new
/// file"; uploads take the HTTP route instead.
fn write(cfg: &Config, rel_request: &str, text: &str) -> Result<String> {
    let root = root(cfg).map(|r| canonical(&r))?;
    let path = resolve(cfg, rel_request)?;
    if path == root {
        return Err(anyhow!("that is the workspace itself, not a file"));
    }
    if path.is_dir() {
        return Err(anyhow!("'{}' is a directory", relative(&root, &path)));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", relative(&root, parent)))?;
    }
    std::fs::write(&path, text)
        .with_context(|| format!("cannot write {}", relative(&root, &path)))?;
    Ok(format!(
        "saved {} ({})",
        relative(&root, &path),
        human_bytes(text.len() as u64)
    ))
}

fn mkdir(cfg: &Config, rel_request: &str) -> Result<String> {
    let root = root(cfg).map(|r| canonical(&r))?;
    let path = resolve(cfg, rel_request)?;
    if path == root {
        return Err(anyhow!("a folder needs a name"));
    }
    if path.exists() {
        return Err(anyhow!("'{}' already exists", relative(&root, &path)));
    }
    std::fs::create_dir_all(&path)
        .with_context(|| format!("cannot create {}", relative(&root, &path)))?;
    Ok(format!("created {}/", relative(&root, &path)))
}

fn delete(cfg: &Config, rel_request: &str, recursive: bool) -> Result<String> {
    let root = root(cfg).map(|r| canonical(&r))?;
    let path = resolve(cfg, rel_request)?;
    // Deleting the root would take the shared workspace with it.
    if path == root {
        return Err(anyhow!("refusing to delete the workspace itself"));
    }
    if !path.exists() {
        return Err(anyhow!("'{}' does not exist", relative(&root, &path)));
    }

    let rel = relative(&root, &path);
    if path.is_dir() {
        let count = std::fs::read_dir(&path).map(|e| e.count()).unwrap_or(0);
        if count > 0 && !recursive {
            return Err(anyhow!("'{rel}' holds {count} entries; confirm to delete it"));
        }
        std::fs::remove_dir_all(&path).with_context(|| format!("cannot delete {rel}"))?;
        Ok(format!("deleted {rel}/"))
    } else {
        std::fs::remove_file(&path).with_context(|| format!("cannot delete {rel}"))?;
        Ok(format!("deleted {rel}"))
    }
}

/// Rename or move. Refuses to clobber, because a rename in a file explorer
/// that silently ate the destination would be a data-loss bug.
fn rename(cfg: &Config, from_raw: &str, to_raw: &str) -> Result<String> {
    let root = root(cfg).map(|r| canonical(&r))?;
    let from = resolve(cfg, from_raw)?;
    let to = resolve(cfg, to_raw)?;
    if from == root || to == root {
        return Err(anyhow!("the workspace itself cannot be renamed"));
    }
    if !from.exists() {
        return Err(anyhow!("'{}' does not exist", relative(&root, &from)));
    }
    if to.exists() {
        return Err(anyhow!("'{}' already exists", relative(&root, &to)));
    }
    // Moving a directory into itself would detach the subtree.
    if to.starts_with(&from) {
        return Err(anyhow!("a folder cannot be moved inside itself"));
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", relative(&root, parent)))?;
    }
    std::fs::rename(&from, &to).with_context(|| {
        format!(
            "cannot move {} to {}",
            relative(&root, &from),
            relative(&root, &to)
        )
    })?;
    Ok(format!(
        "moved {} to {}",
        relative(&root, &from),
        relative(&root, &to)
    ))
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} {}", UNITS[0])
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// The raw-bytes URL for a path, percent-encoded a segment at a time so a
/// name containing a space or a `#` still resolves.
pub fn raw_url(rel: &str) -> String {
    let encoded: Vec<String> = rel
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|segment| {
            segment
                .bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        (b as char).to_string()
                    }
                    other => format!("%{other:02X}"),
                })
                .collect::<String>()
        })
        .collect();
    format!("/workspace/file/{}", encoded.join("/"))
}

// --- frame dispatch ----------------------------------------------------------

/// Handles one frame, returning the replies to send on this socket.
pub async fn handle(cfg: &Config, frame: &Value) -> Vec<String> {
    let frame_type = frame
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // Every arm of `dispatch` is synchronous filesystem work — a directory
    // walk, a whole-file read, a recursive delete — driven straight from a
    // websocket frame. Inline it holds a runtime thread for its duration.
    match crate::offload::blocking(|| dispatch(cfg, &frame_type, frame)) {
        Ok(replies) => replies,
        Err(e) => vec![json!({
            "type": "workspace-result",
            "op": frame_type.trim_start_matches("workspace-"),
            "ok": false,
            "path": frame.get("path").and_then(Value::as_str).unwrap_or_default(),
            "message": format!("{e:#}"),
        })
        .to_string()],
    }
}

fn dispatch(cfg: &Config, frame_type: &str, frame: &Value) -> Result<Vec<String>> {
    let path = frame
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    match frame_type {
        "workspace-list" => Ok(vec![list(cfg, &path)?.to_string()]),
        "workspace-read" => Ok(vec![read(cfg, &path)?.to_string()]),

        // Fuzzy path search, for the composer's `@` menu. Read-only, and served
        // from a cached walk — see `find`.
        "workspace-find" => {
            let query = frame.get("query").and_then(Value::as_str).unwrap_or("");
            let dir = frame.get("dir").and_then(Value::as_str).unwrap_or("");
            Ok(vec![find(cfg, query, dir)?.to_string()])
        }

        // Every mutation answers with its own result *and* a fresh listing of
        // the directory it touched, so the explorer never has to guess what
        // changed or fire a second round trip to find out.
        "workspace-write" => {
            let text = frame.get("text").and_then(Value::as_str).unwrap_or("");
            let message = write(cfg, &path, text)?;
            Ok(mutation_replies(cfg, "write", &path, &message))
        }
        "workspace-mkdir" => {
            let message = mkdir(cfg, &path)?;
            Ok(mutation_replies(cfg, "mkdir", &path, &message))
        }
        "workspace-delete" => {
            let recursive = frame
                .get("recursive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let message = delete(cfg, &path, recursive)?;
            Ok(mutation_replies(cfg, "delete", &path, &message))
        }
        "workspace-move" => {
            let from = frame
                .get("from")
                .and_then(Value::as_str)
                .context("missing 'from'")?;
            let to = frame
                .get("to")
                .and_then(Value::as_str)
                .context("missing 'to'")?;
            let message = rename(cfg, from, to)?;
            Ok(mutation_replies(cfg, "move", to, &message))
        }

        other => anyhow::bail!("unknown workspace frame '{other}'"),
    }
}

/// The result frame, plus a relisting of the affected directory.
fn mutation_replies(
    cfg: &Config,
    op: &str,
    path: &str,
    message: &str,
) -> Vec<String> {
    // The tree just changed, so the cached path index is a liar. Cheaper to
    // drop it than to work out what moved.
    invalidate_index();

    let mut replies = vec![json!({
        "type": "workspace-result",
        "op": op,
        "ok": true,
        "path": path,
        "message": message,
    })
    .to_string()];

    // The directory to redraw is the parent of whatever was touched — except
    // for a delete of a directory, whose parent is also its own parent.
    let dir = parent_of(path).unwrap_or_default();
    if let Ok(listing) = list(cfg, &dir) {
        replies.push(listing.to_string());
    }
    replies
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `resolve` and the operations only read `grip.cfg.wasi.dirs`, so the
    /// path logic is tested against a config rather than a whole grip.
    fn fixture() -> (Config, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let ws = canonical(dir.path()).join("workspace");
        std::fs::create_dir_all(ws.join("notes")).unwrap();
        std::fs::write(ws.join("notes/todo.md"), "- ship it").unwrap();
        std::fs::write(ws.join("data.bin"), [0u8, 159, 146, 150]).unwrap();

        let mut cfg = Config::load().unwrap();
        cfg.wasi.dirs = vec![ws];
        (cfg, dir)
    }

    #[test]
    fn resolves_inside_the_workspace() {
        let (cfg, _d) = fixture();
        let root = canonical(&root(&cfg).unwrap());

        assert_eq!(resolve(&cfg, "").unwrap(), root);
        assert_eq!(resolve(&cfg, "/").unwrap(), root);
        assert_eq!(resolve(&cfg, "workspace").unwrap(), root);
        assert_eq!(resolve(&cfg, "notes/todo.md").unwrap(), root.join("notes/todo.md"));
        // The UI spells the root as "workspace/…"; that round-trips.
        assert_eq!(
            resolve(&cfg, "workspace/notes/todo.md").unwrap(),
            root.join("notes/todo.md")
        );
    }

    #[test]
    fn refuses_to_leave_the_workspace() {
        let (cfg, _d) = fixture();
        for bad in [
            "..",
            "../secret",
            "notes/../../secret",
            "/etc/passwd",
            "C:/Windows/System32/drivers/etc/hosts",
        ] {
            let err = resolve(&cfg, bad).unwrap_err();
            let text = format!("{err:#}");
            assert!(
                text.contains("workspace"),
                "{bad} gave: {text}"
            );
        }
    }

    #[test]
    fn a_symlink_cannot_be_used_to_escape() {
        let (cfg, dir) = fixture();
        let outside = canonical(dir.path()).join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        let link = root(&cfg).unwrap().join("escape");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let err = resolve(&cfg, "escape").unwrap_err();
        assert!(format!("{err:#}").contains("outside the workspace"), "{err:#}");
    }

    #[test]
    fn listing_puts_directories_first_and_counts_bytes() {
        let (cfg, _d) = fixture();
        let listing = list(&cfg, "").unwrap();
        let names: Vec<&str> = listing["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["notes", "data.bin"]);
        assert_eq!(listing["entries"][0]["kind"], "dir");
        assert_eq!(listing["files"], 1);
        assert_eq!(listing["bytes"], 4);
        assert_eq!(listing["parent"], Value::Null);
        assert_eq!(listing["path"], "");
    }

    #[test]
    fn a_subdirectory_knows_its_parent() {
        let (cfg, _d) = fixture();
        let listing = list(&cfg, "notes").unwrap();
        assert_eq!(listing["path"], "notes");
        assert_eq!(listing["parent"], "");
    }

    #[test]
    fn text_comes_back_inline_and_binary_does_not() {
        let (cfg, _d) = fixture();

        let text = read(&cfg, "notes/todo.md").unwrap();
        assert_eq!(text["text"], "- ship it");
        assert_eq!(text["text_available"], true);
        assert_eq!(text["url"], "/workspace/file/notes/todo.md");

        // Invalid UTF-8 is binary whatever the extension says.
        let binary = read(&cfg, "data.bin").unwrap();
        assert_eq!(binary["text_available"], false);
        assert!(binary["text"].is_null());
    }

    #[test]
    fn the_full_crud_cycle_works() {
        let (cfg, _d) = fixture();

        write(&cfg, "a/b/new.txt", "hello").unwrap();
        assert_eq!(read(&cfg, "a/b/new.txt").unwrap()["text"], "hello");

        write(&cfg, "a/b/new.txt", "hello again").unwrap();
        assert_eq!(read(&cfg, "a/b/new.txt").unwrap()["text"], "hello again");

        rename(&cfg, "a/b/new.txt", "a/renamed.txt").unwrap();
        assert!(read(&cfg, "a/b/new.txt").is_err());
        assert_eq!(read(&cfg, "a/renamed.txt").unwrap()["text"], "hello again");

        mkdir(&cfg, "a/fresh").unwrap();
        assert!(list(&cfg, "a/fresh").is_ok());

        delete(&cfg, "a/renamed.txt", false).unwrap();
        assert!(read(&cfg, "a/renamed.txt").is_err());
        delete(&cfg, "a", true).unwrap();
        assert!(list(&cfg, "a").is_err());
    }

    #[test]
    fn destructive_mistakes_are_refused() {
        let (cfg, _d) = fixture();

        // The workspace root itself.
        assert!(format!("{:#}", delete(&cfg, "", true).unwrap_err()).contains("refusing"));
        // A non-empty directory without confirmation.
        assert!(format!("{:#}", delete(&cfg, "notes", false).unwrap_err()).contains("confirm"));
        // Clobbering by rename.
        write(&cfg, "other.md", "x").unwrap();
        assert!(format!("{:#}", rename(&cfg, "other.md", "notes/todo.md").unwrap_err())
            .contains("already exists"));
        // A folder into itself.
        assert!(format!("{:#}", rename(&cfg, "notes", "notes/inner").unwrap_err())
            .contains("inside itself"));
        // An existing directory as a new folder.
        assert!(format!("{:#}", mkdir(&cfg, "notes").unwrap_err()).contains("already exists"));
    }

    #[test]
    fn active_content_is_flagged_for_forced_download() {
        for name in ["evil.html", "page.HTM", "vector.svg", "doc.xhtml", "data.xml"] {
            assert!(is_active_content(name), "{name} must be treated as active");
        }
        for name in ["notes.md", "photo.png", "data.json", "script.js", "plain.txt", "noext"] {
            assert!(!is_active_content(name), "{name} may preview inline");
        }
    }

    #[test]
    fn urls_encode_awkward_names() {
        assert_eq!(raw_url("a b/c#d.txt"), "/workspace/file/a%20b/c%23d.txt");
        assert_eq!(raw_url("plain.md"), "/workspace/file/plain.md");
    }

    fn paths(frame: &Value) -> Vec<String> {
        frame["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap().to_string())
            .collect()
    }

    /* The `@`-mention search, all in one test on purpose: the path index is a
     * process-wide cache keyed on the workspace root, so two tests with two
     * temp roots running in parallel would each invalidate the other's index
     * and the assertions would depend on the interleaving. */
    #[test]
    fn mention_search_finds_what_was_meant() {
        let (cfg, _d) = fixture();
        let root = canonical(&root(&cfg).unwrap());
        std::fs::create_dir_all(root.join("moor/crates/kernel/src")).unwrap();
        std::fs::write(root.join("moor/crates/kernel/src/vm.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("moor/README.md"), "# moor").unwrap();
        // The generated trees a mention menu must never offer.
        std::fs::create_dir_all(root.join("moor/target/debug")).unwrap();
        std::fs::write(root.join("moor/target/debug/vm.rs"), "generated").unwrap();
        std::fs::create_dir_all(root.join("moor/.git")).unwrap();
        std::fs::write(root.join("moor/.git/config"), "[core]").unwrap();
        invalidate_index();

        // An exact name beats everything, and `target/` is not offered at all.
        let found = find(&cfg, "vm.rs", "").unwrap();
        assert_eq!(paths(&found), vec!["moor/crates/kernel/src/vm.rs"]);

        // Skipped trees are absent even when asked for by name.
        assert!(paths(&find(&cfg, "config", "").unwrap()).is_empty());
        assert!(paths(&find(&cfg, "debug", "").unwrap()).is_empty());

        // A subsequence over the whole path: "mckvm" for moor/crates/kernel/…
        assert!(paths(&find(&cfg, "mckvm", "").unwrap())
            .contains(&"moor/crates/kernel/src/vm.rs".to_string()));

        // A directory prefix narrows the search to inside it.
        let inside = find(&cfg, "md", "moor").unwrap();
        assert_eq!(paths(&inside), vec!["moor/README.md"]);
        assert!(paths(&find(&cfg, "todo", "moor").unwrap()).is_empty());

        // An empty query offers the top of the tree rather than everything.
        let top = find(&cfg, "", "").unwrap();
        let top_paths = paths(&top);
        assert!(top_paths.contains(&"moor".to_string()));
        assert!(top_paths.contains(&"notes".to_string()));
        assert!(!top_paths.iter().any(|p| p.split('/').count() > 2));

        // The frame echoes the query, which is what lets the client drop an
        // answer to something already typed past.
        assert_eq!(top["query"], json!(""));
        assert_eq!(inside["dir"], json!("moor"));

        // A mutation drops the cached index, so a new file is findable at once
        // rather than after the TTL.
        write(&cfg, "moor/fresh.txt", "hello").unwrap();
        mutation_replies(&cfg, "write", "moor/fresh.txt", "saved");
        assert_eq!(
            paths(&find(&cfg, "fresh.txt", "").unwrap()),
            vec!["moor/fresh.txt"]
        );
    }

    #[test]
    fn subsequence_matches_in_order_only() {
        assert!(subsequence("gateway-web/src/composer.js", "gwcomp"));
        assert!(subsequence("abc", ""));
        assert!(!subsequence("abc", "cba"));
        assert!(!subsequence("abc", "abcd"));
    }

    #[test]
    fn sizes_read_as_sizes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(20 * 1024), "20 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
