---
name = "GitHub via the app identity"
brief = "Commit, branch, read files, open PRs and clone repositories with the git-* tools, authored as the app's own [bot] user."
when_to_use = "Use for any task touching a GitHub repository: reading a file without cloning, committing changes, creating a branch or repo, opening a pull request, checking commit history, or cloning a real working tree. Use it also when a git-* tool returns 401, 403, 404 or 422, since each has a specific cause here, and when a task needs a rebase or test run against a working tree. Not for this project's own sandbox branch — that is branch_status, branch_log and update_from_trunk, which never touch GitHub."
universal = false
tags = ["github", "git", "commit", "pull request", "clone", "branch", "repository", "push"]
version = 2
---

# GitHub via the app identity

## The tools

The group is named `git-*`. It was `github-*` until the rename; if you see the
old names anywhere, they are stale.

| Tool | Use it for | Writes? |
|---|---|---|
| `git-whoami` | Verify credentials; show the `[bot]` identity and reachable repos | no |
| `git-file` | Read a file or list a directory at any ref | no |
| `git-repo` | `list`, `get`, `create`, `branches`, `create-branch`, `delete-branch`, `commits` | some actions |
| `git-commit` | Create/update/delete several files in one commit; optionally open a PR | yes |
| `git_clone` | Clone into a real working tree with `.git` (a built-in, not a component) | yes |

The four components share `[tools.git]` in the config, so the credential is set
once — the scope follows the tool names, so an old `[tools.github]` block is no
longer read by anything.

## Start here when anything fails

Run `git-whoami`. It exercises the whole chain — JWT, `/app`, installations,
installation token, bot user id, repository list — and names the step that
failed. A bare 401 from another tool tells you nothing by comparison.

## Committing

`git-commit` builds a real git commit through the Git Data API: blob, tree
(with `base_tree`, so untouched files are inherited), commit, then move the ref.
Consequences worth knowing:

- **Several files land atomically.** Pass them all in one call rather than making
  one commit per file, which would leave the repo broken at each step.
- **Omit `content`, or set `delete: true`, to delete a path.**
- **A no-op is detected.** If the resulting tree matches the current one, it
  says nothing to commit instead of making an empty commit.
- **The push is fast-forward only** unless `force: true`. If the ref moved under
  you, re-read the files and commit again; do not force.
- **Authorship is automatic.** The installation token makes GitHub attribute the
  commit to `<slug>[bot]`. Do not set author or committer fields.

To put work on a branch and propose it in one call:

```json
{ "repo": "owner/repo", "branch": "feature-x", "create_branch": true,
  "message": "Add the thing", "files": [{"path": "src/a.rs", "content": "..."}],
  "pull_request": true }
```

If the PR fails after the commit succeeded, the output says so. Do not retry the
whole call — that would duplicate the commit.

## When you need a real working tree

The API needs no clone, which is why it stays the default path. But a rebase, a
bisect, or running a test suite needs actual files. That is `git_clone`:

```json
{ "repo": "owner/repo" }
```

It lands in `workspace/<name>` — gitignored, so a large repository does not show
up as untracked cruft in your own checkout — and takes `dir`, `ref` and `depth`.
Omit `depth` for a rebase or a bisect: those need real history.

What it leaves behind, and why:

- **The remote is tokenless.** An installation token lasts an hour, so writing
  one into `.git/config` would leak a secret *and* leave a stale one.
- **`user.name` / `user.email` are set on the clone** to the `[bot]` identity,
  because the `GIT_AUTHOR_*` exports only live as long as the shell that made
  them; a later terminal would otherwise commit as nobody.
- **To push, authenticate that session first:**

```bash
eval "$(scripts/github-app-git.sh env)"   # token + GIT_AUTHOR_*/GIT_COMMITTER_*
```

`env` installs a `url.insteadOf` rewrite, so git supplies the credential itself
and the stored remote stays clean. The same script does `clone`, `token` and
`identity` by hand if you need them.

Committing by hand in a tree that `git_clone` did not set up needs the exact bot
identity, or GitHub shows the commit as an unlinked stranger:

```
git config user.name  "<slug>[bot]"
git config user.email "<bot-user-id>+<slug>[bot]@users.noreply.github.com"
```

The id is the **bot user's**, not the app's. `git-whoami` prints both lines
ready to paste.

## Reading the errors

| Status | What it actually means |
|---|---|
| 401 | Bad private key, wrong `app_id`, or a clock over a minute fast. |
| 403 | Installed but missing a permission. Adding a permission needs the installation to **approve** it — it is not granted retroactively. |
| 404 | Wrong path, *or* the app is not installed on that repo. GitHub hides invisible repos as 404, so this is the usual symptom of a missing install. |
| 409 | Empty repository, or a ref moved mid-request. Retry. |
| 422 | Shape was fine, content refused: ref already exists, PR has no diff, branch name taken. |

`git_clone` failing with `no app_id found in [tools.git]` is the same missing
credential, reported by the script rather than by a tool.

## Setup, if it is not configured

Needs a human once; the agent cannot create an app or install it. See
`references/setup.md` for the exact click path and the permission set.
