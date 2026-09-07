//! The branch protocol the browser speaks.
//!
//! `branch-*` frames are handled here, in the gateway process, before the UI
//! guest ever sees them: git and the worker fleet live host-side, and the
//! gateway guest's world is deliberately too small to reach either. Replies
//! go back on the asking socket; after every mutation a fresh `branch-status`
//! is also broadcast to every tab watching the session.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::bindings::branch::BranchState;
use crate::branches::Branches;
use crate::grip::{Grip, RenderedFrame, Role};
use crate::workers::{PENDING_BASE_KEY, WorkerRouter, call_session};

/// True when this frame type belongs to the branch protocol.
pub fn handles(frame_type: &str) -> bool {
    frame_type.starts_with("branch-")
}

/// Handles one frame, returning the reply frames to send on this socket.
pub async fn handle(grip: &Arc<Grip>, frame: &Value) -> Vec<String> {
    let frame_type = frame
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let session = frame
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    match dispatch(grip, &frame_type, &session, frame).await {
        Ok(replies) => replies,
        Err(e) => {
            let op = frame_type.trim_start_matches("branch-").to_string();
            vec![
                json!({
                    "type": "branch-result",
                    "session": session,
                    "op": op,
                    "ok": false,
                    "state": "error",
                    "conflicts": [],
                    "message": format!("{e:#}"),
                })
                .to_string(),
            ]
        }
    }
}

async fn dispatch(
    grip: &Arc<Grip>,
    frame_type: &str,
    session: &str,
    frame: &Value,
) -> Result<Vec<String>> {
    let Role::Gateway(router) = &grip.role else {
        anyhow::bail!("branch frames are a gateway concern");
    };
    let router = router.clone();

    match frame_type {
        "branch-status" => Ok(vec![status_frame(grip, &router, session).await?]),

        // Read-only history. A click on the history panel must never
        // materialize a worker: `call_session` would create the branch, the
        // worktree and the process, and the tab would sit there for as long as
        // that took. Only ask the worker when there already is one — it knows
        // about commits the shared refs have not caught up with — and read the
        // refs directly otherwise, exactly as `branch-graph` does.
        "branch-log" => {
            let limit = frame.get("limit").and_then(Value::as_u64).unwrap_or(50);
            let live = router.live_sessions().await.contains(&session.to_string());
            let commits: Value = if live {
                call_session(
                    grip,
                    &router,
                    session,
                    "branch.log",
                    json!({ "limit": limit }),
                )
                .await?
            } else {
                let store = grip.local_store().context("gateway only")?;
                let branches = Branches::new(grip.cfg.clone(), store.clone());
                match branches.get(session)? {
                    Some(row) => {
                        let root = branches.root_git();
                        let trunk = root.current_branch().await?;
                        let commits = root
                            .log_args(&[&row.branch_ref], limit as usize)
                            .await
                            .unwrap_or_default();
                        // Same shape the worker answers with, `on_trunk` and
                        // all — the client cannot tell the two apart.
                        let mut out = Vec::with_capacity(commits.len());
                        for c in commits {
                            let on_trunk = root.is_ancestor(&c.rev, &trunk).await.unwrap_or(false);
                            out.push(json!({
                                "rev": c.rev,
                                "subject": c.subject,
                                "author": c.author,
                                "ts_ms": c.ts_ms,
                                "on_trunk": on_trunk,
                            }));
                        }
                        Value::Array(out)
                    }
                    None => json!([]),
                }
            };
            Ok(vec![
                json!({
                    "type": "branch-log",
                    "session": session,
                    "commits": commits,
                })
                .to_string(),
            ])
        }

        // Everything a commit graph needs, computed from shared refs alone —
        // no worker involved, so it works for stopped conversations too and
        // costs the running turn nothing.
        "branch-graph" => {
            let store = grip.local_store().context("gateway only")?;
            let branches = Branches::new(grip.cfg.clone(), store.clone());
            let root = branches.root_git();
            let trunk_name = root.current_branch().await?;
            let limit = frame
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(30)
                .clamp(5, 200) as usize;

            let row = branches.get(session)?;
            let (branch_name, base, branch_commits) = match &row {
                Some(row) => {
                    let base = root
                        .merge_base(&trunk_name, &row.branch_ref)
                        .await?
                        .unwrap_or_else(|| row.base_commit.clone());
                    // Only what the branch has that trunk lacks; the shared
                    // spine is drawn from the trunk lane.
                    let not_trunk = format!("^{trunk_name}");
                    let commits = root
                        .log_args(&[&row.branch_ref, &not_trunk], limit)
                        .await
                        .unwrap_or_default();
                    (Some(row.branch_ref.clone()), Some(base), commits)
                }
                None => (None, None, Vec::new()),
            };

            let trunk_commits = root.log_args(&[&trunk_name], limit).await?;
            let commit_json = |c: &crate::gitctl::CommitInfo| {
                json!({
                    "rev": c.rev,
                    "subject": c.subject,
                    "author": c.author,
                    "ts_ms": c.ts_ms,
                    "parents": c.parents,
                })
            };

            Ok(vec![
                json!({
                    "type": "branch-graph",
                    "session": session,
                    "trunk_name": trunk_name,
                    "branch_name": branch_name,
                    "base": base,
                    "trunk": trunk_commits.iter().map(commit_json).collect::<Vec<_>>(),
                    "branch": branch_commits.iter().map(commit_json).collect::<Vec<_>>(),
                })
                .to_string(),
            ])
        }

        "branch-trunk-log" => {
            let limit = frame.get("limit").and_then(Value::as_u64).unwrap_or(30) as usize;
            let root = crate::gitctl::GitCtl::new(grip.cfg.root.clone());
            let commits: Vec<Value> = root
                .log("HEAD", limit.clamp(1, 200))
                .await?
                .into_iter()
                .map(|c| {
                    json!({
                        "rev": c.rev,
                        "subject": c.subject,
                        "author": c.author,
                        "ts_ms": c.ts_ms,
                    })
                })
                .collect();
            Ok(vec![
                json!({ "type": "branch-trunk-log", "commits": commits }).to_string(),
            ])
        }

        // The user picked a starting revision before their first message.
        "branch-base" => {
            let store = grip.local_store().context("gateway only")?;
            let branches = Branches::new(grip.cfg.clone(), store.clone());
            if branches.get(session)?.is_some() {
                anyhow::bail!(
                    "this conversation already has its branch; the starting point is fixed"
                );
            }
            let revision = frame
                .get("revision")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !revision.is_empty() {
                let root = branches.root_git();
                root.rev_parse(revision)
                    .await?
                    .with_context(|| format!("'{revision}' does not name a trunk revision"))?;
            }
            store.kv_put(session, PENDING_BASE_KEY, revision)?;
            Ok(vec![result_frame(
                session,
                "base",
                true,
                "clean",
                &[],
                "starting point set",
            )])
        }

        "branch-merge" => {
            match crate::merge::merge_to_trunk(grip, &router, session).await? {
                crate::merge::MergeResult::Merged { to, .. } => {
                    let mut replies = vec![result_frame(
                        session,
                        "merge",
                        true,
                        "clean",
                        &[],
                        &format!("merged; trunk is now at {}", &to[..12.min(to.len())]),
                    )];
                    replies.push(broadcast_status(grip, &router, session).await);
                    // Other conversations' behind-counts changed too.
                    push_status_to_live_sessions(grip, &router, session).await;
                    Ok(replies)
                }
                crate::merge::MergeResult::Conflicts(state) => Ok(vec![
                    result_frame(
                        session,
                        "merge",
                        false,
                        "conflict",
                        &state.conflicts,
                        "trunk changed under this branch and the update hit conflicts; \
                         resolve them (or hand them to the conversation), then merge again",
                    ),
                    broadcast_status(grip, &router, session).await,
                ]),
            }
        }

        "branch-update" => {
            let state: BranchState = serde_json::from_value(
                call_session(
                    grip,
                    &router,
                    session,
                    "branch.update",
                    json!({ "session": session }),
                )
                .await?,
            )?;
            let ok = state.state != "conflict";
            Ok(vec![
                result_frame(
                    session,
                    "update",
                    ok,
                    &state.state,
                    &state.conflicts,
                    &state.detail,
                ),
                broadcast_status(grip, &router, session).await,
            ])
        }

        "branch-reset" => {
            let rev = frame
                .get("rev")
                .and_then(Value::as_str)
                .context("missing 'rev'")?;
            let state: BranchState = serde_json::from_value(
                call_session(
                    grip,
                    &router,
                    session,
                    "branch.reset",
                    json!({ "session": session, "rev": rev }),
                )
                .await?,
            )?;
            Ok(vec![
                result_frame(session, "reset", true, &state.state, &[], &state.detail),
                broadcast_status(grip, &router, session).await,
            ])
        }

        "branch-resolve" => {
            crate::merge::resolve_in_conversation(grip, &router, session).await?;
            Ok(vec![result_frame(
                session,
                "resolve",
                true,
                "conflict",
                &[],
                "handed to the conversation; the agent is resolving the conflicts",
            )])
        }

        "branch-abort" => {
            let state: BranchState = serde_json::from_value(
                call_session(
                    grip,
                    &router,
                    session,
                    "branch.abort",
                    json!({ "session": session }),
                )
                .await?,
            )?;
            Ok(vec![
                result_frame(session, "abort", true, &state.state, &[], &state.detail),
                broadcast_status(grip, &router, session).await,
            ])
        }

        other => anyhow::bail!("unknown branch frame '{other}'"),
    }
}

/// The status frame for a session — from its worker when one is live, from
/// refs alone when not (a stopped conversation should not cost a worker
/// spawn just to draw a panel).
async fn status_frame(
    grip: &Arc<Grip>,
    router: &Arc<WorkerRouter>,
    session: &str,
) -> Result<String> {
    let store = grip.local_store().context("gateway only")?;
    let branches = Branches::new(grip.cfg.clone(), store.clone());
    let root = branches.root_git();
    let trunk_head = root.head().await.unwrap_or_default();

    let Some(row) = branches.get(session)? else {
        // Not materialized: the composer's picker is the interesting surface.
        return Ok(json!({
            "type": "branch-status",
            "session": session,
            "materialized": false,
            "trunk": { "rev": trunk_head },
        })
        .to_string());
    };

    let live = router.live_sessions().await.contains(&session.to_string());
    let (state, recent) = if live {
        let state: BranchState = serde_json::from_value(
            call_session(grip, router, session, "branch.status", json!({})).await?,
        )?;
        let recent: Value =
            call_session(grip, router, session, "branch.log", json!({ "limit": 3 }))
                .await
                .unwrap_or(Value::Array(Vec::new()));
        (state, recent)
    } else {
        // Refs only. No dirtiness or conflict detail without the worktree's
        // worker, but position is exact.
        let trunk = root
            .current_branch()
            .await
            .unwrap_or_else(|_| "main".into());
        let (ahead, behind) = root
            .ahead_behind(&row.branch_ref, &trunk)
            .await
            .unwrap_or((0, 0));
        let head = root
            .rev_parse(&row.branch_ref)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let recent: Vec<Value> = root
            .log(&row.branch_ref, 3)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|c| {
                json!({ "rev": c.rev, "subject": c.subject, "author": c.author, "ts_ms": c.ts_ms })
            })
            .collect();
        (
            BranchState {
                branch: row.branch_ref.clone(),
                state: "idle".to_string(),
                ahead: ahead as u32,
                behind: behind as u32,
                head_rev: head,
                trunk_rev: trunk_head.clone(),
                conflicts: Vec::new(),
                detail: String::new(),
            },
            Value::Array(recent),
        )
    };

    Ok(json!({
        "type": "branch-status",
        "session": session,
        "materialized": true,
        "branch": state.branch,
        "state": state.state,
        "ahead": state.ahead,
        "behind": state.behind,
        "head": { "rev": state.head_rev },
        "trunk": { "rev": state.trunk_rev },
        "base": { "rev": row.base_commit },
        "conflicts": state.conflicts,
        "recent": recent,
    })
    .to_string())
}

/// Broadcasts a fresh status to every tab watching the session, returning it
/// so the asking socket gets it first.
async fn broadcast_status(grip: &Arc<Grip>, router: &Arc<WorkerRouter>, session: &str) -> String {
    match status_frame(grip, router, session).await {
        Ok(frame) => {
            let _ = grip.frames_tx.send(RenderedFrame {
                session_id: session.to_string(),
                frame: frame.clone(),
            });
            frame
        }
        Err(e) => json!({
            "type": "branch-result",
            "session": session,
            "op": "status",
            "ok": false,
            "state": "error",
            "conflicts": [],
            "message": format!("{e:#}"),
        })
        .to_string(),
    }
}

/// Trunk moved: every live conversation's behind-count just changed. Pushed,
/// never pulled — the panel updates without polling.
async fn push_status_to_live_sessions(grip: &Arc<Grip>, router: &Arc<WorkerRouter>, except: &str) {
    for session in router.live_sessions().await {
        if session == except {
            continue;
        }
        let _ = broadcast_status(grip, router, &session).await;
    }
}

fn result_frame(
    session: &str,
    op: &str,
    ok: bool,
    state: &str,
    conflicts: &[String],
    message: &str,
) -> String {
    json!({
        "type": "branch-result",
        "session": session,
        "op": op,
        "ok": ok,
        "state": state,
        "conflicts": conflicts,
        "message": message,
    })
    .to_string()
}
