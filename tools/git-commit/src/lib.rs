//! Commit changes to a repository as the app's own `[bot]` identity.
//!
//! # Why the Git Data API rather than the Contents API
//!
//! `PUT /contents/{path}` writes one file per commit. Anything real touches
//! several files at once, and a series of single-file commits leaves the
//! repository broken at every intermediate step. So this builds a commit the
//! way git does:
//!
//! 1. `POST /git/blobs` — one blob per changed file.
//! 2. `POST /git/trees` — a tree based on the current one, with those blobs
//!    patched in. Passing `base_tree` means untouched files are inherited
//!    rather than deleted, which is the difference between a commit and a
//!    catastrophe.
//! 3. `POST /git/commits` — the commit object, pointing at the tree, with the
//!    branch head as its parent.
//! 4. `PATCH /git/refs/heads/{branch}` — move the branch to it.
//!
//! Step 4 is not forced by default. A non-fast-forward push is refused, which
//! is what protects a concurrent change from being silently overwritten.
//!
//! # Authorship
//!
//! Because the request is authenticated as the App installation, GitHub
//! attributes the commit to the App's bot user automatically — no author or
//! committer field is needed, and the commit shows as `slug[bot]` with the
//! app's avatar. Deletion is supported by omitting a file's content.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod github;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use github::{clip, parse_repo, GitHub};
use serde_json::{json, Value};

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "git-commit".to_string(),
            description: "Commit file changes to a GitHub repository — create, update or delete \
                          several files in one atomic commit, authored by the app's own [bot] \
                          identity. Can also open a pull request for the branch it just wrote. \
                          Reads go through git-file."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "repo": {
                        "type": "string",
                        "description": "Repository as `owner/repo`, or its URL."
                    },
                    "branch": {
                        "type": "string",
                        "description": "Branch to commit to. Defaults to the repository's \
                                        default branch. Created automatically if missing when \
                                        `create_branch` is set."
                    },
                    "message": {
                        "type": "string",
                        "description": "The commit message. A concise subject line, optionally \
                                        followed by a blank line and detail."
                    },
                    "files": {
                        "type": "array",
                        "description": "The changes to make. Every file lands in one commit.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {
                                    "type": "string",
                                    "description": "Path within the repository."
                                },
                                "content": {
                                    "type": "string",
                                    "description": "The file's full new contents. Omit to \
                                                    delete the file."
                                },
                                "delete": {
                                    "type": "boolean",
                                    "description": "Delete this path instead of writing it."
                                }
                            },
                            "required": ["path"],
                            "additionalProperties": false
                        }
                    },
                    "create_branch": {
                        "type": "boolean",
                        "description": "Create `branch` if it does not exist, from `base_branch` \
                                        or the default branch. Defaults to false, so a typo in \
                                        a branch name fails rather than making a stray branch."
                    },
                    "base_branch": {
                        "type": "string",
                        "description": "For create_branch: what to branch from. Defaults to the \
                                        repository's default branch."
                    },
                    "pull_request": {
                        "type": "boolean",
                        "description": "After committing, open a pull request from `branch` into \
                                        `base_branch` or the default branch. Defaults to false."
                    },
                    "pr_title": {
                        "type": "string",
                        "description": "Title for the pull request. Defaults to the commit \
                                        message's first line."
                    },
                    "pr_body": {
                        "type": "string",
                        "description": "Body for the pull request."
                    },
                    "force": {
                        "type": "boolean",
                        "description": "Allow a non-fast-forward update, discarding commits on \
                                        the branch that are not ancestors of this one. Off by \
                                        default, and rarely the right answer."
                    }
                },
                "required": ["repo", "message", "files"],
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["http".to_string()],
        }
    }

    fn invoke(_session_id: String, args_json: String, config_json: String) -> Result<String, String> {
        let args: Value = serde_json::from_str(&args_json)
            .map_err(|e| format!("arguments were not valid JSON: {e}"))?;
        let client = GitHub::from_config(&config_json)?;

        let (owner, repo) = parse_repo(
            args.get("repo")
                .and_then(Value::as_str)
                .ok_or("missing required argument 'repo'")?,
        )?;

        let message = args
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .ok_or("missing required argument 'message'")?;

        let files = args
            .get("files")
            .and_then(Value::as_array)
            .filter(|f| !f.is_empty())
            .ok_or("'files' must be a non-empty array of changes to make")?;

        // The default branch is needed as a fallback in several places, so it
        // is fetched once here.
        let repo_info = client.get(&format!("/repos/{owner}/{repo}"), &[])?;
        let default_branch = repo_info
            .get("default_branch")
            .and_then(Value::as_str)
            .unwrap_or("main")
            .to_string();

        let branch = args
            .get("branch")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .unwrap_or(&default_branch)
            .to_string();

        let base_branch = args
            .get("base_branch")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .unwrap_or(&default_branch)
            .to_string();

        let mut notes: Vec<String> = Vec::new();

        // --- The branch head, creating the branch if asked to ---------------
        let head = match branch_head(&client, &owner, &repo, &branch)? {
            Some(sha) => sha,
            None => {
                if !args.get("create_branch").and_then(Value::as_bool).unwrap_or(false) {
                    return Err(format!(
                        "branch {branch:?} does not exist in {owner}/{repo}. Pass \
                         create_branch: true to make it, or check the name."
                    ));
                }
                let base = branch_head(&client, &owner, &repo, &base_branch)?.ok_or_else(|| {
                    format!("base branch {base_branch:?} does not exist in {owner}/{repo}")
                })?;
                client.post(
                    &format!("/repos/{owner}/{repo}/git/refs"),
                    &json!({ "ref": format!("refs/heads/{branch}"), "sha": base }),
                )?;
                notes.push(format!("created branch {branch} from {base_branch}"));
                base
            }
        };

        // The commit's tree is based on the head's tree, so files nobody
        // mentioned are inherited instead of vanishing.
        let head_commit = client.get(&format!("/repos/{owner}/{repo}/git/commits/{head}"), &[])?;
        let base_tree = head_commit
            .get("tree")
            .and_then(|t| t.get("sha"))
            .and_then(Value::as_str)
            .ok_or("could not read the head commit's tree")?
            .to_string();

        // --- Blobs and the tree --------------------------------------------
        let mut tree_entries: Vec<Value> = Vec::new();
        let mut written: Vec<String> = Vec::new();
        let mut deleted: Vec<String> = Vec::new();

        for file in files {
            let path = file
                .get("path")
                .and_then(Value::as_str)
                .map(str::trim)
                .map(|p| p.trim_start_matches('/'))
                .filter(|p| !p.is_empty())
                .ok_or("every entry in 'files' needs a 'path'")?;

            let explicit_delete = file.get("delete").and_then(Value::as_bool).unwrap_or(false);
            let content = file.get("content").and_then(Value::as_str);

            if explicit_delete || content.is_none() {
                // A null sha in a tree entry is how git records a deletion.
                tree_entries.push(json!({
                    "path": path,
                    "mode": "100644",
                    "type": "blob",
                    "sha": Value::Null,
                }));
                deleted.push(path.to_string());
                continue;
            }

            // Base64 rather than the `content` field: it makes the request
            // encoding-agnostic, so a file with unusual bytes or CRLF endings
            // round-trips exactly as given.
            let blob = client.post(
                &format!("/repos/{owner}/{repo}/git/blobs"),
                &json!({
                    "content": STANDARD.encode(content.unwrap().as_bytes()),
                    "encoding": "base64",
                }),
            )?;
            let blob_sha = blob
                .get("sha")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("GitHub returned no blob sha for {path}"))?;

            tree_entries.push(json!({
                "path": path,
                "mode": "100644",
                "type": "blob",
                "sha": blob_sha,
            }));
            written.push(path.to_string());
        }

        let tree = client.post(
            &format!("/repos/{owner}/{repo}/git/trees"),
            &json!({ "base_tree": base_tree, "tree": tree_entries }),
        )?;
        let tree_sha = tree
            .get("sha")
            .and_then(Value::as_str)
            .ok_or("GitHub returned no tree sha")?;

        // An identical tree means nothing changed. Committing that would make
        // an empty commit, which is noise rather than a result.
        if tree_sha == base_tree {
            return Ok(format!(
                "Nothing to commit: the files given are already exactly what \
                 {owner}/{repo}@{branch} contains.\n"
            ));
        }

        // --- The commit and the ref ----------------------------------------
        let commit = client.post(
            &format!("/repos/{owner}/{repo}/git/commits"),
            &json!({ "message": message, "tree": tree_sha, "parents": [head] }),
        )?;
        let commit_sha = commit
            .get("sha")
            .and_then(Value::as_str)
            .ok_or("GitHub returned no commit sha")?
            .to_string();

        let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
        client
            .patch(
                &format!("/repos/{owner}/{repo}/git/refs/heads/{branch}"),
                &json!({ "sha": commit_sha, "force": force }),
            )
            .map_err(|e| {
                format!(
                    "the commit {} was created but the branch could not be moved to it: {e}\n\n\
                     If someone else pushed to {branch} meanwhile, re-read the files and commit \
                     again rather than forcing.",
                    short(&commit_sha)
                )
            })?;

        // --- Report ---------------------------------------------------------
        let mut out = format!(
            "Committed {} to {owner}/{repo}@{branch}\n\n{}\n",
            short(&commit_sha),
            clip(message.lines().next().unwrap_or(message), 200)
        );
        for path in &written {
            out.push_str(&format!("  + {path}\n"));
        }
        for path in &deleted {
            out.push_str(&format!("  - {path}\n"));
        }

        // Naming the author confirms the App identity actually took effect,
        // which is the whole point of the App setup and is otherwise invisible.
        if let Some(author) = commit
            .get("author")
            .and_then(|a| a.get("name"))
            .and_then(Value::as_str)
        {
            out.push_str(&format!("\nauthored by: {author}\n"));
        }
        if let Some(url) = commit.get("html_url").and_then(Value::as_str) {
            out.push_str(&format!("{url}\n"));
        }
        for note in &notes {
            out.push_str(&format!("({note})\n"));
        }

        // --- Optional pull request ------------------------------------------
        if args.get("pull_request").and_then(Value::as_bool).unwrap_or(false) {
            if branch == base_branch {
                out.push_str(
                    "\nNo pull request opened: the commit went straight to the base branch, so \
                     there is nothing to merge.\n",
                );
                return Ok(out);
            }

            let title = args
                .get("pr_title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| message.lines().next().unwrap_or(message));

            let mut body = json!({
                "title": title,
                "head": branch,
                "base": base_branch,
            });
            if let Some(text) = args.get("pr_body").and_then(Value::as_str) {
                body["body"] = json!(text);
            }

            match client.post(&format!("/repos/{owner}/{repo}/pulls"), &body) {
                Ok(pr) => {
                    let number = pr.get("number").and_then(Value::as_u64).unwrap_or(0);
                    out.push_str(&format!(
                        "\nOpened pull request #{number}: {} -> {base_branch}\n{}\n",
                        branch,
                        pr.get("html_url").and_then(Value::as_str).unwrap_or("")
                    ));
                }
                Err(e) => {
                    // The commit succeeded; only the PR failed. Saying so
                    // prevents a retry that would duplicate the commit.
                    out.push_str(&format!(
                        "\nThe commit succeeded but the pull request could not be opened: {e}\n"
                    ));
                }
            }
        }

        Ok(out)
    }
}

/// The commit SHA a branch points at, or `None` when the branch does not exist.
///
/// A missing branch is an ordinary situation here — it decides whether to
/// create one — so it must not be indistinguishable from a real failure.
fn branch_head(
    client: &GitHub,
    owner: &str,
    repo: &str,
    branch: &str,
) -> Result<Option<String>, String> {
    match client.get(&format!("/repos/{owner}/{repo}/git/ref/heads/{branch}"), &[]) {
        Ok(response) => Ok(response
            .get("object")
            .and_then(|o| o.get("sha"))
            .and_then(Value::as_str)
            .map(str::to_string)),
        Err(e) if e.contains("404") => Ok(None),
        Err(e) => Err(e),
    }
}

/// Seven characters: what git shows and what a human quotes back.
fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

export!(Component);
