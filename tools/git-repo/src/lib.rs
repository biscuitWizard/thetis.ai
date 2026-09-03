//! Repositories and branches: list, inspect, create, and move refs.
//!
//! One tool with an `action` argument rather than six tools, because these
//! operations share their arguments and their output shape, and six near-
//! identical manifests would cost more context than they earn.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod github;

use github::{clip, parse_repo, GitHub};
use serde_json::{json, Value};

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "git-repo".to_string(),
            description: "Inspect and manage GitHub repositories and branches: list the repos \
                          the app can reach, get one repo's details, create a repo, list \
                          branches, create or delete a branch, and read commit history. Use \
                          git-file to read or write file contents."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "list", "get", "create", "branches", "create-branch",
                            "delete-branch", "commits"
                        ],
                        "description":
                            "list: every repository the app can reach (no repo needed). \
                             get: one repository's details. \
                             create: make a new repository. \
                             branches: list branches. \
                             create-branch: branch from `from_ref` or the default branch. \
                             delete-branch: delete a branch. \
                             commits: recent commit history."
                    },
                    "repo": {
                        "type": "string",
                        "description": "Target repository as `owner/repo`, or its URL. Required \
                                        for everything except `list`."
                    },
                    "branch": {
                        "type": "string",
                        "description": "Branch name, for create-branch and delete-branch, and \
                                        as the ref to read for commits."
                    },
                    "from_ref": {
                        "type": "string",
                        "description": "For create-branch: the branch, tag or commit SHA to \
                                        branch from. Defaults to the repository's default branch."
                    },
                    "name": {
                        "type": "string",
                        "description": "For create: the new repository's name."
                    },
                    "owner": {
                        "type": "string",
                        "description": "For create: the org to own the new repository. Omit to \
                                        create it under the account the app is installed on."
                    },
                    "description": {
                        "type": "string",
                        "description": "For create: the repository description."
                    },
                    "private": {
                        "type": "boolean",
                        "description": "For create: whether the repository is private. Defaults \
                                        to true, which is the safe default for new work."
                    },
                    "path": {
                        "type": "string",
                        "description": "For commits: only commits touching this file path."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum items to return, 1-100. Defaults to 30."
                    }
                },
                "required": ["action"],
                "additionalProperties": false
            })
            .to_string(),
            // Not read-only: create, create-branch and delete-branch all write.
            capabilities: vec!["http".to_string()],
        }
    }

    fn invoke(_session_id: String, args_json: String, config_json: String) -> Result<String, String> {
        let args: Value = serde_json::from_str(&args_json)
            .map_err(|e| format!("arguments were not valid JSON: {e}"))?;
        let client = GitHub::from_config(&config_json)?;

        let action = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or("missing required argument 'action'")?;

        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(30)
            .clamp(1, 100) as usize;

        match action {
            "list" => list_repos(&client, limit),
            "get" => get_repo(&client, &args),
            "create" => create_repo(&client, &args),
            "branches" => list_branches(&client, &args, limit),
            "create-branch" => create_branch(&client, &args),
            "delete-branch" => delete_branch(&client, &args),
            "commits" => list_commits(&client, &args, limit),
            other => Err(format!(
                "unknown action {other:?}. Valid actions: list, get, create, branches, \
                 create-branch, delete-branch, commits."
            )),
        }
    }
}

/// The repo argument, resolved to `(owner, repo)`.
fn repo_of(args: &Value) -> Result<(String, String), String> {
    let raw = args
        .get("repo")
        .and_then(Value::as_str)
        .ok_or("this action needs a 'repo' argument, as `owner/repo` or a URL")?;
    parse_repo(raw)
}

fn list_repos(client: &GitHub, limit: usize) -> Result<String, String> {
    let response = client.get(
        "/installation/repositories",
        &[("per_page".to_string(), limit.to_string())],
    )?;
    let repos = response
        .get("repositories")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if repos.is_empty() {
        return Ok(format!(
            "The app can reach no repositories.\n\n{}",
            github::INSTALL_HELP
        ));
    }

    let total = response
        .get("total_count")
        .and_then(Value::as_u64)
        .unwrap_or(repos.len() as u64);

    let mut out = format!("{total} repository/ies reachable\n");
    for repo in &repos {
        out.push_str(&format!("\n{}\n", repo_line(repo)));
    }
    if (repos.len() as u64) < total {
        out.push_str(&format!(
            "\n... {} more; raise `limit` to see them.\n",
            total - repos.len() as u64
        ));
    }
    Ok(out)
}

fn get_repo(client: &GitHub, args: &Value) -> Result<String, String> {
    let (owner, repo) = repo_of(args)?;
    let r = client.get(&format!("/repos/{owner}/{repo}"), &[])?;

    let mut out = format!("{}\n", repo_line(&r));
    if let Some(desc) = r.get("description").and_then(Value::as_str) {
        out.push_str(&format!("\n{desc}\n"));
    }
    out.push_str(&format!(
        "\ndefault branch: {}\n",
        r.get("default_branch").and_then(Value::as_str).unwrap_or("?")
    ));
    if let Some(lang) = r.get("language").and_then(Value::as_str) {
        out.push_str(&format!("language: {lang}\n"));
    }
    out.push_str(&format!(
        "open issues: {}\n",
        r.get("open_issues_count").and_then(Value::as_u64).unwrap_or(0)
    ));
    out.push_str(&format!(
        "clone url: {}\n",
        r.get("clone_url").and_then(Value::as_str).unwrap_or("?")
    ));

    // Permissions decide what the next call can do, so they are worth stating.
    if let Some(perms) = r.get("permissions").and_then(Value::as_object) {
        let granted: Vec<&str> = perms
            .iter()
            .filter(|(_, v)| v.as_bool() == Some(true))
            .map(|(k, _)| k.as_str())
            .collect();
        out.push_str(&format!("app permissions: {}\n", granted.join(", ")));
    }
    Ok(out)
}

fn create_repo(client: &GitHub, args: &Value) -> Result<String, String> {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or("creating a repository needs a 'name'")?;

    let private = args.get("private").and_then(Value::as_bool).unwrap_or(true);
    let mut body = json!({ "name": name, "private": private, "auto_init": true });
    if let Some(desc) = args.get("description").and_then(Value::as_str) {
        body["description"] = json!(desc);
    }

    // A GitHub App cannot create a repository for a *user* account — that
    // endpoint needs a user token. Under an org it can, given the permission.
    let owner = args.get("owner").and_then(Value::as_str).map(str::trim);
    let path = match owner.filter(|o| !o.is_empty()) {
        Some(org) => format!("/orgs/{org}/repos"),
        None => {
            let installations = client.list_installations()?;
            let account = installations
                .first()
                .map(github::account_of)
                .unwrap_or_default();
            return Err(format!(
                "creating a repository needs an 'owner' — the org that will own it (try \
                 {account:?}).\n\nA GitHub App can create repositories in an organisation \
                 where it has 'Administration: write', but it cannot create one under a \
                 personal account: that endpoint requires a user token. For a personal repo, \
                 create it by hand and install the app on it."
            ));
        }
    };

    let created = client.post(&path, &body)?;
    Ok(format!(
        "Created {}\n\n{}\n\nThe app must be installed on it to push. With 'all \
         repositories' selected that is already true; with a chosen list, add it at the \
         installation's settings.\n",
        created.get("full_name").and_then(Value::as_str).unwrap_or(name),
        repo_line(&created)
    ))
}

fn list_branches(client: &GitHub, args: &Value, limit: usize) -> Result<String, String> {
    let (owner, repo) = repo_of(args)?;
    let branches = client.paginate(&format!("/repos/{owner}/{repo}/branches"), &[], limit)?;

    if branches.is_empty() {
        return Ok(format!("{owner}/{repo} has no branches — it may be empty.\n"));
    }

    let mut out = format!("{} branch(es) in {owner}/{repo}\n", branches.len());
    for branch in &branches {
        let name = branch.get("name").and_then(Value::as_str).unwrap_or("?");
        let sha = branch
            .get("commit")
            .and_then(|c| c.get("sha"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let protected = branch
            .get("protected")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        out.push_str(&format!(
            "  {name}  {}{}\n",
            short_sha(sha),
            if protected { "  (protected)" } else { "" }
        ));
    }
    Ok(out)
}

fn create_branch(client: &GitHub, args: &Value) -> Result<String, String> {
    let (owner, repo) = repo_of(args)?;
    let branch = args
        .get("branch")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .ok_or("creating a branch needs a 'branch' name")?;

    // Resolve the starting point to a commit SHA. `from_ref` may be a branch,
    // a tag or a SHA already, and the refs API needs a SHA.
    let from = args
        .get("from_ref")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|f| !f.is_empty());

    let (base_name, sha) = match from {
        Some(reference) => (reference.to_string(), resolve_sha(client, &owner, &repo, reference)?),
        None => {
            let r = client.get(&format!("/repos/{owner}/{repo}"), &[])?;
            let default = r
                .get("default_branch")
                .and_then(Value::as_str)
                .unwrap_or("main")
                .to_string();
            let sha = resolve_sha(client, &owner, &repo, &default)?;
            (default, sha)
        }
    };

    client.post(
        &format!("/repos/{owner}/{repo}/git/refs"),
        &json!({ "ref": format!("refs/heads/{branch}"), "sha": sha }),
    )?;

    Ok(format!(
        "Created branch {branch} in {owner}/{repo} at {} (from {base_name}).\n",
        short_sha(&sha)
    ))
}

fn delete_branch(client: &GitHub, args: &Value) -> Result<String, String> {
    let (owner, repo) = repo_of(args)?;
    let branch = args
        .get("branch")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .ok_or("deleting a branch needs a 'branch' name")?;

    client.delete(&format!("/repos/{owner}/{repo}/git/refs/heads/{branch}"))?;
    Ok(format!("Deleted branch {branch} from {owner}/{repo}.\n"))
}

fn list_commits(client: &GitHub, args: &Value, limit: usize) -> Result<String, String> {
    let (owner, repo) = repo_of(args)?;
    let mut query: Vec<(String, String)> = Vec::new();
    if let Some(branch) = args.get("branch").and_then(Value::as_str) {
        query.push(("sha".to_string(), branch.to_string()));
    }
    if let Some(path) = args.get("path").and_then(Value::as_str) {
        query.push(("path".to_string(), path.to_string()));
    }

    let commits = client.paginate(&format!("/repos/{owner}/{repo}/commits"), &query, limit)?;
    if commits.is_empty() {
        return Ok(format!("No commits found in {owner}/{repo}.\n"));
    }

    let mut out = format!("{} commit(s) in {owner}/{repo}\n", commits.len());
    for entry in &commits {
        let sha = entry.get("sha").and_then(Value::as_str).unwrap_or("");
        let commit = entry.get("commit").unwrap_or(&Value::Null);
        let author = commit
            .get("author")
            .and_then(|a| a.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let date = commit
            .get("author")
            .and_then(|a| a.get("date"))
            .and_then(Value::as_str)
            .unwrap_or("");
        // First line only: a commit body can be arbitrarily long and this is a
        // listing, not a reader.
        let message = commit
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("");

        out.push_str(&format!(
            "\n  {}  {}\n    {author}  {}\n",
            short_sha(sha),
            clip(message, 120),
            clip(date, 19)
        ));
    }
    Ok(out)
}

/// Resolves a branch, tag or SHA to a commit SHA.
fn resolve_sha(client: &GitHub, owner: &str, repo: &str, reference: &str) -> Result<String, String> {
    // A 40-character hex string is already a SHA; asking GitHub would be a
    // wasted round trip.
    if reference.len() == 40 && reference.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(reference.to_string());
    }

    let response = client
        .get(&format!("/repos/{owner}/{repo}/commits/{reference}"), &[])
        .map_err(|e| format!("could not resolve {reference:?} in {owner}/{repo}: {e}"))?;

    response
        .get("sha")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{reference:?} did not resolve to a commit"))
}

fn repo_line(repo: &Value) -> String {
    let full = repo.get("full_name").and_then(Value::as_str).unwrap_or("?");
    let private = repo.get("private").and_then(Value::as_bool).unwrap_or(false);
    let archived = repo.get("archived").and_then(Value::as_bool).unwrap_or(false);
    let mut line = format!("{full}");
    if private {
        line.push_str(" (private)");
    }
    if archived {
        line.push_str(" (archived)");
    }
    if let Some(url) = repo.get("html_url").and_then(Value::as_str) {
        line.push_str(&format!("\n   {url}"));
    }
    line
}

/// Seven characters is what git itself shows and what a human quotes back.
fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

export!(Component);
