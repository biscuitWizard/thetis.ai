//! Branch operations, executed inside the worker that owns the worktree.
//!
//! Both surfaces land here: the agent's `branch` host interface and the
//! gateway's user-initiated requests (relayed over IPC). Keeping every
//! content-level git mutation in the worker preserves the one-writer-per-
//! worktree rule, keeps watcher suppression in-process with the watcher, and
//! means agent- and user-initiated operations behave identically.
//!
//! Merging to trunk is deliberately not here: trunk only ever moves in the
//! gateway, by fast-forward, on a human's say-so.

use anyhow::{Result, anyhow, bail};
use std::sync::Arc;

use crate::bindings::branch::{BranchState, CommitInfo};
use crate::bindings::types::{BranchOp, SessionEvent};
use crate::gitctl::MergeOutcome;
use crate::grip::Grip;
use crate::pipeline;
use crate::revisions::Origin;

/// The trunk branch every conversation forks from and merges back to: the
/// branch the gateway's root checkout is on, pinned into workers over the
/// environment at spawn. Defaults to "main".
pub fn trunk_ref() -> String {
    std::env::var("THETIS_TRUNK")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "main".to_string())
}

fn git(grip: &Arc<Grip>) -> Result<&crate::gitctl::GitCtl> {
    grip.git
        .as_ref()
        .ok_or_else(|| anyhow!("this process has no checkout"))
}

pub async fn status(grip: &Arc<Grip>) -> Result<BranchState> {
    let git = git(grip)?;
    let branch = git.current_branch().await?;
    let head = git.head().await?;
    let trunk = git.rev_parse(&trunk_ref()).await?.unwrap_or_default();
    let (ahead, behind) = git
        .ahead_behind("HEAD", &trunk_ref())
        .await
        .unwrap_or((0, 0));
    let merging = git.merge_in_progress().await?;
    let conflicts = if merging {
        git.unmerged_paths().await?
    } else {
        Vec::new()
    };
    let dirty = git.is_dirty().await?;

    let state = if merging {
        "conflict"
    } else if dirty {
        "dirty"
    } else {
        "clean"
    };

    Ok(BranchState {
        branch,
        state: state.to_string(),
        ahead: ahead as u32,
        behind: behind as u32,
        head_rev: head,
        trunk_rev: trunk,
        conflicts,
        detail: String::new(),
    })
}

pub async fn log(grip: &Arc<Grip>, limit: u32) -> Result<Vec<CommitInfo>> {
    let git = git(grip)?;
    let limit = limit.clamp(1, 200) as usize;
    let mut out = Vec::new();
    for commit in git.log("HEAD", limit).await? {
        let on_trunk = git
            .is_ancestor(&commit.rev, &trunk_ref())
            .await
            .unwrap_or(false);
        out.push(CommitInfo {
            rev: commit.rev,
            subject: commit.subject,
            author: commit.author,
            ts_ms: commit.ts_ms,
            on_trunk,
        });
    }
    Ok(out)
}

/// Brings the latest trunk into this branch — the "pull". A fast-forward when
/// the branch has nothing of its own, an ordinary merge otherwise; conflicts
/// come back as state "conflict" with the markers left in the working tree.
pub async fn update_from_trunk(grip: &Arc<Grip>, session_id: &str) -> Result<BranchState> {
    let git = git(grip)?;
    commit_dirty(grip, "checkpoint: before trunk update").await?;

    if git.merge_in_progress().await? {
        bail!("a merge is already in progress; resolve or abort it first");
    }

    let before = git.head().await?;
    let (_, behind) = git.ahead_behind("HEAD", &trunk_ref()).await?;
    if behind == 0 {
        let mut state = status(grip).await?;
        state.detail = "already up to date with trunk".to_string();
        return Ok(state);
    }

    // The merge rewrites source files; without suppression the watcher would
    // race it with rebuilds of half-merged trees.
    grip.suppress_watch_all(grip.cfg().watchdog.watch_suppression);

    match git.merge(&trunk_ref(), "update from trunk").await? {
        MergeOutcome::Clean { head } => {
            let rebuilt = refresh_everything(grip, &before, session_id, "trunk update").await;
            grip.skills.invalidate();
            append_op(
                grip,
                session_id,
                BranchOp {
                    op: "update".into(),
                    ok: true,
                    from_rev: before,
                    to_rev: head,
                    conflicts: Vec::new(),
                    detail: rebuilt,
                },
            )
            .await;
            status(grip).await
        }
        MergeOutcome::Conflict { paths } => {
            append_op(
                grip,
                session_id,
                BranchOp {
                    op: "update".into(),
                    ok: false,
                    from_rev: before.clone(),
                    to_rev: String::new(),
                    conflicts: paths.clone(),
                    detail: "trunk update stopped on conflicts".into(),
                },
            )
            .await;
            let mut state = status(grip).await?;
            state.detail = format!(
                "the merge stopped on {} conflicted file(s); resolve them and call \
                 complete-merge, or abort-merge to give up",
                paths.len()
            );
            Ok(state)
        }
    }
}

/// Restores the whole branch to how it looked at `rev`, as a new commit.
/// History is preserved — this is the rollback replacement, and like the old
/// rollback it only ever moves forward.
pub async fn reset_to(grip: &Arc<Grip>, session_id: &str, rev: &str) -> Result<BranchState> {
    let git = git(grip)?;
    if git.merge_in_progress().await? {
        bail!("a merge is in progress; resolve or abort it before resetting");
    }
    let target = git
        .rev_parse(rev)
        .await?
        .ok_or_else(|| anyhow!("'{rev}' does not name a commit"))?;
    let before = git.head().await?;

    grip.suppress_watch_all(grip.cfg().watchdog.watch_suppression);
    git.sync_paths_to(&target, ".").await?;
    let short = &target[..12.min(target.len())];
    let commit = grip
        .commit_worktree(&format!("reset: branch restored to {short}"))
        .await?;

    let rebuilt = refresh_everything(grip, &before, session_id, "branch reset").await;
    grip.skills.invalidate();
    append_op(
        grip,
        session_id,
        BranchOp {
            op: "reset".into(),
            ok: true,
            from_rev: before,
            to_rev: commit.unwrap_or(target),
            conflicts: Vec::new(),
            detail: rebuilt,
        },
    )
    .await;
    status(grip).await
}

/// Commits a conflicted merge after its conflicts were resolved in the
/// working tree, then rebuilds whatever the merge changed.
pub async fn complete_merge(
    grip: &Arc<Grip>,
    session_id: &str,
    message: Option<String>,
) -> Result<BranchState> {
    let git = git(grip)?;
    if !git.merge_in_progress().await? {
        bail!("there is no merge in progress to complete");
    }
    let before = git.head().await?;
    let message = message
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "update from trunk (conflicts resolved)".to_string());

    grip.suppress_watch_all(grip.cfg().watchdog.watch_suppression);
    let head = git.commit_merge(&message).await?;
    let rebuilt = refresh_everything(grip, &before, session_id, "resolved trunk update").await;
    grip.skills.invalidate();
    append_op(
        grip,
        session_id,
        BranchOp {
            op: "merge-completed".into(),
            ok: true,
            from_rev: before,
            to_rev: head,
            conflicts: Vec::new(),
            detail: rebuilt,
        },
    )
    .await;
    status(grip).await
}

pub async fn abort_merge(grip: &Arc<Grip>, session_id: &str) -> Result<BranchState> {
    let git = git(grip)?;
    if !git.merge_in_progress().await? {
        bail!("there is no merge in progress to abort");
    }
    let head = git.head().await?;
    grip.suppress_watch_all(grip.cfg().watchdog.watch_suppression);
    git.merge_abort().await?;
    append_op(
        grip,
        session_id,
        BranchOp {
            op: "abort".into(),
            ok: true,
            from_rev: head.clone(),
            to_rev: head,
            conflicts: Vec::new(),
            detail: "merge aborted; the branch is back to its pre-merge state".into(),
        },
    )
    .await;
    status(grip).await
}

/// Checkpoints any uncommitted work — run before every branch-level operation
/// so nothing in the working tree can be silently swept into a merge.
pub async fn commit_dirty(grip: &Arc<Grip>, message: &str) -> Result<()> {
    grip.commit_worktree(message).await?;
    Ok(())
}

/// Everything a commit landing in this worktree has to bring up to date:
/// guest aspects by hot swap, and the branch's own kernel by rebuild and
/// restart. Every operation that moves HEAD goes through here, so no path can
/// quietly leave half the system running the old code.
async fn refresh_everything(
    grip: &Arc<Grip>,
    before: &str,
    session_id: &str,
    cause: &str,
) -> String {
    let mut parts = Vec::new();
    let aspects = refresh_changed_aspects(grip, before, session_id).await;
    if !aspects.is_empty() {
        parts.push(aspects);
    }
    if let Some(note) = refresh_branch_kernel(grip, before, session_id, cause).await {
        parts.push(note);
    }
    parts.join("; ")
}

/// Rebuilds this branch's own kernel after a commit moved the code it was
/// built from, and restarts the conversation onto it.
///
/// Only for a branch that *has* its own kernel. A conversation running trunk's
/// binary is already running trunk's code, so a merge leaves it nothing to
/// catch up on, and a host build on every trunk update would be an absurd toll
/// on the common case.
///
/// The build runs in a task of its own because it takes minutes and the RPC
/// that asked for this update gives up after sixty seconds. That is also why
/// the failure path appends an incident: nobody is still waiting on a return
/// value to read it in.
async fn refresh_branch_kernel(
    grip: &Arc<Grip>,
    before: &str,
    session_id: &str,
    cause: &str,
) -> Option<String> {
    let built = crate::control::kernel_binary(&grip.cfg().root);
    if !built.is_file() {
        return None;
    }

    let git = git(grip).ok()?;
    if !crate::control::kernel_source_moved(git, before, "HEAD").await {
        return None;
    }

    let grip = grip.clone();
    let session = session_id.to_string();
    let cause = cause.to_string();
    tokio::spawn(async move {
        match crate::control::build_kernel(&grip.cfg()).await {
            // A build already running is the build this wanted. It will
            // restart on its own when it lands, so saying anything here would
            // only contradict it.
            Ok(crate::control::KernelBuild::Busy(why)) => {
                tracing::info!(%why, "a kernel build was already running; leaving it to finish");
            }
            Ok(crate::control::KernelBuild::Built(_)) => {
                if let Err(e) = crate::control::request_restart(
                    &grip,
                    &format!(
                        "{cause} moved the orchestrator's own code; \
                         restarting on the rebuilt kernel"
                    ),
                    true,
                    Some(&session),
                )
                .await
                {
                    tracing::warn!(error = %e, "the rebuilt kernel was not picked up");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "rebuilding this branch's kernel failed");
                let _ = grip
                    .append_event(
                        &session,
                        crate::bindings::types::SessionEvent::Incident(format!(
                            "The {cause} landed, but rebuilding this conversation's own \
                             kernel failed, so it is still running the one built before it: {e:#}"
                        )),
                    )
                    .await;
            }
        }
    });

    Some("its kernel is rebuilding, and the conversation will restart onto it".to_string())
}

/// Brings the branch's guest aspects up to date with what the merge just brought
/// in.
///
/// Cache hits are installed inline — they are near-instant, and the caller
/// wants to hear about them. Anything needing a compile is handed to a task and
/// reported through the session log instead, because a compile takes minutes
/// and the RPC that asked for this update gives up after sixty seconds. Awaited
/// inline, a merge touching `wit/` (which invalidates *every* aspect's cache key
/// at once) reliably blew the deadline and reported a failure for an update
/// that had in fact landed.
async fn refresh_changed_aspects(grip: &Arc<Grip>, before: &str, session_id: &str) -> String {
    let mut refreshed: Vec<String> = Vec::new();
    // Kept for the cache-hit path's own failures; compiles report through the log.
    let failed: Vec<String> = Vec::new();
    let mut deferred = Vec::new();

    for aspect in pipeline::discover_aspects(&grip.cfg()) {
        let old_key = pipeline::aspect_cache_key(grip, before, &aspect).await;
        let new_key = pipeline::aspect_cache_key(grip, "HEAD", &aspect).await;
        if old_key == new_key {
            continue;
        }

        // Cache first: the merged tree is very often one trunk already built.
        if let Some(key) = &new_key {
            if let Ok(Some(meta)) = grip.buildcache.lookup(&aspect.key(), key) {
                let loaded = grip
                    .buildcache
                    .artifact_path(&meta, pipeline::CACHE_ARTIFACT)
                    .and_then(|artifact| {
                        crate::loader::Loader::compile(
                            &grip.runtime.engine,
                            &aspect,
                            pipeline::key_revision(key),
                            &artifact,
                        )
                    });
                if let Ok(component) = loaded {
                    grip.install_component(component).await;
                    refreshed.push(format!("{aspect} (cached)"));
                    continue;
                }
            }
        }

        deferred.push(aspect);
    }

    if !deferred.is_empty() {
        let names: Vec<String> = deferred.iter().map(|s| s.key()).collect();
        let grip = grip.clone();
        let session = session_id.to_string();
        tokio::spawn(async move {
            let mut done = Vec::new();
            let mut broke = Vec::new();
            for aspect in deferred {
                match pipeline::build_and_activate(
                    &grip,
                    &aspect,
                    Origin::HumanEdit,
                    "trunk update",
                )
                .await
                {
                    Ok(outcome) if outcome.success => done.push(aspect.key()),
                    Ok(outcome) => broke.push(format!("{aspect}: {}", outcome.detail)),
                    Err(e) => broke.push(format!("{aspect}: {e:#}")),
                }
            }
            // Nobody is still holding a return value, so the result has to
            // arrive in the log.
            let mut note = String::new();
            if !done.is_empty() {
                note.push_str(&format!("Rebuilt after the update: {}.", done.join(", ")));
            }
            if !broke.is_empty() {
                if !note.is_empty() {
                    note.push(' ');
                }
                note.push_str(&format!(
                    "These did NOT rebuild and are still serving the previous \
                     revision: {}.",
                    broke.join("; ")
                ));
            }
            if !note.is_empty() {
                let _ = grip
                    .append_event(&session, SessionEvent::Incident(note))
                    .await;
            }
        });
        refreshed.push(format!("rebuilding {}", names.join(", ")));
    }

    let mut parts = Vec::new();
    if !refreshed.is_empty() {
        parts.push(format!("refreshed {}", refreshed.join(", ")));
    }
    if !failed.is_empty() {
        parts.push(format!("FAILED {}", failed.join("; ")));
    }
    parts.join("; ")
}

async fn append_op(grip: &Arc<Grip>, session_id: &str, op: BranchOp) {
    if let Err(e) = grip
        .append_event(session_id, SessionEvent::BranchOp(op))
        .await
    {
        tracing::warn!(error = %e, "branch operation was not logged");
    }
}
