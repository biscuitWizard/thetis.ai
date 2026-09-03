---
name = "GitHub via the app identity"
brief = "Commit, branch, read files, open PRs and clone repositories with the git-* tools, authored as the app's own [bot] user."
when_to_use = "Use for any task touching a GitHub repository: reading a file, committing, branching, opening a pull request, or cloning a working tree. Use it also when a git-* tool returns 401, 403, 404 or 422, since each has a specific cause here. Not for this project's own sandbox branch — that is branch_status, branch_log and update_from_trunk, which never touch GitHub."
universal = false
tags = ["github", "git", "commit", "pull request", "clone", "branch", "repository", "push", "rebase", "test run", "working tree", "401", "403", "404", "422", "tool-group:github", "tool-group:shell"]
version = 3
---
