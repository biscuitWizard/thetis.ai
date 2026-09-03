//! Trunk orchestration: the gateway side of merging.
//!
//! The one invariant everything here defends: **trunk only ever moves by
//! fast-forward, and only the gateway moves it.** All conflicts materialize
//! in the conversation's own worktree — by merging trunk *into* the branch
//! first — so a half-finished merge on trunk is structurally impossible.
//!
//! Within that, a merge is squashed. A conversation's branch accumulates a
//! commit per turn checkpoint, per green build, per skill write and per trunk
//! update, which is the right granularity to recover *inside* the branch and
//! the wrong granularity for trunk — one merge used to add dozens of lines to
//! trunk's log, none of which named the work. So the branch is collapsed onto
//! trunk as a single commit, listing what it absorbed; then trunk fast-forwards
//! to it as before. Its subject line comes from a model call over the branch's
//! commit subjects and its diffstat — the conversation's title names what was
//! first asked for, not what landed — held to one line under 200 characters,
//! and falling back to the title if the call fails, so a merge never depends
//! on the provider being up.
//!
//! Merging is user-only. The agent can prepare (update from trunk, resolve
//! conflicts), but nothing in the agent's tool surface reaches this module.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::json;
use std::sync::Arc;

use crate::bindings::branch::BranchState;
use crate::branches::Branches;
use crate::grip::Grip;
use crate::pipeline;
use crate::workers::{WorkerRouter, call_session};

/// The result of asking for a merge: either trunk moved, or the branch is
/// left holding conflicts for someone to resolve.
#[derive(Debug)]
pub enum MergeResult {
    Merged { from: String, to: String },
    Conflicts(BranchState),
}

/// Merges a conversation's branch to trunk.
///
/// Steps: checkpoint the worktree; bring trunk into the branch if it moved
/// (conflicts stop here, in the branch); require every aspect the branch
/// changed to have a green build; squash the branch's own commits into one;
/// then fast-forward trunk and refresh the UI the gateway serves.
pub async fn merge_to_trunk(
    grip: &Arc<Grip>,
    router: &Arc<WorkerRouter>,
    session_id: &str,
) -> Result<MergeResult> {
    let store = grip
        .local_store()
        .context("merging is a gateway operation")?;
    let branches = Branches::new(grip.cfg.clone(), store.clone());
    let row = branches
        .get(session_id)?
        .ok_or_else(|| anyhow!("this conversation has no branch yet — nothing to merge"))?;
    let root = branches.root_git();
    let trunk = root.current_branch().await?;

    // Anything sitting uncommitted in the worktree belongs to the merge.
    call_session(
        grip,
        router,
        session_id,
        "branch.commit_dirty",
        json!({ "message": "checkpoint: before merge to trunk" }),
    )
    .await?;

    // Trunk moved since this branch last saw it? Bring trunk in first — in
    // the branch's worktree, where a conflict can sit safely.
    if !root.is_ancestor(&trunk, &row.branch_ref).await? {
        let state: BranchState = serde_json::from_value(
            call_session(
                grip,
                router,
                session_id,
                "branch.update",
                json!({ "session": session_id }),
            )
            .await?,
        )?;
        if state.state == "conflict" {
            return Ok(MergeResult::Conflicts(state));
        }
    }

    // The exact commit everything below judges, squashes and fast-forwards to.
    //
    // Pinned here rather than re-resolving `row.branch_ref` at each step: the
    // conversation's worker is still live and still committing — the watcher
    // checkpoints on every file change — so the tree the gate declared green
    // was not necessarily the tree that landed on trunk. A commit arriving
    // between the two would ride onto trunk with no green build behind it.
    let gated_tip = root
        .rev_parse(&row.branch_ref)
        .await?
        .ok_or_else(|| anyhow!("{} does not name a commit", row.branch_ref))?;

    // The gate: every aspect this branch changed must have a green build of
    // exactly the source being merged. Judged by tree identity rather than by
    // cache key, because the key carries the fingerprint of the kernel that
    // issued the verdict — and a branch that changed the contract earns its
    // greens under its own kernel, which is still evidence the source builds
    // and runs.
    for aspect in pipeline::discover_aspects(&grip.cfg) {
        let rel = grip.cfg.aspect_source_rel(&aspect);
        let Some(rel) = rel else { continue };
        let branch_aspect_tree = root.tree_oid(&gated_tip, &rel).await?.unwrap_or_default();
        let trunk_aspect_tree = root.tree_oid(&trunk, &rel).await?.unwrap_or_default();
        let branch_wit_tree = root.tree_oid(&gated_tip, "wit").await?.unwrap_or_default();
        let trunk_wit_tree = root.tree_oid(&trunk, "wit").await?.unwrap_or_default();
        if branch_aspect_tree == trunk_aspect_tree && branch_wit_tree == trunk_wit_tree {
            continue; // unchanged by this branch
        }
        if branch_aspect_tree.is_empty() {
            continue; // the branch deleted this aspect; there is nothing to build
        }
        let green = grip.buildcache.list(&aspect.key())?.iter().any(|meta| {
            meta.aspect_tree == branch_aspect_tree
                && meta.wit_tree == branch_wit_tree
                && meta.smoke == crate::buildcache::SmokeVerdict::Pass
        });
        if !green {
            bail!(
                "{aspect} was changed on this branch but its latest state has no green build; \
                 build it successfully (or reset the branch) before merging"
            );
        }
    }

    // Collapse the branch's own commits into one before trunk sees them. The
    // squashed commit carries the branch's tree unchanged, so the gate above
    // still describes exactly what lands.
    // Did the conversation commit while the gate was running? Squashing would
    // then collapse commits nobody judged, so stop and say so rather than
    // merging something that was never verified. Retrying is cheap: the second
    // pass gates the new tip.
    let now_at = root
        .rev_parse(&row.branch_ref)
        .await?
        .unwrap_or_else(|| gated_tip.clone());
    if now_at != gated_tip {
        bail!(
            "this conversation committed while the merge was being checked \
             ({} moved from {} to {}); nothing was merged — try again",
            row.branch_ref,
            &gated_tip[..12.min(gated_tip.len())],
            &now_at[..12.min(now_at.len())]
        );
    }

    let squashed = squash_branch(grip, &branches, &row, &trunk, session_id).await?;

    let from = root.head().await?;
    // Fast-forward to the commit the gate judged, not to whatever the ref
    // names now. `squash_branch` moves the ref to a commit carrying that exact
    // tree, so this is the same content either way — but naming the pinned
    // commit means a branch that moves at the last instant cannot slip
    // unverified work onto trunk.
    let target = match &squashed {
        Some(squashed) => squashed.clone(),
        None => gated_tip.clone(),
    };
    let to = root.merge_ff_only(&target).await.map_err(|e| {
        anyhow!("trunk would not fast-forward (did it move again mid-merge?): {e:#}")
    })?;
    tracing::info!(
        session = %session_id,
        branch = %row.branch_ref,
        from = %&from[..12.min(from.len())],
        to = %&to[..12.min(to.len())],
        "merged to trunk"
    );

    // The record lands through the worker so it is rendered like any other
    // event and every tab sees it.
    let _ = call_session(
        grip,
        router,
        session_id,
        "branch.record_op",
        json!({
            "session": session_id,
            "op": "merge",
            "ok": true,
            "from_rev": from,
            "to_rev": to,
            "conflicts": [],
            "detail": match &squashed {
                Some(_) => "this conversation's changes were squashed into one commit and are \
                            now on trunk (the detailed history is kept under refs/thetis/presquash)"
                    .to_string(),
                None => "this conversation's changes are now on trunk".to_string(),
            },
        }),
    )
    .await;

    // Everyone loads the page from trunk's build; pick up the one that just
    // landed. Content addressing means the branch already built it.
    crate::roles::gateway::load_ui_gateway(grip).await;

    // The guest aspects are hot-swappable; the kernel is not. If this merge moved
    // the orchestrator's own source, trunk's binary is now older than trunk.
    refresh_trunk_kernel(grip, &from, session_id).await;

    Ok(MergeResult::Merged { from, to })
}

/// Collapses everything the branch has of its own into one commit parented on
/// trunk, so a merge adds a single line to trunk's log instead of the dozens of
/// checkpoints, rebuild records and trunk-update merges a conversation
/// accumulates while it works.
///
/// Safe to do here, immediately before the fast-forward, because the squashed
/// commit reuses the branch tip's tree unchanged:
///
/// * the green-build gate above judged that tree, and still describes it;
/// * the worktree and its index match that tree, so the conversation stays
///   clean and does not have to be told anything;
/// * build-cache entries are keyed by tree oid, so every artifact stays valid.
///
/// The pre-squash tip is kept under `refs/thetis/presquash/…`, so the detailed
/// history is rewritten out of the branch but never unreachable.
async fn squash_branch(
    grip: &Arc<Grip>,
    branches: &Branches,
    row: &crate::branches::BranchRow,
    trunk: &str,
    session_id: &str,
) -> Result<Option<String>> {
    let root = branches.root_git();

    // A worktree mid-merge has a MERGE_HEAD that names the old tip; rewriting
    // the branch under it would leave it referring to a commit off the branch.
    let worktree = crate::gitctl::GitCtl::new(row.worktree.clone());
    if row.worktree.is_dir() && worktree.merge_in_progress().await.unwrap_or(false) {
        bail!("this conversation has a merge in progress; resolve or abort it before merging");
    }

    let commits = root
        .log_args(&[&row.branch_ref, &format!("^{trunk}")], 200)
        .await
        .unwrap_or_default();
    if commits.len() < 2 {
        return Ok(None); // nothing to collapse; one commit is already tidy
    }

    let title = grip
        .local_store()
        .and_then(|store| store.get_session(session_id).ok().flatten())
        .map(|meta| meta.title)
        .filter(|t| !t.trim().is_empty() && t != crate::store::DEFAULT_TITLE)
        .unwrap_or_else(|| row.branch_ref.clone());
    let fallback: String = clamp_subject(&title);

    // Oldest first reads as a narrative of the conversation's work.
    let mut body = String::new();
    for commit in commits.iter().rev().take(60) {
        body.push_str(&format!("  {}\n", commit.subject));
    }

    // The subject is what trunk's log shows, so it is worth a model call: the
    // conversation's title is whatever the first message happened to be about,
    // while the branch's own commits and diffstat say what actually landed.
    let oldest_first: Vec<crate::gitctl::CommitInfo> = commits.iter().rev().cloned().collect();
    let subject = match summarize_subject(grip, branches, row, trunk, &title, &oldest_first).await {
        Ok(line) => line,
        Err(e) => {
            tracing::warn!(error = %e, "could not summarize the branch for its commit subject; using the conversation title");
            fallback
        }
    };
    if commits.len() > 60 {
        body.push_str(&format!("  … and {} more\n", commits.len() - 60));
    }
    let message = format!(
        "{subject}\n\n{} commits from {}, squashed:\n{body}\nSession: {session_id}\n",
        commits.len(),
        row.branch_ref,
    );

    let keep = format!(
        "refs/thetis/presquash/{}/{}",
        row.branch_ref.replace('/', "-"),
        crate::store::now_ms()
    );
    // The ref move is a compare-and-swap against the tip read above, so a
    // turn that commits while this runs loses nothing — the squash simply
    // refuses, and the merge can be asked for again.
    let squashed = root
        .squash_onto(&row.branch_ref, trunk, &message, Some(&keep))
        .await
        .map_err(|e| {
            anyhow!(
                "could not squash this conversation's commits before merging, so nothing \
                 was merged: {e:#}. If the conversation committed while the merge was \
                 starting, ask for the merge again."
            )
        })?;

    tracing::info!(
        session = %session_id,
        branch = %row.branch_ref,
        collapsed = commits.len(),
        squashed = %&squashed[..12.min(squashed.len())],
        kept = %keep,
        "squashed the branch into one commit"
    );
    Ok(Some(squashed))
}

/// Trunk's log is read at a glance, so a subject line is held to one line and
/// under 200 characters — the limit the whole squashed-subject path obeys,
/// whether the line came from a model or from the conversation's title.
const SUBJECT_LIMIT: usize = 199;

/// One line, whitespace collapsed, no markdown fencing or leading bullet, cut
/// to `SUBJECT_LIMIT` characters at a word boundary where possible.
fn clamp_subject(raw: &str) -> String {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("```"))
        .unwrap_or("")
        .trim_start_matches(['-', '*', '#', '>', ' '])
        .trim_matches('"')
        .trim();
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= SUBJECT_LIMIT {
        return collapsed;
    }
    let cut: String = collapsed.chars().take(SUBJECT_LIMIT).collect();
    match cut.rsplit_once(' ') {
        // Only respect the word boundary if it does not throw away the line.
        Some((head, _)) if head.chars().count() >= SUBJECT_LIMIT / 2 => {
            head.trim_end_matches(&[',', ';', ':', '-'][..]).to_string()
        }
        _ => cut,
    }
}

/// Asks the model for the squashed commit's subject line.
///
/// The evidence is the branch's own commit subjects and its diffstat against
/// trunk: between them they say what changed and where, which the conversation
/// title often does not. Any failure — no API key, a provider error, an empty
/// answer — is the caller's cue to fall back to the title, so this never
/// blocks a merge.
async fn summarize_subject(
    grip: &Arc<Grip>,
    branches: &Branches,
    row: &crate::branches::BranchRow,
    trunk: &str,
    title: &str,
    commits: &[crate::gitctl::CommitInfo],
) -> Result<String> {
    let root = branches.root_git();
    let stat = root
        .diff_stat(trunk, &row.branch_ref, 40)
        .await
        .unwrap_or_default();

    let subjects = commits
        .iter()
        .take(80)
        .map(|c| format!("- {}", c.subject))
        .collect::<Vec<_>>()
        .join("\n");

    let model = if grip.cfg.context.summary_model.is_empty() {
        grip.cfg.model.clone()
    } else {
        grip.cfg.context.summary_model.clone()
    };

    let prompt = format!(
        "Write the subject line for a squashed git commit that merges a branch to trunk.\n\n\
         Rules:\n\
         - One line, imperative mood, under {SUBJECT_LIMIT} characters.\n\
         - Say what the branch changed and where, concretely. No ticket ids, no \
           trailing period, no quotes, no markdown.\n\
         - Prefer the substance of the work over process noise (checkpoints, \
           rebuilds, trunk updates).\n\
         - Reply with the line and nothing else.\n\n\
         Conversation title: {title}\n\n\
         Commits on the branch, oldest first:\n{subjects}\n\n\
         Diffstat against trunk:\n{stat}\n"
    );

    let request = serde_json::json!({
        "model": model,
        "max_tokens": 200,
        "messages": [{ "role": "user", "content": prompt }],
    });

    let raw = grip
        .llm
        .chat(&request.to_string())
        .await
        .map_err(|e| anyhow!("{e:?}"))?;
    let parsed: serde_json::Value = serde_json::from_str(&raw)?;
    let text = parsed["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default();
    let subject = clamp_subject(text);
    if subject.is_empty() {
        bail!("the model returned no usable subject line");
    }
    tracing::info!(%model, %subject, "summarized the branch for its squashed commit subject");
    Ok(subject)
}

/// Rebuilds trunk's orchestrator binary after a merge moved its source, and
/// restarts the gateway onto it.
///
/// Guest components hot-swap, so a merge of agent, gateway or tool code takes
/// effect on its own. Native code cannot: the gateway process, and every worker
/// it spawns from its own executable, keep running the binary they started
/// with. Without this, kernel changes reach trunk and then simply never run.
///
/// The whole system goes away for a moment, so the order is strict: build,
/// probe, only then restart. A build or probe failure leaves trunk serving the
/// binary it already had and files an incident in the conversation that merged
/// — the source is on trunk either way, so this is staleness, not loss.
///
/// Runs detached: the build takes minutes and the browser's merge request must
/// not wait on it.
async fn refresh_trunk_kernel(grip: &Arc<Grip>, before: &str, session_id: &str) {
    let root = crate::gitctl::GitCtl::new(grip.cfg.root.clone());
    if !crate::control::kernel_source_moved(&root, before, "HEAD").await {
        return;
    }

    let grip = grip.clone();
    let session = session_id.to_string();
    tokio::spawn(async move {
        let incident = |grip: Arc<Grip>, session: String, text: String| async move {
            tracing::warn!(%text, "trunk's kernel was not refreshed");
            let _ = grip
                .append_event(
                    &session,
                    crate::bindings::types::SessionEvent::Incident(text),
                )
                .await;
        };

        let _ = grip
            .append_event(
                &session,
                crate::bindings::types::SessionEvent::Incident(
                    "This merge moved the orchestrator's own source, so trunk's binary is \
                     being rebuilt. Thetis will restart onto it once it builds and answers \
                     its startup probe."
                        .to_string(),
                ),
            )
            .await;

        let built = match crate::control::build_kernel(&grip.cfg).await {
            Ok(crate::control::KernelBuild::Built(path)) => path,
            // Contention, not failure. The build already going covers this
            // merge too, so the announcement above stays true and this adds
            // nothing.
            Ok(crate::control::KernelBuild::Busy(why)) => {
                tracing::info!(%why, "a kernel build was already running; leaving it to finish");
                return;
            }
            Err(e) => {
                incident(
                    grip.clone(),
                    session,
                    format!(
                        "The merge landed on trunk, but rebuilding trunk's orchestrator \
                         binary failed, so Thetis is still running the one built before \
                         the merge: {e:#}"
                    ),
                )
                .await;
                return;
            }
        };

        if let Err(e) = crate::control::probe_kernel(&built).await {
            incident(
                grip.clone(),
                session,
                format!(
                    "Trunk's rebuilt orchestrator binary did not pass its startup probe, so \
                     Thetis was left running the previous one: {e:#}"
                ),
            )
            .await;
            return;
        }

        // A restart re-execs *this* process's own path. When that is not where
        // the build landed, the new binary is real but unreachable, and saying
        // so is better than restarting onto the old code and claiming success.
        //
        // Read through `launch_path`, because the build above has already
        // unlinked the file this process is executing: raw `current_exe` now
        // reports that path with a " (deleted)" suffix, which matches nothing,
        // and this guard would veto the very deploy it exists to protect.
        let running = crate::control::launch_path();
        if running.as_deref() != Some(built.as_path()) {
            incident(
                grip.clone(),
                session,
                format!(
                    "Trunk's orchestrator was rebuilt and passed its probe at {}, but Thetis \
                     is running {} — a different binary. Deploy the new build and restart to \
                     pick up the merged kernel changes.",
                    built.display(),
                    running
                        .as_deref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "an unknown path".into()),
                ),
            )
            .await;
            return;
        }

        if let Err(e) = crate::control::request_restart(
            &grip,
            "a merge to trunk moved the orchestrator's own code; restarting on the \
             rebuilt kernel",
            true,
            Some(&session),
        )
        .await
        {
            incident(
                grip.clone(),
                session,
                format!(
                    "Trunk's orchestrator was rebuilt and passed its probe, but the restart \
                     was refused, so the merged kernel changes are not live yet: {e:#}"
                ),
            )
            .await;
        }
    });
}

/// Hands a conflicted merge to the conversation itself: composes a manifest
/// of the conflict and submits it as a user message, so the agent resolves it
/// with its ordinary tools and finishes with `complete_merge`.
pub async fn resolve_in_conversation(
    grip: &Arc<Grip>,
    router: &Arc<WorkerRouter>,
    session_id: &str,
) -> Result<()> {
    let state: BranchState = serde_json::from_value(
        call_session(grip, router, session_id, "branch.status", json!({})).await?,
    )?;
    if state.state != "conflict" {
        bail!("there is no conflicted merge in this conversation to resolve");
    }

    let _ = call_session(
        grip,
        router,
        session_id,
        "branch.record_op",
        json!({
            "session": session_id,
            "op": "resolve-handoff",
            "ok": true,
            "from_rev": state.head_rev,
            "to_rev": "",
            "conflicts": state.conflicts,
            "detail": "conflict resolution handed to this conversation",
        }),
    )
    .await;

    let files = state
        .conflicts
        .iter()
        .map(|f| format!("  - {f}"))
        .collect::<Vec<_>>()
        .join("\n");
    let manifest = format!(
        "A merge of trunk into this conversation's branch stopped on conflicts. The \
         conflicted files, each containing standard git conflict markers \
         (`<<<<<<<` ours / `>>>>>>>` trunk), are:\n{files}\n\n\
         Please resolve each conflict with your normal editing tools — keep this \
         branch's intent where the two sides disagree unless the trunk side is \
         clearly a fix — verify the affected components still build, then call \
         `complete_merge`. If the conflicts cannot be resolved sensibly, call \
         `abort_merge` and say why."
    );

    // System-authored: a conflict manifest is not a person speaking.
    grip.submit(session_id, manifest, Vec::new(), None).await
}

/// Ahead/behind of a branch relative to trunk, readable without a worker —
/// refs are enough, no worktree required.
pub async fn ref_ahead_behind(branches: &Branches, branch_ref: &str) -> Result<(u64, u64)> {
    let trunk = branches.root_git().current_branch().await?;
    branches.root_git().ahead_behind(branch_ref, &trunk).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_models_chattier_habits_are_stripped() {
        // Preamble line, a fence, a bullet and quotes: all things a model adds
        // and none of which belong in a git subject.
        let raw = "```\n- \"Add a diffstat helper to gitctl\"\n```\nHope that helps!";
        assert_eq!(clamp_subject(raw), "Add a diffstat helper to gitctl");
    }

    #[test]
    fn whitespace_is_collapsed_to_one_line() {
        assert_eq!(clamp_subject("  Fix   the\tpump  "), "Fix the pump");
    }

    #[test]
    fn an_overlong_line_is_cut_at_a_word_boundary_under_the_limit() {
        let long = "Summarize ".repeat(60);
        let out = clamp_subject(&long);
        assert!(
            out.chars().count() <= SUBJECT_LIMIT,
            "got {} chars",
            out.chars().count()
        );
        assert!(out.chars().count() < 200);
        assert!(!out.ends_with(' '));
        // Cut on a boundary, so no word is left half-written.
        assert!(out.ends_with("Summarize"), "got {out:?}");
    }

    #[test]
    fn a_single_enormous_word_is_still_cut_to_the_limit() {
        let out = clamp_subject(&"x".repeat(500));
        assert_eq!(out.chars().count(), SUBJECT_LIMIT);
    }

    #[test]
    fn nothing_usable_yields_nothing_so_the_caller_falls_back() {
        assert_eq!(clamp_subject("```\n```"), "");
        assert_eq!(clamp_subject("   "), "");
    }
}
