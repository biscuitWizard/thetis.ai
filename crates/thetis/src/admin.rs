//! The operator's controls, as plain host functions.
//!
//! Two surfaces render these: the host-drawn `/admin` page in `web.rs`, which
//! has no WebAssembly in its path and is the recovery console, and the `admin`
//! interface the gateway guest imports for the control panel in the chat UI.
//! Both call here, so a control exists once and behaves the same whichever way
//! it was reached.
//!
//! Nothing in this module checks who is asking. The HTTP middleware gates
//! `/admin` on `Principal::is_admin`, and the host import gates every call the
//! same way (`require_admin` in `host_api.rs`); by the time a function here
//! runs, the caller is an administrator.

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::grip::Grip;

/// One trunk commit, newest first.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CommitRow {
    pub rev: String,
    pub subject: String,
    pub author: String,
    pub head: bool,
}

/// One conversation's branch and worker.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BranchRow {
    pub session_id: String,
    pub title: String,
    pub branch_ref: String,
    pub live: bool,
    pub ahead: u32,
    pub behind: u32,
    pub state: String,
    /// The kernel commit this conversation runs on, or `trunk`.
    pub kernel: String,
}

/// One configured account, with what the database says about it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountRow {
    pub id: String,
    pub name: String,
    pub role: String,
    pub admin: bool,
    pub read_only: bool,
    pub sees_all: bool,
    pub conversations: u32,
    pub logins: u32,
    pub spend_usd: f64,
}

/// Everything the overview shows, gathered in one pass.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Overview {
    pub trunk_name: String,
    pub trunk_head: String,
    pub commits: Vec<CommitRow>,
    pub branches: Vec<BranchRow>,
    pub accounts: Vec<AccountRow>,
    pub private_dirs: Vec<String>,
    pub sessions: u64,
    /// Local mode: one implicit administrator and no accounts.
    pub local_mode: bool,
    pub admin_enabled: bool,
    pub restart_available: bool,
    pub config_path: String,
    pub overlay_path: String,
}

/// A manual override, described for whoever draws the button.
///
/// The table is what makes a new control cheap: an entry here, an arm in
/// [`act`], and every surface offers it — the HTML page and the control panel
/// both draw from this list rather than hard-coding their own.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct ActionInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// Whether the action names a commit or a conversation.
    pub needs_target: bool,
    /// Whether a surface should ask twice before running it.
    pub destructive: bool,
    /// What to ask, when it should.
    pub confirm: &'static str,
}

pub const ACTIONS: &[ActionInfo] = &[
    ActionInfo {
        id: "trunk-reset",
        label: "reset trunk here",
        description: "Put trunk's checkout at an earlier commit. Every worker stops first; \
                      forward history survives in the conversation branches that made it.",
        needs_target: true,
        destructive: true,
        confirm: "Reset trunk? All workers stop first.",
    },
    ActionInfo {
        id: "stop-worker",
        label: "stop worker",
        description: "Ask a conversation's worker process to exit. Nothing is lost: branch \
                      state is on disk and in the log, and the next message restarts it.",
        needs_target: true,
        destructive: false,
        confirm: "",
    },
    ActionInfo {
        id: "abort-merge",
        label: "abort merge",
        description: "Abandon a merge the conversation's branch is stuck in.",
        needs_target: true,
        destructive: true,
        confirm: "Abort the merge? The branch returns to its pre-merge state.",
    },
    ActionInfo {
        id: "release-worktree",
        label: "release checkout",
        description: "Remove a stopped conversation's worktree from disk. Its branch and \
                      commits remain.",
        needs_target: true,
        destructive: false,
        confirm: "",
    },
    ActionInfo {
        id: "export-public",
        label: "export public history",
        description: "Derive the filtered `public` branch from trunk, leaving out every \
                      directory holding a .thetis-private marker.",
        needs_target: false,
        destructive: false,
        confirm: "",
    },
    ActionInfo {
        id: "push-public",
        label: "publish to origin/main",
        description: "Export, then push the filtered history to the remote.",
        needs_target: false,
        destructive: true,
        confirm: "Publish to origin/main? This replaces the remote's main with the filtered \
                  export of trunk.",
    },
    ActionInfo {
        id: "pull-public",
        label: "pull from origin/main",
        description: "Merge what another checkout published into trunk here, so the next \
                      publish carries both instead of replacing theirs.",
        needs_target: false,
        destructive: false,
        confirm: "",
    },
    ActionInfo {
        id: "adopt-remote",
        label: "adopt origin/main as base",
        description: "Record that what is published now is where this checkout and the \
                      remote last agreed. Only when the remote holds nothing this checkout lacks.",
        needs_target: false,
        destructive: true,
        confirm: "Adopt origin/main as the base? Only if what is published there is work this \
                  checkout already has — anything on it that is new here would be treated as \
                  already-had and dropped from the next publish.",
    },
];

pub fn action(id: &str) -> Option<&'static ActionInfo> {
    ACTIONS.iter().find(|a| a.id == id)
}

pub fn restart_available(grip: &Grip) -> bool {
    grip.cfg.control.allow_restart
}

/// Gathers the overview. Every source is best-effort: a git error or a missing
/// store empties its section rather than hiding the rest.
pub async fn overview(grip: &Arc<Grip>) -> Overview {
    let root = crate::gitctl::GitCtl::new(grip.cfg.root.clone());
    let trunk_name = root
        .current_branch()
        .await
        .unwrap_or_else(|_| "trunk".to_string());
    let trunk_head = root.head().await.unwrap_or_default();

    let commits = root
        .log("HEAD", 15)
        .await
        .unwrap_or_default()
        .iter()
        .map(|c| CommitRow {
            rev: c.rev.clone(),
            subject: c.subject.clone(),
            author: c.author.clone(),
            head: c.rev == trunk_head,
        })
        .collect();

    let mut branches = Vec::new();
    if let (Some(store), crate::grip::Role::Gateway(router)) = (grip.local_store(), &grip.role) {
        let titles: std::collections::HashMap<String, String> = store
            .list_sessions(true)
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.id, s.title))
            .collect();
        let live: std::collections::HashSet<String> =
            router.live_sessions().await.into_iter().collect();

        for row in store.list_branches().unwrap_or_default() {
            let (ahead, behind) = root
                .ahead_behind(&row.branch_ref, &trunk_name)
                .await
                .unwrap_or((0, 0));
            branches.push(BranchRow {
                title: titles
                    .get(&row.session_id)
                    .cloned()
                    .unwrap_or_else(|| row.session_id.clone()),
                live: live.contains(&row.session_id),
                ahead: ahead as u32,
                behind: behind as u32,
                state: format!("{:?}", row.state).to_lowercase(),
                kernel: if row.kernel_commit.is_empty() {
                    "trunk".to_string()
                } else {
                    row.kernel_commit[..12.min(row.kernel_commit.len())].to_string()
                },
                branch_ref: row.branch_ref,
                session_id: row.session_id,
            });
        }
    }

    let sessions = grip
        .persist
        .list_sessions(true)
        .await
        .map(|s| s.len() as u64)
        .unwrap_or(0);
    let logins = grip
        .local_store()
        .and_then(|store| store.active_logins_by_user(crate::store::now_ms()).ok())
        .unwrap_or_default();
    let owned = grip
        .local_store()
        .and_then(|store| store.owners_map().ok())
        .unwrap_or_default();
    let accounts = grip
        .cfg
        .auth
        .users
        .iter()
        .map(|user| AccountRow {
            id: user.id.clone(),
            name: user.name.clone(),
            role: user.role.clone(),
            admin: user.policy.admin,
            read_only: user.policy.read_only,
            sees_all: user.policy.see_all_sessions,
            conversations: owned.values().filter(|o| *o == &user.id).count() as u32,
            logins: logins.get(&user.id).copied().unwrap_or(0) as u32,
            spend_usd: grip
                .local_store()
                .and_then(|store| store.get_user_spend(&user.id).ok())
                .unwrap_or(0.0),
        })
        .collect();

    let private_dirs = crate::publish::private_dirs(&root, "HEAD")
        .await
        .unwrap_or_default();

    Overview {
        trunk_name,
        trunk_head,
        commits,
        branches,
        accounts,
        private_dirs,
        sessions,
        local_mode: !grip.cfg.auth.users_mode,
        admin_enabled: grip.cfg.admin_enabled,
        restart_available: restart_available(grip),
        config_path: grip.cfg.config_path.display().to_string(),
        overlay_path: grip.cfg.local_overlay().display().to_string(),
    }
}

/// Runs one manual override. `target` names a commit or a conversation for
/// the actions that need one, and is ignored by the rest.
pub async fn act(grip: &Arc<Grip>, action: &str, target: &str) -> Result<String> {
    let crate::grip::Role::Gateway(router) = &grip.role else {
        anyhow::bail!("admin actions run on the gateway");
    };
    let store = grip.local_store().context("gateway has no local store")?;
    let branches = crate::branches::Branches::new(grip.cfg.clone(), store.clone());

    let result = match action {
        // Break glass: put trunk's checkout at an earlier commit. Forward
        // history is preserved in the conversation branches that made it;
        // this moves the shared starting point everyone inherits.
        "trunk-reset" => {
            let root = branches.root_git();
            let rev = root
                .rev_parse(target)
                .await?
                .with_context(|| format!("'{target}' does not name a commit"))?;
            router.stop_all().await;
            root.hard_reset_clean(&rev).await?;
            crate::roles::gateway::load_ui_gateway(grip).await;
            Ok(format!(
                "trunk was reset to {}; stopped workers restart on their next message",
                &rev[..12]
            ))
        }
        "stop-worker" => {
            let peer = router
                .live_peer(target)
                .await
                .with_context(|| format!("no live worker for {target}"))?;
            router.mark_stopping(target).await;
            let _ = peer.call("shutdown", serde_json::Value::Null).await;
            Ok(format!("asked the worker for {target} to stop"))
        }
        "abort-merge" => {
            let state: crate::bindings::branch::BranchState = serde_json::from_value(
                crate::workers::call_session(
                    grip,
                    router,
                    target,
                    "branch.abort",
                    serde_json::json!({ "session": target }),
                )
                .await?,
            )?;
            Ok(format!(
                "merge aborted; the branch is {} again",
                state.state
            ))
        }
        "release-worktree" => {
            if router.live_peer(target).await.is_some() {
                anyhow::bail!("stop the worker first; its checkout is in use");
            }
            branches.release_worktree(target).await?;
            Ok(format!(
                "released the checkout for {target}; its branch and commits remain"
            ))
        }
        // Publishing: derive the filtered history, then (separately)
        // push it. Two explicit human actions, never automatic.
        "export-public" => {
            let root = branches.root_git();
            let export = crate::publish::export_public(root).await?;
            Ok(match export.public_head {
                Some(head) => format!(
                    "exported {} commit(s); public is at {}",
                    export.commits,
                    &head[..12.min(head.len())]
                ),
                None => "nothing to export yet".to_string(),
            })
        }
        // The other direction: take in what another checkout published, so
        // that publishing from here adds to it rather than replacing it.
        "pull-public" => {
            let root = branches.root_git();
            let before = root.head().await?;
            match crate::publish::plan_pull(root).await? {
                crate::publish::Pull::NothingPublished => Ok(format!(
                    "nothing has been published to origin/{} yet, so there is nothing to pull",
                    crate::publish::REMOTE_BRANCH
                )),
                crate::publish::Pull::UpToDate => Ok(format!(
                    "already up to date with origin/{}",
                    crate::publish::REMOTE_BRANCH
                )),
                crate::publish::Pull::Ready(plan) => {
                    let count = plan.subjects.len();
                    let pulled = crate::publish::apply_pull(root, plan).await?;
                    let Some(commit) = pulled.trunk_commit else {
                        return Ok(format!(
                            "origin/{} held nothing this checkout was missing; its history is \
                             now part of ours, so publishing from here no longer replaces it",
                            crate::publish::REMOTE_BRANCH
                        ));
                    };
                    // Everyone's page is served from trunk's build, and trunk
                    // just moved.
                    crate::roles::gateway::load_ui_gateway(grip).await;
                    let mut msg = format!(
                        "pulled {count} commit(s) from origin/{}; trunk is at {}",
                        crate::publish::REMOTE_BRANCH,
                        &commit[..12.min(commit.len())]
                    );
                    // The guest aspects hot-swap; the kernel does not, and
                    // nothing here rebuilds it — say so rather than leave the
                    // operator running a binary older than trunk unawares.
                    if crate::control::kernel_source_moved(root, &before, "HEAD").await {
                        msg.push_str(
                            ". This moved the orchestrator's own source, so trunk's binary is \
                             now older than trunk — rebuild and restart to run it",
                        );
                    }
                    Ok(msg)
                }
            }
        }
        // A claim about the past only the operator can make: what is published
        // now is where this checkout and the remote last agreed.
        "adopt-remote" => {
            let root = branches.root_git();
            let remote = crate::publish::adopt_remote(root).await?;
            Ok(format!(
                "adopted origin/{} at {} as the base this checkout last agreed with; pull now \
                 to join the two histories",
                crate::publish::REMOTE_BRANCH,
                &remote[..12.min(remote.len())]
            ))
        }
        "push-public" => {
            let root = branches.root_git();
            // Export first. Pushing meant "publish where trunk is now", but the
            // button only pushed whatever `public` already pointed at — and on
            // a checkout that had never exported there was no such ref at all,
            // so it failed with git's `src refspec public does not match any`,
            // which says nothing about what to do. Exporting is idempotent, so
            // doing it here costs nothing when the branch is already current.
            let export = crate::publish::export_public(root).await?;
            let Some(head) = export.public_head else {
                anyhow::bail!(
                    "there is nothing to publish yet: trunk has no commits that survive \
                     the private-path filter, so nothing was exported."
                );
            };

            crate::publish::push_public(root).await?;
            Ok(format!(
                "exported {} commit(s) and published {} as '{}' on origin",
                export.commits,
                &head[..12.min(head.len())],
                crate::publish::REMOTE_BRANCH
            ))
        }
        other => anyhow::bail!("unknown action '{other}'"),
    };
    if let Ok(message) = &result {
        tracing::warn!(action, "admin: {message}");
    }
    result
}

/// Ends every login the account holds, on every device. Returns how many.
pub fn sign_out_everywhere(grip: &Grip, user: &str) -> Result<u32> {
    if grip.cfg.auth.user(user).is_none() {
        anyhow::bail!("unknown user '{user}'");
    }
    let store = grip.local_store().context("gateway has no local store")?;
    Ok(store.remove_logins_for(user)? as u32)
}

/// What the system is waiting on, as JSON.
///
/// The first thing to open when something looks frozen: it names the sessions
/// whose worker is still materializing, every outstanding RPC with its age,
/// and who holds the fleet build lock — so "the UI is stuck" becomes a page
/// load rather than an investigation with `gdb`.
pub async fn waits(grip: &Arc<Grip>) -> serde_json::Value {
    let workers = match &grip.role {
        crate::grip::Role::Gateway(router) => router.waits().await,
        crate::grip::Role::Worker(peer) => serde_json::json!({
            "pending_to_gateway": peer
                .in_flight()
                .into_iter()
                .map(|(id, method, age)| serde_json::json!({
                    "id": id, "method": method, "age_s": age
                }))
                .collect::<Vec<_>>(),
        }),
    };

    // The build lock file carries its holder's pid (written when taken), so a
    // build that is queueing the fleet can be identified from here. The pid
    // alone is not evidence that anyone holds it: the file keeps the name of
    // whoever wrote it last, and a holder that was killed leaves it behind. Ask
    // the kernel whether the lock is actually taken, and only name a pid when
    // it is — reporting a stale one as live sent more than one investigation
    // after a process that had exited hours before.
    let lock = grip.cfg.build_lock_path();
    let build_lock_held = crate::builder::lock_is_held(&lock);
    let build_lock = std::fs::read_to_string(&lock)
        .ok()
        .map(|pid| pid.trim().to_string())
        .filter(|pid| !pid.is_empty() && build_lock_held);

    // Turns run in workers, so this process's own counter is zero on the
    // gateway and the fleet's total is what the question means. Both are
    // reported: `turns_running` is the honest answer to "is anything running",
    // and `turns_running_here` keeps the old, narrower number available.
    let turns_here = grip.turns_in_flight();
    let turns_in_workers: u64 = workers
        .get("workers")
        .and_then(|w| w.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r.get("turns_running").and_then(|t| t.as_u64()))
                .sum()
        })
        .unwrap_or(0);

    serde_json::json!({
        "uptime_s": crate::control::uptime().as_secs(),
        "workers": workers,
        "build_lock_held": build_lock_held,
        "build_lock_holder_pid": build_lock,
        "building": grip.building_aspects(),
        "turns_running": turns_here as u64 + turns_in_workers,
        "turns_running_here": turns_here,
    })
}

/// Restarts the orchestrator. The whole process, not one worker: this is the
/// operator applying a configuration change, and configuration is read once
/// at boot.
pub async fn restart(grip: &Arc<Grip>, reason: &str) -> Result<String> {
    let reason = if reason.trim().is_empty() {
        "requested from the control panel"
    } else {
        reason.trim()
    };
    crate::control::request_restart(grip, reason, false, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table and the dispatcher must agree, or a surface offers a button
    /// the host refuses (or the host answers to a name nothing can send).
    #[test]
    fn every_action_in_the_table_is_dispatched_and_vice_versa() {
        let src = include_str!("admin.rs");
        let body = src
            .split("pub async fn act(")
            .nth(1)
            .unwrap()
            .split("other => anyhow::bail!")
            .next()
            .unwrap();
        let arms: Vec<&str> = body
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                let name = l.strip_prefix('"')?;
                let (name, rest) = name.split_once('"')?;
                rest.trim_start().starts_with("=>").then_some(name)
            })
            .collect();
        for a in ACTIONS {
            assert!(arms.contains(&a.id), "{} has no arm in act()", a.id);
        }
        for arm in &arms {
            assert!(action(arm).is_some(), "{arm} is dispatched but not in ACTIONS");
        }
    }

    #[test]
    fn destructive_actions_say_what_they_ask() {
        for a in ACTIONS {
            assert_eq!(a.destructive, !a.confirm.is_empty(), "{}", a.id);
        }
    }
}
