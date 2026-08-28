//! Git operations for the sandbox-branch workflow.
//!
//! Every conversation runs against its own branch (`conv/<session-id>`)
//! checked out in its own worktree, and trunk (`main`) only ever moves by
//! fast-forward. This module is the single place the kernel talks to git: it
//! shells out to the `git` binary rather than binding libgit2, because
//! worktrees, merges and conflict state are exactly the corners where library
//! reimplementations diverge from the reference implementation. Output is
//! parsed only from plumbing formats git documents as stable.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::process::Output;
use std::time::Duration;

/// Local git operations never legitimately take this long; a hung git would
/// otherwise wedge a worker's turn loop with it.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Separators for `--format` parsing. Git subjects can contain almost
/// anything, so fields are split on control characters no subject carries.
const FIELD_SEP: char = '\u{1f}';
const RECORD_SEP: char = '\u{1e}';

/// One git checkout — the root working tree or a single worktree.
///
/// Worktree and branch management run on the root instance; content-level
/// operations (commit, merge, reset) run on the instance for the checkout
/// they touch, which keeps exactly one writer per worktree.
#[derive(Debug, Clone)]
pub struct GitCtl {
    dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub rev: String,
    pub subject: String,
    pub author: String,
    /// Unix time, milliseconds.
    pub ts_ms: u64,
    /// Parent commit ids — what a graph needs to draw edges. Empty when the
    /// log was read without them.
    pub parents: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Merged cleanly (or nothing to do); the resulting HEAD.
    Clean { head: String },
    /// The merge stopped on conflicts, which remain in the working tree for
    /// someone — the user or the conversation — to resolve.
    Conflict { paths: Vec<String> },
}

/// One lock per checkout, serializing the git commands that write.
///
/// Git protects its index with `index.lock` and simply *fails* when it is
/// held — it does not queue. Two writers in one worktree is not hypothetical
/// here: the watcher commits a checkpoint whenever a file changes, while the
/// agent's own branch tools commit, merge and reset the same tree. The loser
/// got "Unable to create index.lock: File exists", which surfaced as a failed
/// merge or a lost checkpoint.
///
/// Read-only commands are deliberately *not* serialized: a `log` for the
/// history panel must not queue behind a multi-minute merge.
static WRITE_LOCKS: std::sync::OnceLock<
    std::sync::Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
> = std::sync::OnceLock::new();

/// Git subcommands that take the index or move refs.
///
/// Unknown verbs are treated as writers: being wrong that way costs a little
/// serialization, the other way costs a lost commit.
fn mutates(args: &[&str]) -> bool {
    const READ_ONLY: &[&str] = &[
        "log",
        "diff",
        "show",
        "status",
        "rev-parse",
        "rev-list",
        "show-ref",
        "for-each-ref",
        "cat-file",
        "ls-tree",
        "ls-files",
        "merge-base",
        "describe",
        "config",
        "symbolic-ref",
        "name-rev",
        "var",
    ];
    // Skip the `-c key=value` prelude and any other leading options.
    let verb = args
        .iter()
        .find(|a| !a.starts_with('-') && !a.contains('='));
    match verb {
        Some(v) => !READ_ONLY.contains(v),
        None => true,
    }
}

impl GitCtl {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// This checkout's write lock, created on demand.
    fn write_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        let locks = WRITE_LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
        let mut map = match locks.lock() {
            Ok(map) => map,
            // Poisoned by a panic elsewhere; an unshared lock is better than
            // refusing to run git at all.
            Err(e) => e.into_inner(),
        };
        if map.len() > 64 {
            map.retain(|_, l| Arc::strong_count(l) > 1);
        }
        map.entry(self.dir.clone()).or_default().clone()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Run git with this checkout as the working directory and return its
    /// output, failing on a non-zero exit. `run_ok` is the variant for
    /// commands whose non-zero exits are answers rather than errors.
    async fn run(&self, args: &[&str]) -> Result<Output> {
        let out = self.run_ok(args).await?;
        if !out.status.success() {
            bail!(
                "git {} failed in {}: {}",
                args.join(" "),
                self.dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out)
    }

    /// The shared command prelude.
    ///
    /// `hooks` is false for every internal operation. A `commit`, `merge`, or
    /// `checkout` fires hooks from the repository's shared hooks directory —
    /// and the agent, which has a shell in its worktree, can write a
    /// `pre-commit` there that would then run with the orchestrator's
    /// privileges, escaping the wasm sandbox. Pointing `core.hooksPath` at a
    /// non-directory disables every hook. Only `push` keeps hooks on, because
    /// the pre-push privacy guard is one (see `run_hooked`).
    fn base_cmd(&self, hooks: bool) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("git");
        cmd.arg("-C")
            .arg(&self.dir)
            // These are operator commands, not a user at a prompt.
            .arg("-c")
            .arg("advice.detachedHead=false")
            .arg("-c")
            .arg("commit.gpgsign=false");
        if !hooks {
            cmd.arg("-c").arg("core.hooksPath=/dev/null");
        }
        cmd.env("GIT_TERMINAL_PROMPT", "0").kill_on_drop(true);
        cmd
    }

    async fn run_ok(&self, args: &[&str]) -> Result<Output> {
        // Writers queue per checkout; readers do not queue at all.
        let lock = mutates(args).then(|| self.write_lock());
        let _guard = match &lock {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };

        let mut cmd = self.base_cmd(false);
        cmd.args(args);

        let out = crate::control::run_child(
            cmd,
            GIT_TIMEOUT,
            &format!("git {} in {}", args.join(" "), self.dir.display()),
        )
        .await?;
        tracing::debug!(
            dir = %self.dir.display(),
            cmd = %args.join(" "),
            status = out.status.code().unwrap_or(-1),
            "git"
        );
        Ok(out)
    }

    fn stdout(out: &Output) -> String {
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Escape hatch for setup and plumbing not worth a dedicated method
    /// (init, config in tests; rev-parse variants). Fails on non-zero exit.
    pub async fn run_raw(&self, args: &[&str]) -> Result<Output> {
        self.run(args).await
    }

    /// Runs git with hooks left enabled — only for `push`, whose pre-push
    /// privacy guard is the one hook Thetis deliberately relies on. Every
    /// other path goes through `run`/`run_ok` with hooks disabled. `envs`
    /// carries the documented `THETIS_ALLOW_PUSH` override.
    pub async fn run_hooked(&self, args: &[&str], envs: &[(&str, &str)]) -> Result<Output> {
        // Both callers write (a push, a temp-index export); queue like `run_ok`.
        let _guard = self.write_lock().lock_owned().await;
        let mut cmd = self.base_cmd(true);
        cmd.args(args);
        for (key, value) in envs {
            cmd.env(key, value);
        }
        let out = crate::control::run_child(
            cmd,
            GIT_TIMEOUT,
            &format!("git {} in {}", args.join(" "), self.dir.display()),
        )
        .await?;
        if !out.status.success() {
            bail!(
                "git {} failed in {}: {}",
                args.join(" "),
                self.dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out)
    }

    /// As `run_hooked`, but a non-zero exit is returned rather than raised.
    ///
    /// Publishing needs this: git's own wording for a refused push says
    /// nothing about which of its several causes applies, and the caller can
    /// only translate stderr it is still holding. Callers must check
    /// `out.status` themselves — nothing else here reports failure.
    pub async fn run_hooked_status(&self, args: &[&str], envs: &[(&str, &str)]) -> Result<Output> {
        let _guard = self.write_lock().lock_owned().await;
        let mut cmd = self.base_cmd(true);
        cmd.args(args);
        for (key, value) in envs {
            cmd.env(key, value);
        }
        crate::control::run_child(
            cmd,
            GIT_TIMEOUT,
            &format!("git {} in {}", args.join(" "), self.dir.display()),
        )
        .await
    }

    /// As `run_raw`, with extra environment — the temp-index plumbing the
    /// public export uses (`GIT_INDEX_FILE`).
    pub async fn run_with_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Result<Output> {
        // Both callers write (a push, a temp-index export); queue like `run_ok`.
        let _guard = self.write_lock().lock_owned().await;
        let mut cmd = self.base_cmd(false);
        cmd.args(args);
        for (key, value) in envs {
            cmd.env(key, value);
        }
        let out = crate::control::run_child(
            cmd,
            GIT_TIMEOUT,
            &format!("git {} in {}", args.join(" "), self.dir.display()),
        )
        .await?;
        if !out.status.success() {
            bail!(
                "git {} failed in {}: {}",
                args.join(" "),
                self.dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out)
    }

    /// Every path in `rev`'s tree.
    ///
    /// Read with NUL delimiters and quoting disabled. With git's default
    /// `core.quotePath`, a path containing a non-ASCII byte, a control char, a
    /// quote or a backslash comes back C-quoted (`"tools/caf\303\251/x"`),
    /// which silently defeats any suffix match a caller does — including the
    /// `.thetis-private` marker detection that keeps private paths off the
    /// public remote. `-z` gives the raw bytes, one path per NUL.
    pub async fn tree_files(&self, rev: &str) -> Result<Vec<String>> {
        let out = self
            .run(&[
                "-c",
                "core.quotePath=false",
                "ls-tree",
                "-r",
                "-z",
                "--name-only",
                rev,
            ])
            .await?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Commits in `range` (e.g. "a..b"), oldest first.
    pub async fn rev_list(&self, range: &str) -> Result<Vec<String>> {
        let out = self.run(&["rev-list", "--reverse", range]).await?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect())
    }

    pub async fn update_ref(&self, name: &str, rev: &str) -> Result<()> {
        self.run(&["update-ref", name, rev]).await?;
        Ok(())
    }

    /// Move `name` to `new_rev`, but only while it still points at `old_rev` —
    /// git's own compare-and-swap. Rewriting a branch another process may be
    /// committing to is exactly where a lost update would hurt, so the
    /// expected value is always stated rather than assumed.
    pub async fn update_ref_checked(&self, name: &str, new_rev: &str, old_rev: &str) -> Result<()> {
        self.run(&["update-ref", name, new_rev, old_rev]).await?;
        Ok(())
    }

    /// Collapse everything `branch` has that `onto` does not into a single
    /// commit whose parent is `onto`, and move `branch` to it. Returns the new
    /// commit id.
    ///
    /// The new commit carries `branch`'s tree **byte for byte**, so this is a
    /// history rewrite that changes no file: a worktree with `branch` checked
    /// out stays clean, its index stays valid, and any build cached against
    /// the tree stays valid too. The old tip is left in `keep_ref` (when
    /// given) so the pre-squash history remains reachable.
    ///
    /// `onto` must already be an ancestor of `branch` — the caller merges
    /// trunk in first — otherwise the result would silently drop trunk's work.
    pub async fn squash_onto(
        &self,
        branch: &str,
        onto: &str,
        message: &str,
        keep_ref: Option<&str>,
    ) -> Result<String> {
        let old = self
            .rev_parse(branch)
            .await?
            .with_context(|| format!("'{branch}' does not name a commit"))?;
        let base = self
            .rev_parse(onto)
            .await?
            .with_context(|| format!("'{onto}' does not name a commit"))?;
        if !self.is_ancestor(&base, &old).await? {
            bail!("refusing to squash {branch}: {onto} is not an ancestor of it");
        }
        if base == old {
            return Ok(old); // nothing of its own to collapse
        }

        let tree = Self::stdout(&self.run(&["rev-parse", &format!("{old}^{{tree}}")]).await?);
        let squashed = Self::stdout(
            &self
                .run(&["commit-tree", &tree, "-p", &base, "-m", message])
                .await?,
        );

        if let Some(keep) = keep_ref {
            // Recorded before the rewrite: if this fails, nothing has moved.
            self.update_ref(keep, &old).await?;
        }
        let full = if branch.starts_with("refs/") {
            branch.to_string()
        } else {
            format!("refs/heads/{branch}")
        };
        self.update_ref_checked(&full, &squashed, &old).await?;
        Ok(squashed)
    }

    /// The repository's common git directory (shared across worktrees),
    /// absolute — where hooks live.
    pub async fn common_dir(&self) -> Result<PathBuf> {
        let out = self
            .run(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .await?;
        Ok(PathBuf::from(Self::stdout(&out)))
    }

    // --- inspection -------------------------------------------------------

    pub async fn is_repo(&self) -> bool {
        matches!(
            self.run_ok(&["rev-parse", "--is-inside-work-tree"]).await,
            Ok(out) if out.status.success()
        )
    }

    pub async fn head(&self) -> Result<String> {
        Ok(Self::stdout(&self.run(&["rev-parse", "HEAD"]).await?))
    }

    /// The checked-out branch name, e.g. `conv/3fa2c1d8`.
    pub async fn current_branch(&self) -> Result<String> {
        Ok(Self::stdout(
            &self.run(&["rev-parse", "--abbrev-ref", "HEAD"]).await?,
        ))
    }

    /// Resolve any revision expression to a commit id, `None` if it does not
    /// name one.
    pub async fn rev_parse(&self, rev: &str) -> Result<Option<String>> {
        let spec = format!("{rev}^{{commit}}");
        let out = self.run_ok(&["rev-parse", "--verify", "--quiet", &spec]).await?;
        if out.status.success() {
            Ok(Some(Self::stdout(&out)))
        } else {
            Ok(None)
        }
    }

    /// The tree object id of `path` as of `rev` — the content-addressed key
    /// the build cache uses. `None` when the path does not exist there.
    pub async fn tree_oid(&self, rev: &str, path: &str) -> Result<Option<String>> {
        let spec = format!("{rev}:{path}");
        let out = self.run_ok(&["rev-parse", "--verify", "--quiet", &spec]).await?;
        if out.status.success() {
            Ok(Some(Self::stdout(&out)))
        } else {
            Ok(None)
        }
    }

    /// Whether the working tree differs from HEAD (staged, unstaged, or
    /// untracked-and-not-ignored).
    pub async fn is_dirty(&self) -> Result<bool> {
        let out = self.run(&["status", "--porcelain"]).await?;
        Ok(!out.stdout.is_empty())
    }

    pub async fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let out = self
            .run_ok(&["merge-base", "--is-ancestor", ancestor, descendant])
            .await?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!(
                "git merge-base --is-ancestor {ancestor} {descendant}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
    }

    /// Commits reachable from `ours` but not `theirs`, and vice versa —
    /// the panel's ahead/behind counts.
    pub async fn ahead_behind(&self, ours: &str, theirs: &str) -> Result<(u64, u64)> {
        let range = format!("{theirs}...{ours}");
        let out = self
            .run(&["rev-list", "--left-right", "--count", &range])
            .await?;
        let text = Self::stdout(&out);
        let mut parts = text.split_whitespace();
        let (behind, ahead) = (parts.next(), parts.next());
        match (behind, ahead) {
            (Some(b), Some(a)) => Ok((a.parse()?, b.parse()?)),
            _ => bail!("unexpected rev-list --count output: {text:?}"),
        }
    }

    pub async fn log(&self, rev: &str, limit: usize) -> Result<Vec<CommitInfo>> {
        self.log_args(&[rev], limit).await
    }

    /// A log over arbitrary rev-list arguments (exclusions included), with
    /// parent ids — what a commit graph draws from.
    pub async fn log_args(&self, revs: &[&str], limit: usize) -> Result<Vec<CommitInfo>> {
        let format = format!("%H{FIELD_SEP}%P{FIELD_SEP}%s{FIELD_SEP}%an{FIELD_SEP}%at{RECORD_SEP}");
        let max = format!("--max-count={limit}");
        let fmt = format!("--format={format}");
        let mut args = vec!["log", max.as_str(), fmt.as_str()];
        args.extend_from_slice(revs);
        let out = self.run(&args).await?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut commits = Vec::new();
        for record in text.split(RECORD_SEP) {
            let record = record.trim_matches(['\n', ' ']);
            if record.is_empty() {
                continue;
            }
            let fields: Vec<&str> = record.split(FIELD_SEP).collect();
            if fields.len() != 5 {
                bail!("unexpected git log record: {record:?}");
            }
            commits.push(CommitInfo {
                rev: fields[0].to_string(),
                parents: fields[1].split_whitespace().map(str::to_string).collect(),
                subject: fields[2].to_string(),
                author: fields[3].to_string(),
                ts_ms: fields[4].parse::<u64>().unwrap_or(0) * 1000,
            });
        }
        Ok(commits)
    }

    /// The common ancestor of two revisions — where a branch forked.
    pub async fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
        let out = self.run_ok(&["merge-base", a, b]).await?;
        if out.status.success() {
            Ok(Some(Self::stdout(&out)))
        } else {
            Ok(None)
        }
    }

    /// Paths with unmerged index entries — the conflict list.
    pub async fn unmerged_paths(&self) -> Result<Vec<String>> {
        let out = self.run(&["ls-files", "-u", "-z"]).await?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut paths: Vec<String> = text
            .split('\0')
            .filter(|entry| !entry.is_empty())
            // Each entry is "<mode> <oid> <stage>\t<path>".
            .filter_map(|entry| entry.split_once('\t').map(|(_, p)| p.to_string()))
            .collect();
        paths.dedup();
        Ok(paths)
    }

    /// A merge stopped on conflicts and has not been committed or aborted.
    pub async fn merge_in_progress(&self) -> Result<bool> {
        let out = self.run(&["rev-parse", "--git-path", "MERGE_HEAD"]).await?;
        let rel = Self::stdout(&out);
        let path = Path::new(&rel);
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.dir.join(path)
        };
        Ok(abs.exists())
    }

    // --- mutation ---------------------------------------------------------

    /// Stage everything and commit. Returns the new HEAD, or `None` when the
    /// tree was already clean — callers use this after turns and builds, and
    /// most of the time there is nothing to record.
    pub async fn add_all_and_commit(&self, message: &str) -> Result<Option<String>> {
        self.run(&["add", "-A"]).await?;
        let staged = self.run_ok(&["diff", "--cached", "--quiet"]).await?;
        if staged.status.success() {
            return Ok(None);
        }
        self.run(&["commit", "-m", message]).await?;
        Ok(Some(self.head().await?))
    }

    /// Merge `rev` into the current branch. Conflicts are left in the
    /// working tree, exactly as a human would find them.
    pub async fn merge(&self, rev: &str, message: &str) -> Result<MergeOutcome> {
        let out = self
            .run_ok(&["merge", "--no-edit", "-m", message, rev])
            .await?;
        if out.status.success() {
            return Ok(MergeOutcome::Clean {
                head: self.head().await?,
            });
        }
        let conflicts = self.unmerged_paths().await?;
        if conflicts.is_empty() {
            // Non-zero without conflicts is a real failure (bad rev, dirty
            // tree refusing the merge), not an outcome to hand to a model.
            bail!(
                "git merge {rev} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(MergeOutcome::Conflict { paths: conflicts })
    }

    pub async fn merge_abort(&self) -> Result<()> {
        self.run(&["merge", "--abort"]).await?;
        Ok(())
    }

    /// Commit a conflicted merge after its conflicts were resolved in the
    /// working tree.
    ///
    /// Resolution happens by editing files (the agent never runs `git add`),
    /// so unmerged index entries are expected here — what must NOT remain is
    /// conflict markers in the files themselves. Checked before staging,
    /// because `add -A` erases the unmerged entries that say where to look.
    pub async fn commit_merge(&self, message: &str) -> Result<String> {
        let unresolved: Vec<String> = {
            let mut found = Vec::new();
            for path in self.unmerged_paths().await? {
                let file = self.dir.join(&path);
                // A missing file is a resolution too: deleted on purpose.
                let Ok(text) = tokio::fs::read_to_string(&file).await else {
                    continue;
                };
                if text.lines().any(|l| {
                    l.starts_with("<<<<<<< ") || l.starts_with(">>>>>>> ") || l == "======="
                }) {
                    found.push(path);
                }
            }
            found
        };
        if !unresolved.is_empty() {
            bail!(
                "cannot commit merge: conflict markers remain in {}",
                unresolved.join(", ")
            );
        }
        self.run(&["add", "-A"]).await?;
        self.run(&["commit", "--no-edit", "-m", message]).await?;
        self.head().await
    }

    /// Advance the current branch to `rev` only if it is a descendant —
    /// the one and only way trunk moves.
    pub async fn merge_ff_only(&self, rev: &str) -> Result<String> {
        self.run(&["merge", "--ff-only", rev]).await?;
        self.head().await
    }

    /// Restore `paths` to their state at `rev`, leaving other files alone —
    /// the watchdog's aspect-subtree reset.
    pub async fn checkout_paths(&self, rev: &str, paths: &[&str]) -> Result<()> {
        let mut args = vec!["checkout", rev, "--"];
        args.extend_from_slice(paths);
        self.run(&args).await?;
        Ok(())
    }

    pub async fn reset_hard(&self, rev: &str) -> Result<()> {
        self.run(&["reset", "--hard", rev]).await?;
        Ok(())
    }

    /// Makes `path` exactly match its state at `rev` — content, additions,
    /// and deletions alike. A plain `checkout rev -- path` restores and adds
    /// but never deletes, which would leave a "reset" subtree still carrying
    /// files the bad version introduced.
    pub async fn sync_paths_to(&self, rev: &str, path: &str) -> Result<()> {
        // Restore first, remove second. The old order deleted the subtree and
        // *then* checked it out, so any failure in between — a git lock held by
        // a concurrent checkpoint, the 120s timeout, a killed process — left
        // the worktree with that path simply gone and the caller with no way
        // back. Writing `rev`'s content over whatever is there first means a
        // failure at any point leaves the tree no worse than it started.
        self.run(&["checkout", rev, "--", path]).await?;

        // Tracked files under `path` that `rev` does not have. Diffing *from*
        // `rev` makes them the additions; they are the only tracked leftovers,
        // since everything `rev` does have was just overwritten.
        let listing = self
            .run(&[
                "diff",
                "--name-only",
                "-z",
                "--diff-filter=A",
                rev,
                "--",
                path,
            ])
            .await?;
        let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
        let extra: Vec<&str> = listing.split('\0').filter(|f| !f.is_empty()).collect();
        if !extra.is_empty() {
            let mut args = vec!["rm", "-q", "-f", "--ignore-unmatch", "--"];
            args.extend(extra);
            self.run(&args).await?;
        }

        // Untracked strays in the subtree go too; without -x, ignored build
        // output survives.
        self.run(&["clean", "-fdq", "--", path]).await?;
        Ok(())
    }

    /// Puts the whole checkout at exactly `rev`: tracked files reset,
    /// untracked files removed, ignored files (build caches) left alone.
    pub async fn hard_reset_clean(&self, rev: &str) -> Result<()> {
        self.run(&["reset", "--hard", rev]).await?;
        self.run(&["clean", "-fdq"]).await?;
        Ok(())
    }

    // --- branches and worktrees (root checkout only) ----------------------

    pub async fn branch_create(&self, name: &str, start: &str) -> Result<()> {
        self.run(&["branch", name, start]).await?;
        Ok(())
    }

    pub async fn branch_delete(&self, name: &str) -> Result<()> {
        self.run(&["branch", "-D", name]).await?;
        Ok(())
    }

    pub async fn branch_exists(&self, name: &str) -> Result<bool> {
        let rev = format!("refs/heads/{name}");
        let out = self
            .run_ok(&["show-ref", "--verify", "--quiet", &rev])
            .await?;
        Ok(out.status.success())
    }

    /// Check `branch` out at `path` as a linked worktree and return a
    /// `GitCtl` scoped to it.
    pub async fn worktree_add(&self, path: &Path, branch: &str) -> Result<GitCtl> {
        let path_str = path
            .to_str()
            .with_context(|| format!("non-utf8 worktree path {}", path.display()))?;
        self.run(&["worktree", "add", path_str, branch]).await?;
        Ok(GitCtl::new(path))
    }

    pub async fn worktree_remove(&self, path: &Path) -> Result<()> {
        let path_str = path
            .to_str()
            .with_context(|| format!("non-utf8 worktree path {}", path.display()))?;
        self.run(&["worktree", "remove", "--force", path_str]).await?;
        Ok(())
    }

    /// Drop bookkeeping for worktrees whose directories no longer exist —
    /// part of the boot sweep after a crash.
    pub async fn worktree_prune(&self) -> Result<()> {
        self.run(&["worktree", "prune"]).await?;
        Ok(())
    }

    pub async fn worktree_paths(&self) -> Result<Vec<PathBuf>> {
        let out = self.run(&["worktree", "list", "--porcelain"]).await?;
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .map(PathBuf::from)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// A repo with one commit on `main`, identity configured so commits work
    /// on a machine with no global git config.
    async fn repo() -> (TempDir, GitCtl) {
        let tmp = TempDir::new().unwrap();
        let git = GitCtl::new(tmp.path());
        git.run(&["init", "-b", "main"]).await.unwrap();
        git.run(&["config", "user.name", "test"]).await.unwrap();
        git.run(&["config", "user.email", "test@example.com"])
            .await
            .unwrap();
        fs::write(tmp.path().join("base.txt"), "base\n").unwrap();
        git.add_all_and_commit("base").await.unwrap().unwrap();
        (tmp, git)
    }

    #[tokio::test]
    async fn commit_returns_none_when_clean() {
        let (_tmp, git) = repo().await;
        assert!(git.add_all_and_commit("noop").await.unwrap().is_none());
        assert!(!git.is_dirty().await.unwrap());
    }

    #[tokio::test]
    async fn commit_captures_untracked_files() {
        let (tmp, git) = repo().await;
        fs::write(tmp.path().join("new.txt"), "hi\n").unwrap();
        assert!(git.is_dirty().await.unwrap());
        let head = git.add_all_and_commit("add new").await.unwrap().unwrap();
        assert_eq!(head, git.head().await.unwrap());
        assert!(!git.is_dirty().await.unwrap());
    }

    #[tokio::test]
    async fn worktree_branch_lifecycle() {
        let (tmp, git) = repo().await;
        git.branch_create("conv/abc", "main").await.unwrap();
        assert!(git.branch_exists("conv/abc").await.unwrap());

        let wt_path = tmp.path().join("worktrees").join("abc");
        let wt = git.worktree_add(&wt_path, "conv/abc").await.unwrap();
        assert!(wt_path.join("base.txt").is_file());

        fs::write(wt_path.join("tool.rs"), "fn main() {}\n").unwrap();
        wt.add_all_and_commit("branch work").await.unwrap().unwrap();

        // Branch moved; main did not.
        let (ahead, behind) = git.ahead_behind("conv/abc", "main").await.unwrap();
        assert_eq!((ahead, behind), (1, 0));

        git.worktree_remove(&wt_path).await.unwrap();
        assert!(!wt_path.exists());
        // The branch and its commit survive the worktree.
        assert!(git.branch_exists("conv/abc").await.unwrap());
        git.branch_delete("conv/abc").await.unwrap();
        assert!(!git.branch_exists("conv/abc").await.unwrap());
    }

    #[tokio::test]
    async fn prune_recovers_from_a_deleted_worktree_dir() {
        let (tmp, git) = repo().await;
        git.branch_create("conv/lost", "main").await.unwrap();
        let wt_path = tmp.path().join("worktrees").join("lost");
        git.worktree_add(&wt_path, "conv/lost").await.unwrap();

        // Simulate a crash that lost the checkout but not the metadata.
        fs::remove_dir_all(&wt_path).unwrap();
        git.worktree_prune().await.unwrap();

        // The path is reusable again afterwards.
        let wt = git.worktree_add(&wt_path, "conv/lost").await.unwrap();
        assert!(wt.dir().join("base.txt").is_file());
    }

    #[tokio::test]
    async fn ff_only_advances_trunk_and_refuses_divergence() {
        let (tmp, git) = repo().await;
        git.branch_create("conv/ff", "main").await.unwrap();
        let wt_path = tmp.path().join("worktrees").join("ff");
        let wt = git.worktree_add(&wt_path, "conv/ff").await.unwrap();
        fs::write(wt_path.join("skill.md"), "notes\n").unwrap();
        let branch_head = wt.add_all_and_commit("skill").await.unwrap().unwrap();

        assert!(git.is_ancestor("main", "conv/ff").await.unwrap());
        let new_main = git.merge_ff_only("conv/ff").await.unwrap();
        assert_eq!(new_main, branch_head);

        // Diverge: a commit on main the branch lacks.
        fs::write(tmp.path().join("trunk.txt"), "trunk\n").unwrap();
        git.add_all_and_commit("trunk work").await.unwrap().unwrap();
        fs::write(wt_path.join("more.md"), "more\n").unwrap();
        wt.add_all_and_commit("more").await.unwrap().unwrap();
        assert!(!git.is_ancestor("main", "conv/ff").await.unwrap());
        assert!(git.merge_ff_only("conv/ff").await.is_err());
    }

    #[tokio::test]
    async fn squash_collapses_history_without_touching_the_tree() {
        let (tmp, git) = repo().await;
        git.branch_create("conv/sq", "main").await.unwrap();
        let wt_path = tmp.path().join("worktrees").join("sq");
        let wt = git.worktree_add(&wt_path, "conv/sq").await.unwrap();

        for n in 0..4 {
            fs::write(wt_path.join(format!("f{n}.txt")), format!("{n}\n")).unwrap();
            wt.add_all_and_commit(&format!("checkpoint {n}")).await.unwrap().unwrap();
        }
        let old_tip = wt.head().await.unwrap();
        let tree_before = String::from_utf8_lossy(
            &wt.run(&["rev-parse", "HEAD^{tree}"]).await.unwrap().stdout,
        )
        .trim()
        .to_string();
        assert_eq!(git.ahead_behind("conv/sq", "main").await.unwrap(), (4, 0));

        let keep = "refs/thetis/presquash/test";
        let squashed = git
            .squash_onto("conv/sq", "main", "one commit\n\nbody", Some(keep))
            .await
            .unwrap();

        // One commit ahead of trunk, same tree, worktree still clean.
        assert_eq!(git.ahead_behind("conv/sq", "main").await.unwrap(), (1, 0));
        let tree_after = String::from_utf8_lossy(
            &git.run(&["rev-parse", "conv/sq^{tree}"]).await.unwrap().stdout,
        )
        .trim()
        .to_string();
        assert_eq!(tree_before, tree_after, "the squash changed no file");
        assert_eq!(wt.head().await.unwrap(), squashed, "the worktree followed");
        assert!(!wt.is_dirty().await.unwrap(), "worktree stays clean");
        for n in 0..4 {
            assert!(wt_path.join(format!("f{n}.txt")).is_file());
        }

        // The old history is rewritten out of the branch but still reachable.
        assert_eq!(git.rev_parse(keep).await.unwrap().as_deref(), Some(old_tip.as_str()));
        assert!(!git.is_ancestor(&old_tip, "conv/sq").await.unwrap());

        // And trunk takes it as a fast-forward, one line long.
        git.merge_ff_only("conv/sq").await.unwrap();
        let log = git.log("main", 10).await.unwrap();
        assert_eq!(log[0].subject, "one commit");
        assert_eq!(log[1].subject, "base", "no checkpoints reached trunk");
    }

    #[tokio::test]
    async fn squash_refuses_when_trunk_is_not_an_ancestor() {
        let (tmp, git) = repo().await;
        git.branch_create("conv/div", "main").await.unwrap();
        let wt_path = tmp.path().join("worktrees").join("div");
        let wt = git.worktree_add(&wt_path, "conv/div").await.unwrap();

        fs::write(wt_path.join("branch.txt"), "b\n").unwrap();
        wt.add_all_and_commit("branch work").await.unwrap().unwrap();
        fs::write(tmp.path().join("trunk.txt"), "t\n").unwrap();
        git.add_all_and_commit("trunk work").await.unwrap().unwrap();

        let err = git
            .squash_onto("conv/div", "main", "nope", None)
            .await
            .expect_err("diverged branches must not be squashed onto trunk");
        assert!(format!("{err:#}").contains("not an ancestor"), "{err:#}");
    }

    #[tokio::test]
    async fn squash_of_one_commit_keeps_the_branch_one_commit_ahead() {
        let (tmp, git) = repo().await;
        git.branch_create("conv/one", "main").await.unwrap();
        let wt_path = tmp.path().join("worktrees").join("one");
        let wt = git.worktree_add(&wt_path, "conv/one").await.unwrap();
        fs::write(wt_path.join("only.txt"), "x\n").unwrap();
        let tip = wt.add_all_and_commit("the only commit").await.unwrap().unwrap();

        // Still squashed to one commit — and the id is unchanged, so nothing
        // was rewritten for no gain.
        let out = git.squash_onto("conv/one", "main", "rewrite", None).await.unwrap();
        assert_ne!(out, tip, "one commit above trunk is still re-parented");
        assert_eq!(git.ahead_behind("conv/one", "main").await.unwrap(), (1, 0));
    }

    #[tokio::test]
    async fn conflicting_merge_reports_paths_and_survives_abort() {
        let (tmp, git) = repo().await;
        git.branch_create("conv/x", "main").await.unwrap();
        let wt_path = tmp.path().join("worktrees").join("x");
        let wt = git.worktree_add(&wt_path, "conv/x").await.unwrap();

        // Both sides edit the same file.
        fs::write(wt_path.join("base.txt"), "branch version\n").unwrap();
        wt.add_all_and_commit("branch edit").await.unwrap().unwrap();
        fs::write(tmp.path().join("base.txt"), "trunk version\n").unwrap();
        git.add_all_and_commit("trunk edit").await.unwrap().unwrap();

        let outcome = wt.merge("main", "update from trunk").await.unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::Conflict {
                paths: vec!["base.txt".to_string()]
            }
        );
        assert!(wt.merge_in_progress().await.unwrap());
        let text = fs::read_to_string(wt_path.join("base.txt")).unwrap();
        assert!(text.contains("<<<<<<<"), "conflict markers present");

        wt.merge_abort().await.unwrap();
        assert!(!wt.merge_in_progress().await.unwrap());
        assert_eq!(
            fs::read_to_string(wt_path.join("base.txt")).unwrap(),
            "branch version\n"
        );
    }

    #[tokio::test]
    async fn resolved_conflict_commits_and_then_fast_forwards() {
        let (tmp, git) = repo().await;
        git.branch_create("conv/y", "main").await.unwrap();
        let wt_path = tmp.path().join("worktrees").join("y");
        let wt = git.worktree_add(&wt_path, "conv/y").await.unwrap();

        fs::write(wt_path.join("base.txt"), "branch\n").unwrap();
        wt.add_all_and_commit("branch edit").await.unwrap().unwrap();
        fs::write(tmp.path().join("base.txt"), "trunk\n").unwrap();
        git.add_all_and_commit("trunk edit").await.unwrap().unwrap();

        let MergeOutcome::Conflict { .. } = wt.merge("main", "update").await.unwrap() else {
            panic!("expected a conflict");
        };

        // Committing with conflicts still unresolved is refused.
        assert!(wt.commit_merge("update from trunk").await.is_err());

        fs::write(wt_path.join("base.txt"), "resolved\n").unwrap();
        wt.commit_merge("update from trunk").await.unwrap();
        assert!(!wt.merge_in_progress().await.unwrap());

        // The branch now contains trunk, so trunk fast-forwards.
        assert!(git.is_ancestor("main", "conv/y").await.unwrap());
        git.merge_ff_only("conv/y").await.unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("base.txt")).unwrap(),
            "resolved\n"
        );
    }

    #[tokio::test]
    async fn clean_merge_from_trunk() {
        let (tmp, git) = repo().await;
        git.branch_create("conv/z", "main").await.unwrap();
        let wt_path = tmp.path().join("worktrees").join("z");
        let wt = git.worktree_add(&wt_path, "conv/z").await.unwrap();

        // Disjoint files merge cleanly.
        fs::write(wt_path.join("branch.txt"), "b\n").unwrap();
        wt.add_all_and_commit("branch file").await.unwrap().unwrap();
        fs::write(tmp.path().join("trunk.txt"), "t\n").unwrap();
        git.add_all_and_commit("trunk file").await.unwrap().unwrap();

        let outcome = wt.merge("main", "update from trunk").await.unwrap();
        assert!(matches!(outcome, MergeOutcome::Clean { .. }));
        assert!(wt_path.join("trunk.txt").is_file());
        let (ahead, behind) = git.ahead_behind("conv/z", "main").await.unwrap();
        assert_eq!(behind, 0, "branch now contains all of trunk");
        assert!(ahead >= 2);
    }

    #[tokio::test]
    async fn log_parses_awkward_subjects() {
        let (tmp, git) = repo().await;
        fs::write(tmp.path().join("f.txt"), "1\n").unwrap();
        git.add_all_and_commit("subject with \"quotes\" | pipes\ttabs")
            .await
            .unwrap()
            .unwrap();
        let log = git.log("main", 10).await.unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].subject, "subject with \"quotes\" | pipes\ttabs");
        assert_eq!(log[1].subject, "base");
        assert_eq!(log[0].author, "test");
        assert!(log[0].ts_ms > 0);
    }

    #[tokio::test]
    async fn tree_oid_tracks_subtree_content_only() {
        let (tmp, git) = repo().await;
        let tool = tmp.path().join("tools").join("demo");
        fs::create_dir_all(&tool).unwrap();
        fs::write(tool.join("lib.rs"), "one\n").unwrap();
        git.add_all_and_commit("tool v1").await.unwrap().unwrap();
        let v1 = git.tree_oid("HEAD", "tools/demo").await.unwrap().unwrap();

        // An unrelated change leaves the subtree oid alone.
        fs::write(tmp.path().join("other.txt"), "x\n").unwrap();
        git.add_all_and_commit("unrelated").await.unwrap().unwrap();
        assert_eq!(
            git.tree_oid("HEAD", "tools/demo").await.unwrap().unwrap(),
            v1
        );

        // A change inside it does not.
        fs::write(tool.join("lib.rs"), "two\n").unwrap();
        git.add_all_and_commit("tool v2").await.unwrap().unwrap();
        assert_ne!(
            git.tree_oid("HEAD", "tools/demo").await.unwrap().unwrap(),
            v1
        );

        assert!(git.tree_oid("HEAD", "no/such/dir").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn checkout_paths_restores_a_subtree() {
        let (tmp, git) = repo().await;
        let tool = tmp.path().join("tools").join("demo");
        fs::create_dir_all(&tool).unwrap();
        fs::write(tool.join("lib.rs"), "good\n").unwrap();
        let green = git.add_all_and_commit("green").await.unwrap().unwrap();

        fs::write(tool.join("lib.rs"), "broken\n").unwrap();
        git.add_all_and_commit("broken").await.unwrap().unwrap();

        git.checkout_paths(&green, &["tools/demo"]).await.unwrap();
        assert_eq!(
            fs::read_to_string(tool.join("lib.rs")).unwrap(),
            "good\n"
        );
    }

    #[tokio::test]
    async fn checked_out_files_look_new_to_the_build_tool() {
        // The revisions module needed touch() so cargo saw restored source as
        // changed; checkout must give the same guarantee for free.
        let (tmp, git) = repo().await;
        let file = tmp.path().join("base.txt");
        let before = fs::metadata(&file).unwrap().modified().unwrap();

        tokio::time::sleep(Duration::from_millis(1100)).await;
        fs::write(&file, "v2\n").unwrap();
        git.add_all_and_commit("v2").await.unwrap().unwrap();
        git.checkout_paths("HEAD~1", &["base.txt"]).await.unwrap();

        let after = fs::metadata(&file).unwrap().modified().unwrap();
        assert!(after > before, "checkout must refresh mtimes");
    }

    #[tokio::test]
    async fn sync_paths_restores_deletes_and_cleans() {
        let (tmp, git) = repo().await;
        let tool = tmp.path().join("tools").join("demo");
        fs::create_dir_all(&tool).unwrap();
        fs::write(tool.join("lib.rs"), "good\n").unwrap();
        let green = git.add_all_and_commit("green").await.unwrap().unwrap();

        // The bad version edits a file, adds a tracked one, and leaves an
        // untracked stray.
        fs::write(tool.join("lib.rs"), "bad\n").unwrap();
        fs::write(tool.join("extra.rs"), "added later\n").unwrap();
        git.add_all_and_commit("bad").await.unwrap().unwrap();
        fs::write(tool.join("stray.tmp"), "scratch\n").unwrap();

        git.sync_paths_to(&green, "tools/demo").await.unwrap();
        assert_eq!(fs::read_to_string(tool.join("lib.rs")).unwrap(), "good\n");
        assert!(!tool.join("extra.rs").exists(), "later additions must go");
        assert!(!tool.join("stray.tmp").exists(), "untracked strays must go");
    }

    #[tokio::test]
    async fn hard_reset_clean_removes_untracked_but_keeps_ignored() {
        let (tmp, git) = repo().await;
        fs::write(tmp.path().join(".gitignore"), "kept-cache/\n").unwrap();
        let base = git.add_all_and_commit("ignore file").await.unwrap().unwrap();

        fs::write(tmp.path().join("base.txt"), "changed\n").unwrap();
        fs::write(tmp.path().join("untracked.txt"), "new\n").unwrap();
        fs::create_dir_all(tmp.path().join("kept-cache")).unwrap();
        fs::write(tmp.path().join("kept-cache/artifact"), "expensive\n").unwrap();

        git.hard_reset_clean(&base).await.unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("base.txt")).unwrap(),
            "base\n"
        );
        assert!(!tmp.path().join("untracked.txt").exists());
        assert!(
            tmp.path().join("kept-cache/artifact").is_file(),
            "ignored build caches survive a reset"
        );
    }

    #[tokio::test]
    async fn rev_parse_distinguishes_real_and_bogus_revs() {
        let (_tmp, git) = repo().await;
        assert!(git.rev_parse("main").await.unwrap().is_some());
        assert!(git.rev_parse("no-such-branch").await.unwrap().is_none());
    }
}
