//! Read files and directory listings from a repository at any ref.
//!
//! `GET /repos/{owner}/{repo}/contents/{path}` returns either a file (base64
//! encoded) or a directory listing, depending on what the path points at. This
//! tool handles both, and decodes the file so a model gets text rather than
//! base64.
//!
//! Writing lives in github-commit, which can change several files atomically.
//! Splitting them keeps this one read-only, so it survives a read-only mode.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod github;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use github::{parse_repo, GitHub, MAX_OUTPUT_CHARS};
use serde_json::{json, Value};

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "github-file".to_string(),
            description: "Read a file's contents, or list a directory, from a GitHub repository \
                          at any branch, tag or commit — without cloning it. Use github-commit \
                          to change files, and github-repo to find repositories and branches."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "repo": {
                        "type": "string",
                        "description": "Repository as `owner/repo`, or its URL."
                    },
                    "path": {
                        "type": "string",
                        "description": "Path within the repository. Omit or give \"\" for the \
                                        root directory listing."
                    },
                    "ref": {
                        "type": "string",
                        "description": "Branch, tag or commit SHA to read at. Defaults to the \
                                        repository's default branch."
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "First line to return, counting from 1. Use with \
                                        max_lines to window a large file."
                    },
                    "max_lines": {
                        "type": "integer",
                        "description": "How many lines to return from start_line."
                    }
                },
                "required": ["repo"],
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["http".to_string(), "read-only".to_string()],
        }
    }

    fn invoke(_session_id: String, args_json: String, config_json: String) -> Result<String, String> {
        let args: Value = serde_json::from_str(&args_json)
            .map_err(|e| format!("arguments were not valid JSON: {e}"))?;
        let client = GitHub::from_config(&config_json)?;

        let raw_repo = args
            .get("repo")
            .and_then(Value::as_str)
            .ok_or("missing required argument 'repo'")?;
        let (owner, repo) = parse_repo(raw_repo)?;

        let path = args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .trim_start_matches('/')
            .to_string();

        let mut query: Vec<(String, String)> = Vec::new();
        if let Some(reference) = args
            .get("ref")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|r| !r.is_empty())
        {
            query.push(("ref".to_string(), reference.to_string()));
        }

        let response = client.get(
            &format!("/repos/{owner}/{repo}/contents/{path}"),
            &query,
        )?;

        // An array is a directory; an object is a single file.
        match &response {
            Value::Array(entries) => Ok(render_directory(&owner, &repo, &path, entries)),
            _ => render_file(&owner, &repo, &path, &response, &args),
        }
    }
}

fn render_directory(owner: &str, repo: &str, path: &str, entries: &[Value]) -> String {
    let where_ = if path.is_empty() {
        format!("{owner}/{repo}")
    } else {
        format!("{owner}/{repo}/{path}")
    };

    if entries.is_empty() {
        return format!("{where_} is an empty directory.\n");
    }

    // Directories first, then files, each alphabetically — a stable order, so
    // the same listing renders identically twice and prompt caching holds.
    let mut dirs: Vec<&Value> = Vec::new();
    let mut files: Vec<&Value> = Vec::new();
    for entry in entries {
        match entry.get("type").and_then(Value::as_str) {
            Some("dir") => dirs.push(entry),
            _ => files.push(entry),
        }
    }
    let name_of = |e: &Value| e.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    dirs.sort_by_key(|e| name_of(e));
    files.sort_by_key(|e| name_of(e));

    let mut out = format!("{where_} — {} entries\n\n", entries.len());
    for dir in dirs {
        out.push_str(&format!("  {}/\n", name_of(dir)));
    }
    for file in files {
        let size = file.get("size").and_then(Value::as_u64).unwrap_or(0);
        out.push_str(&format!("  {}  ({})\n", name_of(file), human_size(size)));
    }
    out
}

fn render_file(
    owner: &str,
    repo: &str,
    path: &str,
    file: &Value,
    args: &Value,
) -> Result<String, String> {
    let size = file.get("size").and_then(Value::as_u64).unwrap_or(0);
    let sha = file.get("sha").and_then(Value::as_str).unwrap_or("");

    // Above 1 MB GitHub returns metadata with an empty body and expects the
    // blob API instead. Say so rather than showing an empty file.
    let encoded = file.get("content").and_then(Value::as_str).unwrap_or("");
    if encoded.trim().is_empty() {
        return Ok(format!(
            "{owner}/{repo}/{path} is {} and too large for the contents API to \
             inline (its limit is 1 MB). sha: {sha}\n",
            human_size(size)
        ));
    }

    // GitHub wraps the base64 at 60 columns, which the decoder rejects.
    let cleaned: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = STANDARD
        .decode(cleaned)
        .map_err(|e| format!("could not decode {path}: {e}"))?;

    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            return Ok(format!(
                "{owner}/{repo}/{path} is binary ({}). sha: {sha}\n",
                human_size(size)
            ))
        }
    };

    let start = args
        .get("start_line")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1) as usize;
    let max_lines = args.get("max_lines").and_then(Value::as_u64).map(|n| n as usize);

    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let end = match max_lines {
        Some(n) => (start - 1 + n).min(total),
        None => total,
    };

    if start > total {
        return Ok(format!(
            "{owner}/{repo}/{path} has only {total} lines; start_line {start} is past the end.\n"
        ));
    }

    let mut body = String::new();
    for (offset, line) in lines[start - 1..end].iter().enumerate() {
        body.push_str(&format!("{:>6}\t{line}\n", start + offset));
        // Numbered lines are what make a follow-up edit anchorable, and the
        // cost is bounded here rather than at the host's 32 KiB cut.
        if body.chars().count() > MAX_OUTPUT_CHARS {
            body.push_str(&format!(
                "\n[cut at the output limit — {} lines remain. Use start_line and max_lines \
                 to read on.]\n",
                total - (start + offset)
            ));
            break;
        }
    }

    let mut out = format!(
        "{owner}/{repo}/{path}  ({}, {total} lines)\n",
        human_size(size)
    );
    if end < total || start > 1 {
        out.push_str(&format!("showing lines {start}-{end}\n"));
    }
    out.push('\n');
    out.push_str(&body);
    Ok(out)
}

fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

export!(Component);
