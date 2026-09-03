//! The branch registry: which sandbox backs which conversation.
//!
//! Every conversation runs against its own git branch (`conv/<short-id>`)
//! checked out in its own worktree. Materialization is lazy — a session gets
//! its branch at its first message, pinned to trunk's head (or an explicitly
//! chosen revision) at that moment — so empty conversations cost nothing.
//!
//! All of this is gateway-side: the gateway owns the root checkout and the
//! ref namespace, and workers only ever touch the worktree they were handed.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Config;
use crate::gitctl::GitCtl;
use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchState {
    /// Live: a worker may be running it.
    Active,
    /// A merge or trunk update stopped on conflicts that are not yet resolved.
    Merging,
    /// The conversation was archived; the branch is kept until pruned.
    Archived,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchRow {
    pub session_id: String,
    /// The git ref, e.g. `conv/3fa2c1d8`.
    pub branch_ref: String,
    pub worktree: PathBuf,
    /// The trunk commit the branch started from.
    pub base_commit: String,
    pub state: BranchState,
    /// The kernel commit this branch's worker runs (empty = trunk binary).
    #[serde(default)]
    pub kernel_commit: String,
    pub created_ms: u64,
}

/// Owns branch lifecycle against the root checkout.
pub struct Branches {
    cfg: Arc<Config>,
    store: Arc<Store>,
    /// The root checkout, always on trunk. Ref-level operations only.
    root: GitCtl,
}

impl Branches {
    pub fn new(cfg: Arc<Config>, store: Arc<Store>) -> Self {
        let root = GitCtl::new(&cfg.root);
        Self { cfg, store, root }
    }

    pub fn root_git(&self) -> &GitCtl {
        &self.root
    }

    pub fn get(&self, session_id: &str) -> Result<Option<BranchRow>> {
        self.store.get_branch(session_id)
    }

    pub fn list(&self) -> Result<Vec<BranchRow>> {
        self.store.list_branches()
    }

    pub fn update(&self, row: &BranchRow) -> Result<()> {
        self.store.put_branch(row)
    }

    /// The branch name for a session. Short but disambiguated: the first id
    /// segment reads well in a UI, the full id never collides.
    fn branch_name(session_id: &str) -> String {
        let short: String = session_id.chars().take(8).collect();
        format!("conv/{short}")
    }

    /// Returns the session's branch, creating branch + worktree at `base`
    /// (default: trunk head) on first use.
    pub async fn ensure(&self, session_id: &str, base: Option<&str>) -> Result<BranchRow> {
        if let Some(existing) = self.store.get_branch(session_id)? {
            // The registry can outlive its checkout (a crash mid-cleanup, a
            // hand-deleted directory). Recreate rather than refuse: the
            // branch ref is the durable thing.
            if !existing.worktree.is_dir() {
                self.root.worktree_prune().await?;
                self.root
                    .worktree_add(&existing.worktree, &existing.branch_ref)
                    .await
                    .with_context(|| format!("recreating the checkout for {session_id}"))?;
            }
            return Ok(existing);
        }

        let branch_ref = Self::branch_name(session_id);
        let base_commit = match base {
            Some(rev) => self
                .root
                .rev_parse(rev)
                .await?
                .with_context(|| format!("'{rev}' does not name a trunk revision"))?,
            None => self.root.head().await?,
        };

        if !self.root.branch_exists(&branch_ref).await? {
            self.root.branch_create(&branch_ref, &base_commit).await?;
        }

        let worktree = self.cfg.paths.worktrees.join(&branch_ref.replace('/', "-"));
        if !worktree.is_dir() {
            std::fs::create_dir_all(&self.cfg.paths.worktrees)?;
            self.root.worktree_add(&worktree, &branch_ref).await?;
        }

        let row = BranchRow {
            session_id: session_id.to_string(),
            branch_ref,
            worktree,
            base_commit,
            state: BranchState::Active,
            kernel_commit: String::new(),
            created_ms: crate::store::now_ms(),
        };
        self.store.put_branch(&row)?;
        tracing::info!(
            session = %session_id,
            branch = %row.branch_ref,
            base = %&row.base_commit[..12.min(row.base_commit.len())],
            "materialized a sandbox branch"
        );
        Ok(row)
    }

    /// Removes a branch's checkout and registry row, keeping the ref itself —
    /// commits stay reachable until someone deletes the branch deliberately.
    pub async fn release_worktree(&self, session_id: &str) -> Result<()> {
        let Some(row) = self.store.get_branch(session_id)? else {
            return Ok(());
        };
        if row.worktree.is_dir() {
            self.root.worktree_remove(&row.worktree).await?;
        }
        self.root.worktree_prune().await?;
        Ok(())
    }

    /// The boot sweep: drop bookkeeping for checkouts lost to a crash.
    pub async fn reconcile_on_boot(&self) -> Result<()> {
        self.root.worktree_prune().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn fixture() -> (TempDir, Branches) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();

        // A minimal repo standing in for the thetis tree.
        let git = GitCtl::new(&root);
        git.run_raw(&["init", "-b", "main"]).await.unwrap();
        git.run_raw(&["config", "user.name", "test"]).await.unwrap();
        git.run_raw(&["config", "user.email", "t@example.com"])
            .await
            .unwrap();
        std::fs::write(root.join("file.txt"), "trunk\n").unwrap();
        git.add_all_and_commit("base").await.unwrap();

        let mut cfg = Config::load().unwrap();
        cfg.root = root.clone();
        cfg.paths.worktrees = root.join("worktrees");
        cfg.paths.data = root.join("data");

        let store = Arc::new(Store::open(&cfg.db_path()).unwrap());
        (dir, Branches::new(Arc::new(cfg), store))
    }

    #[tokio::test]
    async fn ensure_is_lazy_idempotent_and_pins_the_base() {
        let (_dir, branches) = fixture().await;

        let first = branches.ensure("abcd1234-rest-of-id", None).await.unwrap();
        assert_eq!(first.branch_ref, "conv/abcd1234");
        assert!(first.worktree.join("file.txt").is_file());

        // Trunk moves on; the existing branch keeps its base.
        let root = branches.root_git();
        std::fs::write(root.dir().join("file.txt"), "trunk v2\n").unwrap();
        root.add_all_and_commit("advance trunk").await.unwrap();

        let again = branches.ensure("abcd1234-rest-of-id", None).await.unwrap();
        assert_eq!(again.base_commit, first.base_commit);
        assert_eq!(
            std::fs::read_to_string(again.worktree.join("file.txt")).unwrap(),
            "trunk\n",
            "the sandbox still sees the world as of its base"
        );
    }

    #[tokio::test]
    async fn ensure_can_start_from_an_older_revision() {
        let (_dir, branches) = fixture().await;
        let root = branches.root_git();
        let old = root.head().await.unwrap();
        std::fs::write(root.dir().join("file.txt"), "newer\n").unwrap();
        root.add_all_and_commit("newer").await.unwrap();

        let row = branches.ensure("11112222-x", Some(&old)).await.unwrap();
        assert_eq!(row.base_commit, old);
        assert_eq!(
            std::fs::read_to_string(row.worktree.join("file.txt")).unwrap(),
            "trunk\n"
        );
    }

    #[tokio::test]
    async fn a_lost_checkout_is_recreated_from_the_branch_ref() {
        let (_dir, branches) = fixture().await;
        let row = branches.ensure("deadbeef-x", None).await.unwrap();

        // Work committed on the branch survives losing the directory.
        std::fs::write(row.worktree.join("work.txt"), "precious\n").unwrap();
        GitCtl::new(&row.worktree)
            .add_all_and_commit("work")
            .await
            .unwrap();
        std::fs::remove_dir_all(&row.worktree).unwrap();

        let recovered = branches.ensure("deadbeef-x", None).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(recovered.worktree.join("work.txt")).unwrap(),
            "precious\n"
        );
    }
}
