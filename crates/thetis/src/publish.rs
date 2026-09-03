//! The publish boundary: what may leave this machine.
//!
//! Tools and skills marked private — a `.thetis-private` file in their
//! directory — stay fully tracked locally (branching, merging, and resets all
//! need them), so privacy is enforced where it actually matters: at the push.
//! `main` on the remote *is* the published version: a derived local branch
//! mirrors trunk commit-for-commit with private
//! paths filtered out of every tree, and a pre-push hook refuses to let any
//! other branch reach a remote. Publishing is an explicit human action from
//! /admin; nothing here runs on its own.
//!
//! Marking something private only affects future exports — anything already
//! pushed is in the remote's history until scrubbed by hand.
//!
//! Publishing runs the boundary one way; pulling runs it the other. More than
//! one checkout can publish to the same remote, and a publish replaces `main`
//! there outright, so the second one to arrive is refused by the lease. The
//! way through is to pull: what is published is merged into trunk here — as a
//! three-way merge between published *trees*, since trunk itself carries
//! things that have never left this machine — and the remote's head becomes a
//! parent of ours, so the next publish adds to it instead of replacing it.

use anyhow::{Context, Result};

use crate::gitctl::{GitCtl, MergeTree};

/// The marker file. Uniform for tools, skills, and any other directory:
/// it diffs, merges, and resets like any tracked file.
pub const PRIVATE_MARKER: &str = ".thetis-private";
/// The marker's name before the project was renamed.
///
/// Still honoured. A marker file is a safety mechanism, and quietly ceasing to
/// recognise one because the project changed names is how a directory somebody
/// deliberately marked private ends up published. Nothing in this repository
/// used it, but a branch or an older checkout may.
pub const LEGACY_PRIVATE_MARKER: &str = ".genesis-private";

/// The branch the export writes and the only one the hook lets out.
/// Where the filtered history is built, locally.
///
/// It is pushed to `main` on the remote — that branch *is* the published
/// version of the project — but it cannot be built on local `main`, which is
/// trunk and carries whatever is private. The two are deliberately different
/// refs with the same public meaning.
pub const PUBLIC_REF: &str = "refs/heads/public";

/// The branch the filtered history is published as.
pub const REMOTE_BRANCH: &str = "main";
/// Bookkeeping: the last trunk commit the export has processed.
const SOURCE_REF: &str = "refs/thetis/public-source";
/// Bookkeeping: the remote commit this checkout last reconciled with.
///
/// Publishing records it, and so does pulling. It exists because a three-way
/// merge needs a base, and two checkouts that have each published from their
/// own trunk share no commit to serve as one: each derives its own chain with
/// `commit-tree`, so the same content becomes different objects on each
/// machine. Once one pull has happened the histories are genuinely joined and
/// git can find the base itself; until then this ref is the only record of
/// where they last agreed.
const REMOTE_SYNC_REF: &str = "refs/thetis/public-remote";

/// Where the fetched state of the published branch is read from.
fn tracking_ref() -> String {
    format!("refs/remotes/origin/{REMOTE_BRANCH}")
}

const HOOK_MARKER: &str = "# thetis publish guard";
/// What the marker said before the project was renamed.
///
/// A hook carrying this is still ours — it is the same guard, written under
/// the old name — so it is upgraded rather than treated as a stranger's and
/// left in place. Without this the rename left every checkout warning that
/// pushes were unguarded (they were guarded, by the stale copy) while the
/// documented override variable no longer matched the one the hook read.
const LEGACY_HOOK_MARKER: &str = "# genesis publish guard";

/// Directories private as of `rev` — every ancestor directory of a marker.
pub async fn private_dirs(git: &GitCtl, rev: &str) -> Result<Vec<String>> {
    Ok(git
        .tree_files(rev)
        .await?
        .into_iter()
        .filter_map(|path| {
            path.strip_suffix(&format!("/{PRIVATE_MARKER}"))
                .or_else(|| path.strip_suffix(&format!("/{LEGACY_PRIVATE_MARKER}")))
                .map(str::to_string)
        })
        .collect())
}

/// The outcome of one export run.
pub struct Export {
    pub commits: usize,
    pub public_head: Option<String>,
}

/// Brings the `public` branch up to date with trunk's head, filtering private
/// paths out of every commit. Idempotent: already-exported commits are never
/// redone, and each trunk commit maps to exactly one public commit with the
/// same message.
pub async fn export_public(git: &GitCtl) -> Result<Export> {
    let head = git.head().await?;
    let last_source = git.rev_parse(SOURCE_REF).await?;
    let range = match &last_source {
        Some(source) if source == &head => {
            return Ok(Export {
                commits: 0,
                public_head: git.rev_parse(PUBLIC_REF).await?,
            });
        }
        Some(source) => format!("{source}..{head}"),
        None => head.clone(),
    };

    let mut parent = git.rev_parse(PUBLIC_REF).await?;
    let mut exported = 0usize;

    for commit in git.rev_list(&range).await? {
        let filtered = filtered_tree(git, &commit).await?;

        // An empty filter delta against the parent would still get its own
        // commit: the one-to-one mapping is what makes the export idempotent
        // and its history legible against trunk's.
        let subject = git
            .log(&commit, 1)
            .await?
            .first()
            .map(|c| c.subject.clone())
            .unwrap_or_else(|| "export".to_string());

        let new_commit = match &parent {
            Some(parent_rev) => git
                .run_raw(&["commit-tree", &filtered, "-p", parent_rev, "-m", &subject])
                .await? ,
            None => git.run_raw(&["commit-tree", &filtered, "-m", &subject]).await?,
        };
        let new_commit = String::from_utf8_lossy(&new_commit.stdout).trim().to_string();
        parent = Some(new_commit);
        exported += 1;
    }

    if let Some(public_head) = &parent {
        git.update_ref(PUBLIC_REF, public_head).await?;
    }
    git.update_ref(SOURCE_REF, &head).await?;

    Ok(Export {
        commits: exported,
        public_head: parent,
    })
}

/// `commit`'s tree with the private paths of *that commit* removed.
async fn filtered_tree(git: &GitCtl, commit: &str) -> Result<String> {
    let private = private_dirs(git, commit).await?;
    strip_private(git, commit, &private)
        .await
        .with_context(|| format!("filtering commit {commit}"))
}

/// `tree_ish`'s tree without `private`, built in a temporary index so the
/// working tree and the real index are never touched.
async fn strip_private(git: &GitCtl, tree_ish: &str, private: &[String]) -> Result<String> {
    let tmp = tempfile_path(git, tree_ish)?;
    let tmp_str = tmp.to_string_lossy().to_string();
    let env: &[(&str, &str)] = &[("GIT_INDEX_FILE", &tmp_str)];

    let result = async {
        git.run_with_env(&["read-tree", tree_ish], env).await?;
        if !private.is_empty() {
            // `-f` because git otherwise refuses to drop an entry whose
            // content matches neither HEAD nor the file on disk, to save
            // someone from losing work. There is nothing to lose here: this
            // index is a temporary file, `--cached` leaves the working tree
            // alone, and the tree being filtered is frequently neither HEAD
            // nor what is checked out — it is another commit's, or the result
            // of a merge that exists only in the object store.
            let mut args: Vec<&str> =
                vec!["rm", "-r", "-q", "-f", "--cached", "--ignore-unmatch", "--"];
            for dir in private {
                args.push(dir);
            }
            git.run_with_env(&args, env).await?;
        }
        let out = git.run_with_env(&["write-tree"], env).await?;
        Ok::<String, anyhow::Error>(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
    .await;

    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("lock"));
    result
}

fn tempfile_path(_git: &GitCtl, commit: &str) -> Result<std::path::PathBuf> {
    // Unique per call: identical commits can exist in different repositories
    // (same content, same second), and write-tree keeps a lockfile beside
    // the index, so a shared name would collide with itself.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(std::env::temp_dir().join(format!(
        "thetis-export-{}-{}-{}",
        std::process::id(),
        seq,
        &commit[..12.min(commit.len())]
    )))
}

/// Installs the pre-push guard into the repository's shared hooks directory.
/// Content-checked: ours is rewritten in place; a hook someone else wrote is
/// left alone, loudly.
pub async fn install_push_guard(git: &GitCtl) -> Result<()> {
    let hooks = git.common_dir().await?.join("hooks");
    std::fs::create_dir_all(&hooks)?;
    let path = hooks.join("pre-push");

    let hook = format!(
        r#"#!/bin/sh
{HOOK_MARKER}
# Only the filtered export may leave this machine: local `main` is trunk and
# the conversation branches can carry private tools and skills. Publish from
# /admin, which exports and pushes it as `main` on the remote. Set
# THETIS_ALLOW_PUSH=1 to override once.
#
# `bench-results` is also allowed. It is not a filtered view of any private
# tree: every commit on it is built file by file from a staging directory that
# the benchmark generator wrote, so it can only ever contain what that
# generator put there — charts and one line of numbers. It carries no history
# from trunk and shares no objects with it.
if [ -n "$THETIS_ALLOW_PUSH" ]; then exit 0; fi
while read local_ref local_sha remote_ref remote_sha; do
  case "$local_ref" in
    refs/heads/public|refs/heads/bench-results|"") ;;
    *)
      echo "thetis: refusing to push $local_ref — only the filtered export leaves this machine." >&2
      echo "thetis: publish from /admin, or set THETIS_ALLOW_PUSH=1 to override." >&2
      exit 1
      ;;
  esac
done
exit 0
"#
    );

    if path.exists() {
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if !existing.contains(HOOK_MARKER) && !existing.contains(LEGACY_HOOK_MARKER) {
            tracing::warn!(
                path = %path.display(),
                "a pre-push hook already exists and is not ours; leaving it — \
                 private paths are NOT push-guarded"
            );
            return Ok(());
        }
        if existing == hook {
            return Ok(());
        }
    }
    std::fs::write(&path, hook)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    tracing::info!(path = %path.display(), "publish guard installed");
    Ok(())
}

/// Publishes the exported `public` branch as `main` on the remote.
///
/// The remote's `main` *is* the published version, so that is where the
/// filtered history goes. It is often not a fast-forward — the filtered chain
/// is derived, and rewriting trunk's history rewrites it too — so publishing
/// may have to replace what is there. What keeps that honest is that it may
/// only replace a state this checkout has reconciled with: its own last
/// publish, or a pull. Anything else on the remote is another checkout's work,
/// and the answer is to pull it in, not to push over it.
pub async fn push_public(git: &GitCtl) -> Result<()> {
    let remote = fetch_remote(git).await?;
    let public = git.rev_parse(PUBLIC_REF).await?;

    // What the lease is told to expect. A bare `--force-with-lease=<branch>`
    // measures against the tracking ref, which the fetch above has just
    // refreshed — so it would only ever catch a remote that moved in the
    // instant between the two, and would wave through the case that actually
    // happens: another checkout published while this one was working. The
    // check has to be made here, against a state this checkout agreed with,
    // before anything is sent.
    let lease = match (&remote, &public) {
        // A branch that does not exist yet cannot be overwritten.
        (None, _) => format!("--force-with-lease={REMOTE_BRANCH}"),
        (Some(remote), Some(public)) => {
            let contained = git.is_ancestor(remote, public).await?;
            let agreed = git.rev_parse(REMOTE_SYNC_REF).await?.as_ref() == Some(remote);
            // Fast-forward: everything published is already in what we are
            // about to push, so nothing can be lost, whatever we last agreed.
            // Otherwise this push replaces published history, and may only do
            // so over the state we last reconciled with.
            if !contained && !agreed {
                anyhow::bail!("{}", not_reconciled(remote));
            }
            format!("--force-with-lease={REMOTE_BRANCH}:{remote}")
        }
        (Some(_), None) => anyhow::bail!(
            "there is nothing exported to publish; export the public history first."
        ),
    };

    let refspec = format!("public:{REMOTE_BRANCH}");
    let push = git
        .run_hooked_status(&["push", &lease, "origin", &refspec], &[])
        .await?;
    if !push.status.success() {
        let err = String::from_utf8_lossy(&push.stderr);
        anyhow::bail!("{}", push_failure(err.trim()));
    }

    // What we just put there is, by definition, where this checkout and the
    // remote last agreed — the base a later pull measures both sides against.
    if let Some(head) = git.rev_parse(PUBLIC_REF).await? {
        git.update_ref(REMOTE_SYNC_REF, &head).await?;
    }
    Ok(())
}

/// Brings the tracking ref up to date and hands back the remote's published
/// head, or `None` when the remote has no such branch yet.
///
/// A failed fetch is fatal here rather than ignored: it leaves the tracking
/// ref pointing at whatever it happened to hold, which makes the push's lease
/// refuse for "stale info" — a message about the lease, blaming the wrong
/// thing entirely — and makes a pull merge against a state the remote left
/// long ago. This used to be fire-and-forget, so the real error (git could not
/// write the tracking ref) was discarded and only its consequence was seen.
async fn fetch_remote(git: &GitCtl) -> Result<Option<String>> {
    let fetch = git
        .run_hooked_status(&["fetch", "origin", REMOTE_BRANCH], &[])
        .await?;
    if !fetch.status.success() {
        let err = String::from_utf8_lossy(&fetch.stderr);
        let err = err.trim();
        // A remote nobody has published to yet has no such branch. There is
        // then nothing to lease against, and a push is a create rather than a
        // replace, which git allows under a lease.
        if missing_remote_branch(err) {
            return Ok(None);
        }
        anyhow::bail!("{}", fetch_failure(err));
    }
    // The tracking ref is what the lease reads. `FETCH_HEAD` is the fallback
    // for a remote configured without a fetch refspec, where git updates no
    // tracking ref at all.
    match git.rev_parse(&tracking_ref()).await? {
        Some(head) => Ok(Some(head)),
        None => git.rev_parse("FETCH_HEAD").await,
    }
}

/// What pulling would do, worked out without touching the checkout.
#[derive(Debug)]
pub enum Pull {
    /// Nobody has published to the remote yet.
    NothingPublished,
    /// The remote holds nothing this checkout lacks.
    UpToDate,
    /// Work to bring in, ready for `apply_pull`.
    Ready(PullPlan),
}

/// A reconciliation with the remote, computed but not yet landed.
///
/// Worked out and applied in two steps because the second step moves files in
/// the trunk checkout: the caller decides — knowing whether anything will
/// actually change — whether that is a moment to stop workers or reload the
/// gateway, and a pull with nothing to do disturbs neither.
#[derive(Debug)]
pub struct PullPlan {
    /// The remote head being reconciled with.
    remote: String,
    /// Our public head the merge was computed from.
    ours: String,
    /// Trunk's head when the plan was made. It must not move before the plan
    /// lands, or the merged tree would describe a trunk that no longer exists.
    trunk: String,
    /// The merged tree, with this checkout's private paths stripped back out.
    tree: String,
    /// Their commits, oldest first — what the trunk commit records bringing in.
    pub subjects: Vec<String>,
    /// Whether landing this changes any file in the checkout. False when the
    /// remote's work is already here in content and only the two published
    /// histories need joining.
    pub changes_files: bool,
}

/// The outcome of a landed pull.
pub struct Pulled {
    /// The public head, now a descendant of the remote's.
    pub public_head: String,
    /// The commit added to trunk, if the merge changed any file.
    pub trunk_commit: Option<String>,
}

/// Works out how to reconcile this checkout with what is published, without
/// changing anything.
///
/// Publishing replaces the remote's `main` outright, which is safe only while
/// one machine does it. With two, each one's publish would discard the
/// other's; the lease catches that and refuses, and this is the way out of the
/// standoff — take their published work in here first, so the next publish
/// carries both and fast-forwards.
///
/// The merge is between *published* trees: ours (trunk with private paths
/// filtered out) and theirs. That is deliberate. Trunk itself holds things
/// that never leave this machine, and merging the remote into trunk directly
/// would need a base that has never existed anywhere.
pub async fn plan_pull(git: &GitCtl) -> Result<Pull> {
    // Our side has to speak for everything on trunk, including commits made
    // since the last publish — otherwise the merge would offer the remote a
    // stale version of our own work and quietly bring it back over the new
    // one. Exporting is idempotent, so this costs nothing when it is current.
    let export = export_public(git).await?;
    let Some(ours) = export.public_head else {
        anyhow::bail!(
            "there is nothing here to reconcile: trunk has no commits that survive the \
             private-path filter, so this checkout has published nothing and has nothing \
             to merge what is published into."
        );
    };
    let Some(remote) = fetch_remote(git).await? else {
        return Ok(Pull::NothingPublished);
    };
    if git.is_ancestor(&remote, &ours).await? {
        return Ok(Pull::UpToDate);
    }

    let trunk = git.head().await?;
    let Some(base) = pick_base(git, &ours, &remote, &trunk).await? else {
        anyhow::bail!("{}", no_common_base());
    };

    // Both sides are grafted onto the base so git's own merge-base search
    // finds it. Merging the two heads as they stand would either find nothing
    // in common — the normal state before the first pull — or find a point far
    // older than the one they actually last agreed at.
    let merged = match git
        .merge_tree(
            &graft(git, &ours, &base).await?,
            &graft(git, &remote, &base).await?,
        )
        .await?
    {
        MergeTree::Merged { tree } => tree,
        MergeTree::Conflict { paths } => anyhow::bail!("{}", conflicted(&paths)),
    };

    // A directory marked private here stays private even when another
    // checkout publishes something inside it: without this, their file would
    // land in our private directory and the very next export would strip it
    // out again, so the two machines would trade it back and forth forever.
    let tree = strip_private(git, &merged, &private_dirs(git, &trunk).await?).await?;

    let subjects = git
        .log_args(&[&format!("{base}..{remote}")], 100)
        .await?
        .into_iter()
        .rev()
        .map(|c| c.subject)
        .collect();

    let changes_files = tree != tree_of(git, &ours).await?;
    Ok(Pull::Ready(PullPlan {
        remote,
        ours,
        trunk,
        tree,
        subjects,
        changes_files,
    }))
}

/// Lands a plan: the merged content onto trunk, and the remote's history
/// under our own.
///
/// The public merge commit is the point of the whole exercise. It carries the
/// remote's head as a parent, so the next publish is a fast-forward the lease
/// is happy with, and their commits stay in the published history as commits
/// rather than being flattened into ours.
pub async fn apply_pull(git: &GitCtl, plan: PullPlan) -> Result<Pulled> {
    let now = git.head().await?;
    if now != plan.trunk {
        anyhow::bail!(
            "trunk moved from {} to {} while the pull was being worked out, so the merged \
             result no longer describes it. Nothing was changed — pull again.",
            &plan.trunk[..12.min(plan.trunk.len())],
            &now[..12.min(now.len())]
        );
    }
    if plan.changes_files && git.is_dirty().await? {
        anyhow::bail!(
            "the trunk checkout has uncommitted changes and pulling would write over them. \
             Nothing was changed — commit or clean them first."
        );
    }

    let message = pull_message(&plan.subjects);
    // Nothing of ours to keep and their history already contains our published
    // work: take their commit as it stands rather than wrapping it in a merge
    // that says nothing. The common case, and it keeps the published history
    // readable.
    let public = if plan.tree == tree_of(git, &plan.remote).await?
        && git.is_ancestor(&plan.ours, &plan.remote).await?
    {
        plan.remote.clone()
    } else {
        let out = git
            .run_raw(&[
                "commit-tree",
                &plan.tree,
                "-p",
                &plan.ours,
                "-p",
                &plan.remote,
                "-m",
                &message,
            ])
            .await?;
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let mut trunk_commit = None;
    if plan.changes_files {
        // Only the paths that actually differ are touched. Checking the whole
        // tree out would rewrite every file, and a rewritten file is a changed
        // file to everything downstream that watches mtimes — cargo would
        // rebuild the world after every pull.
        //
        // Restore first, remove second, so a failure in between leaves the
        // checkout with extra files rather than missing ones. Nothing here can
        // reach a private path: both sides of the diff had them filtered out.
        let changed = changed_paths(git, &plan.ours, &public, "d").await?;
        if !changed.is_empty() {
            let mut args = vec!["checkout", &public, "--"];
            args.extend(changed.iter().map(String::as_str));
            git.run_raw(&args).await?;
        }
        let dropped = changed_paths(git, &plan.ours, &public, "D").await?;
        if !dropped.is_empty() {
            let mut args = vec!["rm", "-q", "-f", "--ignore-unmatch", "--"];
            args.extend(dropped.iter().map(String::as_str));
            git.run_raw(&args).await?;
        }
        trunk_commit = git.add_all_and_commit(&message).await?;
    }

    // Order matters only in that all three describe the same moment: the
    // export's bookkeeping must agree with the trunk commit that just landed,
    // or the next export would rebuild the merge from the wrong side.
    git.update_ref(PUBLIC_REF, &public).await?;
    git.update_ref(SOURCE_REF, &git.head().await?).await?;
    git.update_ref(REMOTE_SYNC_REF, &plan.remote).await?;

    Ok(Pulled {
        public_head: public,
        trunk_commit,
    })
}

/// Records what is published now as the point this checkout last agreed with
/// the remote, merging nothing.
///
/// The bootstrap for two checkouts that have each been publishing on their
/// own: their exported histories have no commit in common, so there is no base
/// to merge from and no way to tell their changes from ours. Adopting says
/// "what is on the remote is where we last agreed" — after which a pull is a
/// well-defined three-way merge again.
///
/// It is a claim about the past that only the operator can make, and it is
/// wrong if the remote holds work this checkout has never seen: that work then
/// reads as already-had, and the next publish drops it from the content while
/// keeping it in the history. Publish from the other machine and pull here
/// first if that is a possibility.
pub async fn adopt_remote(git: &GitCtl) -> Result<String> {
    let Some(remote) = fetch_remote(git).await? else {
        anyhow::bail!(
            "there is nothing published on origin/{REMOTE_BRANCH} to adopt as a base."
        );
    };
    git.update_ref(REMOTE_SYNC_REF, &remote).await?;
    Ok(remote)
}

/// The commit the two published histories last agreed at.
///
/// Three places can know it, and the most recent one wins: what we recorded at
/// the last publish or pull; where git can see the two published chains fork;
/// and where trunk itself forks from the remote, which is the one that answers
/// for a checkout cloned from the published repository — its trunk *is* the
/// remote's history, while its export is a fresh chain that shares nothing.
async fn pick_base(
    git: &GitCtl,
    ours: &str,
    remote: &str,
    trunk: &str,
) -> Result<Option<String>> {
    let mut candidates = Vec::new();
    if let Some(sync) = git.rev_parse(REMOTE_SYNC_REF).await? {
        candidates.push(sync);
    }
    if let Some(forked) = git.merge_base(ours, remote).await? {
        candidates.push(forked);
    }
    if let Some(forked) = git.merge_base(trunk, remote).await? {
        candidates.push(forked);
    }

    let mut best: Option<(u64, String)> = None;
    for candidate in candidates {
        // A recorded point the remote has since been rewritten past is no
        // longer a state it ever passed through, and merging against it would
        // read the rewrite as ours to undo.
        if !git.is_ancestor(&candidate, remote).await? {
            continue;
        }
        let (_, distance) = git.ahead_behind(&candidate, remote).await?;
        match &best {
            Some((closest, _)) if *closest <= distance => {}
            _ => best = Some((distance, candidate)),
        }
    }
    Ok(best.map(|(_, commit)| commit))
}

/// A throwaway commit carrying `rev`'s tree, parented on `base`.
///
/// `git merge-tree` works out the merge base itself and, before git 2.40, has
/// no way to be told one. Parenting both sides on the base is how it is told.
/// These commits are never referenced, so git gc collects them.
async fn graft(git: &GitCtl, rev: &str, base: &str) -> Result<String> {
    let tree = tree_of(git, rev).await?;
    let out = git
        .run_raw(&["commit-tree", &tree, "-p", base, "-m", "merge base graft"])
        .await?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Paths that differ between two revisions, selected by git's diff filter
/// (`D` for the ones that go away, `d` for everything else).
///
/// Rename detection is off: with it on, a file that moved is reported once,
/// under its new name, and the old one is never removed from the checkout.
async fn changed_paths(git: &GitCtl, from: &str, to: &str, filter: &str) -> Result<Vec<String>> {
    let filter = format!("--diff-filter={filter}");
    let out = git
        .run_raw(&[
            "-c",
            "core.quotePath=false",
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            &filter,
            from,
            to,
        ])
        .await?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect())
}

async fn tree_of(git: &GitCtl, rev: &str) -> Result<String> {
    let spec = format!("{rev}^{{tree}}");
    let out = git.run_raw(&["rev-parse", "--verify", &spec]).await?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn pull_message(subjects: &[String]) -> String {
    let mut msg = format!(
        "Take in {} commit(s) published to origin/{REMOTE_BRANCH}\n\n\
         Merged from what another checkout published. Only paths that leave this\n\
         machine are touched; anything marked private here is untouched.\n",
        subjects.len()
    );
    for subject in subjects.iter().take(20) {
        msg.push_str(&format!("\n  - {subject}"));
    }
    if subjects.len() > 20 {
        msg.push_str(&format!("\n  … and {} more", subjects.len() - 20));
    }
    msg
}

fn not_reconciled(remote: &str) -> String {
    format!(
        "origin/{REMOTE_BRANCH} is at {} — work this checkout has not taken in — and \
         publishing replaces that branch outright, so this would discard it. Nothing was \
         pushed.\n\n\
         Pull first: it merges what is published into trunk here and makes their history \
         part of ours, after which publishing adds to the remote instead of replacing it.",
        &remote[..12.min(remote.len())]
    )
}

fn no_common_base() -> String {
    format!(
        "this checkout and origin/{REMOTE_BRANCH} have no published history in common, so \
         there is no point to merge from: every file would read as changed on both sides. \
         Nothing was changed.\n\n\
         That is the normal state until the first pull, because each checkout derives its \
         own filtered history and the same content becomes different commits on each \
         machine. If what is on the remote is work this checkout already has, adopt it as \
         the base and pull again. If it is work this checkout has never seen, publish from \
         the other machine and pull there first instead."
    )
}

fn conflicted(paths: &[String]) -> String {
    format!(
        "the same files were changed here and in what was published, and git could not \
         merge them on its own. Nothing was changed. Resolve by hand — change these files \
         here so they no longer contradict what is published, commit, and pull again:\n\n  {}",
        paths.join("\n  ")
    )
}

/// Whether a failed fetch means only that the remote has no such branch.
pub(crate) fn missing_remote_branch(stderr: &str) -> bool {
    stderr.contains("couldn't find remote ref")
}

/// Whether a failed git command was blocked by a leftover `.lock` file.
///
/// Git writes a lock beside a ref while updating it and renames it into place
/// when done; a process killed in between leaves the lock behind, and every
/// later update of that ref fails until somebody deletes it. Git's advice for
/// this case leads with "Another git process seems to be running", which sends
/// the reader looking for a process that exited hours ago.
fn stale_lock(stderr: &str) -> bool {
    stderr.contains("cannot lock ref") && stderr.contains("File exists")
}

fn fetch_failure(stderr: &str) -> String {
    let mut msg = format!(
        "could not refresh origin/{REMOTE_BRANCH} before publishing, so the check that keeps \
         this from overwriting someone else's work has nothing current to measure against. \
         Nothing was pushed. git said: {stderr}"
    );
    if stale_lock(stderr) {
        msg.push_str(
            "\n\nThat is a leftover lock file rather than a live git: a fetch was killed \
             partway and never cleaned up after itself. Check that no git process is running \
             in this repository, then delete the .lock file git named above and publish again.",
        );
    }
    msg
}

fn push_failure(stderr: &str) -> String {
    // Git's own wording for a refused lease is "(stale info)", which describes
    // the mechanism and not the cause. Having just fetched successfully, the
    // cause is no longer ambiguous: the remote moved between that fetch and
    // this push.
    if stderr.contains("stale info") {
        return format!(
            "publishing was refused: origin/{REMOTE_BRANCH} moved between the fetch a moment \
             ago and the push, so another checkout published in the meantime. Nothing was \
             pushed and those commits are intact. Pull first: that merges what they \
             published into trunk here, after which publishing carries both and no longer \
             has to replace anything. git said: {stderr}"
        );
    }
    if stale_lock(stderr) {
        return format!(
            "publishing failed on a leftover lock file, not on anything about the commits: a \
             git process was killed partway and never cleaned up after itself. Check that no \
             git process is running in this repository, then delete the .lock file git named \
             below and publish again. git said: {stderr}"
        );
    }
    format!("publishing failed and nothing was pushed. git said: {stderr}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn repo() -> (TempDir, GitCtl) {
        let tmp = TempDir::new().unwrap();
        let git = GitCtl::new(tmp.path());
        git.run_raw(&["init", "-b", "main"]).await.unwrap();
        git.run_raw(&["config", "user.name", "test"]).await.unwrap();
        git.run_raw(&["config", "user.email", "t@example.com"])
            .await
            .unwrap();
        std::fs::write(tmp.path().join("open.txt"), "public\n").unwrap();
        git.add_all_and_commit("base").await.unwrap();
        (tmp, git)
    }

    /// A repo with an `origin` it can actually push to, and a second clone
    /// standing in for whoever else publishes.
    async fn repo_with_remote() -> (TempDir, GitCtl, GitCtl) {
        let (tmp, git) = repo().await;
        // The bare remote and the stand-in checkouts sit inside the repo's own
        // directory, which is convenient but nothing the repo should ever see:
        // otherwise they are untracked changes (which a pull rightly refuses
        // to write over) and `add -A` sweeps them into trunk's commits.
        std::fs::create_dir_all(tmp.path().join(".git/info")).unwrap();
        std::fs::write(
            tmp.path().join(".git/info/exclude"),
            "remote.git/\nother/\nelsewhere/\n",
        )
        .unwrap();
        let remote = tmp.path().join("remote.git");
        let bare = GitCtl::new(&remote);
        std::fs::create_dir_all(&remote).unwrap();
        bare.run_raw(&["init", "--bare", "-b", REMOTE_BRANCH])
            .await
            .unwrap();
        git.run_raw(&["remote", "add", "origin", remote.to_str().unwrap()])
            .await
            .unwrap();
        (tmp, git, bare)
    }

    /// The other machine: a clone of the published repository that makes
    /// `change` to its checkout and publishes it, moving the remote out from
    /// under us. It leaves nothing behind, so the pull it provokes sees only
    /// what was published.
    async fn another_checkout_publishes<F>(tmp: &TempDir, bare: &GitCtl, subject: &str, change: F)
    where
        F: FnOnce(&std::path::Path),
    {
        let dir = tmp.path().join("other");
        std::fs::create_dir_all(&dir).unwrap();
        let other = GitCtl::new(&dir);
        other
            .run_raw(&["clone", bare.dir().to_str().unwrap(), "."])
            .await
            .unwrap();
        other.run_raw(&["config", "user.name", "other"]).await.unwrap();
        other
            .run_raw(&["config", "user.email", "o@example.com"])
            .await
            .unwrap();
        change(&dir);
        other.add_all_and_commit(subject).await.unwrap();
        other
            .run_hooked(&["push", "origin", REMOTE_BRANCH], &[])
            .await
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The plain case: another clone adds a file and publishes it.
    async fn somebody_else_publishes(tmp: &TempDir, bare: &GitCtl) {
        another_checkout_publishes(tmp, bare, "someone else's publish", |dir| {
            std::fs::write(dir.join("theirs.txt"), "theirs\n").unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn a_first_publish_succeeds_though_the_remote_has_no_branch_yet() {
        // Nothing to lease against is not a conflict: the push creates the
        // branch. The fetch fails here ("couldn't find remote ref"), and
        // treating every failed fetch as fatal would block the first publish.
        let (_tmp, git, bare) = repo_with_remote().await;
        export_public(&git).await.unwrap();

        push_public(&git).await.unwrap();
        assert_eq!(
            bare.log(REMOTE_BRANCH, 10).await.unwrap()[0].subject,
            "base"
        );
    }

    #[tokio::test]
    async fn a_leftover_lock_file_is_named_as_such_and_nothing_is_pushed() {
        // The regression this whole path exists for: a git killed mid-fetch
        // leaves a `.lock` beside the tracking ref, every later fetch fails to
        // update it, and the lease — measured against that stale ref — refuses
        // the push with "stale info". The fetch error used to be discarded, so
        // the operator saw only the lease complaint and went looking for a
        // conflicting publish that had never happened.
        let (tmp, git, bare) = repo_with_remote().await;
        export_public(&git).await.unwrap();
        push_public(&git).await.unwrap();

        // The remote moves on, so the tracking ref genuinely needs updating —
        // a lock only bites when there is a write for it to block.
        somebody_else_publishes(&tmp, &bare).await;
        std::fs::write(tmp.path().join("open.txt"), "more public\n").unwrap();
        git.add_all_and_commit("second").await.unwrap();
        export_public(&git).await.unwrap();
        let lock = tmp
            .path()
            .join(format!(".git/refs/remotes/origin/{REMOTE_BRANCH}.lock"));
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        std::fs::write(&lock, "").unwrap();

        let err = push_public(&git).await.unwrap_err().to_string();
        assert!(
            err.contains("leftover lock file"),
            "the lock must be named as the cause: {err}"
        );
        assert!(
            err.contains("Nothing was pushed"),
            "the operator must be told the remote is untouched: {err}"
        );
        assert!(
            !err.contains("stale info"),
            "the lease is not the cause and must not be blamed: {err}"
        );
    }

    #[tokio::test]
    async fn a_remote_that_moved_underneath_us_is_reported_as_someone_else_publishing() {
        let (tmp, git, bare) = repo_with_remote().await;
        export_public(&git).await.unwrap();
        push_public(&git).await.unwrap();

        // Somebody else publishes. Their commit must survive our attempt.
        somebody_else_publishes(&tmp, &bare).await;

        // Our tracking ref is now stale, and the lease is what catches it.
        // Freeze it there: the fetch inside `push_public` would otherwise
        // refresh it and the push would simply overwrite their work.
        std::fs::write(tmp.path().join("open.txt"), "ours\n").unwrap();
        git.add_all_and_commit("ours").await.unwrap();
        export_public(&git).await.unwrap();
        git.run_raw(&["config", "remote.origin.fetch", "+refs/heads/nothing:refs/remotes/origin/nothing"])
            .await
            .unwrap();

        let err = push_public(&git).await.unwrap_err().to_string();
        assert!(
            err.contains("published in the meantime"),
            "a real conflict must read as one: {err}"
        );
        let remote_log = bare.log(REMOTE_BRANCH, 10).await.unwrap();
        assert_eq!(
            remote_log[0].subject, "someone else's publish",
            "their publish must be intact: {remote_log:?}"
        );
    }

    /// The whole point of the pull side: two checkouts publishing to one
    /// remote. Whoever publishes second used to be refused (or, worse, to
    /// quietly replace the other's work); now they pull, and their publish
    /// carries both.
    #[tokio::test]
    async fn pulling_takes_in_another_checkouts_work_and_publishing_then_adds_to_it() {
        let (tmp, git, bare) = repo_with_remote().await;
        export_public(&git).await.unwrap();
        push_public(&git).await.unwrap();

        somebody_else_publishes(&tmp, &bare).await;

        // Meanwhile, work of our own.
        std::fs::write(tmp.path().join("ours.txt"), "ours\n").unwrap();
        git.add_all_and_commit("our own work").await.unwrap();
        export_public(&git).await.unwrap();

        // Publishing now would discard their commit, and says so.
        let refused = push_public(&git).await.unwrap_err().to_string();
        assert!(
            refused.contains("Pull first"),
            "the way out has to be named: {refused}"
        );

        let Pull::Ready(plan) = plan_pull(&git).await.unwrap() else {
            panic!("their commit is not here yet, so there is something to pull");
        };
        assert_eq!(plan.subjects, vec!["someone else's publish".to_string()]);
        assert!(plan.changes_files);
        apply_pull(&git, plan).await.unwrap();

        // Their file is in the checkout, ours is untouched.
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("theirs.txt")).unwrap(),
            "theirs\n"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("ours.txt")).unwrap(),
            "ours\n"
        );

        // And publishing now adds to what they published rather than
        // replacing it.
        push_public(&git).await.unwrap();
        let published = bare.log(REMOTE_BRANCH, 20).await.unwrap();
        let subjects: Vec<&str> = published.iter().map(|c| c.subject.as_str()).collect();
        assert!(
            subjects.contains(&"someone else's publish"),
            "their commit must still be in the published history: {subjects:?}"
        );
        assert!(
            subjects.contains(&"our own work"),
            "and ours must have arrived: {subjects:?}"
        );
    }

    /// Deletion is the half of a pull that can lose work, so it gets its own
    /// test: a file the other checkout removed has to actually go, rather than
    /// linger and be re-published as if nobody had deleted it.
    #[tokio::test]
    async fn a_file_deleted_by_the_other_checkout_goes_here_too() {
        let (tmp, git, bare) = repo_with_remote().await;
        export_public(&git).await.unwrap();
        push_public(&git).await.unwrap();

        another_checkout_publishes(&tmp, &bare, "drop the open file", |dir| {
            std::fs::remove_file(dir.join("open.txt")).unwrap();
        })
        .await;

        let Pull::Ready(plan) = plan_pull(&git).await.unwrap() else {
            panic!("their deletion is something to pull");
        };
        apply_pull(&git, plan).await.unwrap();

        assert!(
            !tmp.path().join("open.txt").exists(),
            "the deleted file must be gone from the checkout too"
        );
        let published = git.tree_files(PUBLIC_REF).await.unwrap();
        assert!(
            !published.contains(&"open.txt".to_string()),
            "and must not come back on the next publish: {published:?}"
        );
    }

    #[tokio::test]
    async fn pulling_when_the_remote_has_not_moved_does_nothing() {
        let (_tmp, git, _bare) = repo_with_remote().await;
        export_public(&git).await.unwrap();
        push_public(&git).await.unwrap();
        let before = git.head().await.unwrap();

        assert!(matches!(plan_pull(&git).await.unwrap(), Pull::UpToDate));
        assert_eq!(git.head().await.unwrap(), before, "trunk must not move");
    }

    #[tokio::test]
    async fn there_is_nothing_to_pull_before_anyone_has_published() {
        let (_tmp, git, _bare) = repo_with_remote().await;
        assert!(matches!(
            plan_pull(&git).await.unwrap(),
            Pull::NothingPublished
        ));
    }

    /// A pull must respect the boundary in both directions: it neither
    /// publishes what is private nor lets the remote write into it. The
    /// second half is the one that bites — another checkout that does not
    /// mark this directory private will publish files inside it, and if a
    /// pull accepted them they would land on top of content that never leaves
    /// this machine.
    #[tokio::test]
    async fn a_private_directory_is_untouched_by_a_pull() {
        let (tmp, git, bare) = repo_with_remote().await;
        let secret = tmp.path().join("skills/company-lore");
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("SKILL.md"), "trade secrets\n").unwrap();
        std::fs::write(secret.join(PRIVATE_MARKER), "").unwrap();
        git.add_all_and_commit("a private skill").await.unwrap();
        export_public(&git).await.unwrap();
        push_public(&git).await.unwrap();

        // The other checkout has no such marker — it never saw the directory
        // at all — and publishes something at the same path.
        another_checkout_publishes(&tmp, &bare, "write into a directory we keep private", |dir| {
            std::fs::create_dir_all(dir.join("skills/company-lore")).unwrap();
            std::fs::write(dir.join("skills/company-lore/SKILL.md"), "theirs\n").unwrap();
            std::fs::write(dir.join("open2.txt"), "theirs\n").unwrap();
        })
        .await;

        let Pull::Ready(plan) = plan_pull(&git).await.unwrap() else {
            panic!("there is something to pull");
        };
        apply_pull(&git, plan).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(secret.join("SKILL.md")).unwrap(),
            "trade secrets\n",
            "the private file must survive the pull"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("open2.txt")).unwrap(),
            "theirs\n",
            "everything outside the private directory still arrives"
        );
        let published = git.tree_files(PUBLIC_REF).await.unwrap();
        assert!(
            !published.iter().any(|p| p.contains("company-lore")),
            "and the merged export still carries nothing private: {published:?}"
        );
    }

    /// Two checkouts that have each published on their own share no commit:
    /// each derives its own chain, so the same content is different objects on
    /// each machine. There is then no base to merge from, and guessing one
    /// would read every file as changed on both sides.
    #[tokio::test]
    async fn independently_published_histories_are_reconciled_only_after_adopting() {
        let (tmp, git, bare) = repo_with_remote().await;

        // Somebody else's published history, built from a repository of their
        // own — unrelated to anything here.
        let dir = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&dir).unwrap();
        let other = GitCtl::new(&dir);
        other.run_raw(&["init", "-b", "main"]).await.unwrap();
        other.run_raw(&["config", "user.name", "other"]).await.unwrap();
        other
            .run_raw(&["config", "user.email", "o@example.com"])
            .await
            .unwrap();
        std::fs::write(dir.join("open.txt"), "public\n").unwrap();
        other.add_all_and_commit("their base").await.unwrap();
        other
            .run_raw(&["remote", "add", "origin", bare.dir().to_str().unwrap()])
            .await
            .unwrap();
        other
            .run_hooked(&["push", "origin", "main"], &[])
            .await
            .unwrap();

        let err = plan_pull(&git).await.unwrap_err().to_string();
        assert!(
            err.contains("no published history in common"),
            "the reason has to be the missing base: {err}"
        );
        assert!(
            err.contains("adopt"),
            "and the way out has to be named: {err}"
        );

        adopt_remote(&git).await.unwrap();
        let Pull::Ready(plan) = plan_pull(&git).await.unwrap() else {
            panic!("adopting joins nothing by itself; the pull still has to run");
        };
        assert!(
            !plan.changes_files,
            "their content is the same as ours, so no file changes"
        );
        apply_pull(&git, plan).await.unwrap();

        // The histories are joined now, so publishing is a fast-forward.
        push_public(&git).await.unwrap();
        let subjects: Vec<String> = bare
            .log(REMOTE_BRANCH, 20)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.subject)
            .collect();
        assert!(
            subjects.iter().any(|s| s == "their base"),
            "their history must survive our publish: {subjects:?}"
        );
    }

    #[tokio::test]
    async fn a_pull_that_cannot_be_merged_changes_nothing_and_names_the_files() {
        let (tmp, git, bare) = repo_with_remote().await;
        export_public(&git).await.unwrap();
        push_public(&git).await.unwrap();

        // Both checkouts change the same file, differently.
        another_checkout_publishes(&tmp, &bare, "their edit", |dir| {
            std::fs::write(dir.join("open.txt"), "their edit\n").unwrap();
        })
        .await;

        std::fs::write(tmp.path().join("open.txt"), "our edit\n").unwrap();
        git.add_all_and_commit("our edit").await.unwrap();
        let before = git.head().await.unwrap();

        let err = plan_pull(&git).await.unwrap_err().to_string();
        assert!(
            err.contains("open.txt"),
            "the file to fix must be named: {err}"
        );
        assert_eq!(git.head().await.unwrap(), before, "trunk must not move");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("open.txt")).unwrap(),
            "our edit\n",
            "and the checkout must be left alone"
        );
    }

    #[tokio::test]
    async fn a_marker_written_before_the_rename_still_hides_its_directory() {
        // A marker is a safety mechanism. Ceasing to honour one because the
        // project changed names is how a directory somebody deliberately
        // marked private gets published.
        let (tmp, git) = repo().await;
        let secret = tmp.path().join("tools/legacy-secret");
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("lib.rs"), "private\n").unwrap();
        std::fs::write(secret.join(LEGACY_PRIVATE_MARKER), "").unwrap();
        std::fs::write(tmp.path().join("open.txt"), "public\n").unwrap();
        git.add_all_and_commit("with a pre-rename marker").await.unwrap();

        export_public(&git).await.unwrap();
        let files = git.tree_files(PUBLIC_REF).await.unwrap();
        assert!(
            files.iter().any(|f| f == "open.txt"),
            "public content must survive: {files:?}"
        );
        assert!(
            !files.iter().any(|f| f.starts_with("tools/legacy-secret")),
            "the old marker must still hide its directory: {files:?}"
        );
    }

    #[tokio::test]
    async fn private_paths_never_reach_the_public_branch() {
        let (tmp, git) = repo().await;

        // A private skill and a public tool, in one commit.
        let secret = tmp.path().join("skills/company-lore");
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("SKILL.md"), "trade secrets\n").unwrap();
        std::fs::write(secret.join(PRIVATE_MARKER), "").unwrap();
        let open_tool = tmp.path().join("tools/greeter");
        std::fs::create_dir_all(&open_tool).unwrap();
        std::fs::write(open_tool.join("lib.rs"), "hello\n").unwrap();
        git.add_all_and_commit("add a private skill and a public tool")
            .await
            .unwrap();

        let export = export_public(&git).await.unwrap();
        assert_eq!(export.commits, 2, "base plus the new commit");

        let published = git.tree_files("public").await.unwrap();
        assert!(published.contains(&"open.txt".to_string()));
        assert!(published.contains(&"tools/greeter/lib.rs".to_string()));
        assert!(
            !published.iter().any(|p| p.contains("company-lore")),
            "nothing under the private dir may be published: {published:?}"
        );

        // Message mapping is one-to-one.
        let log = git.log("public", 10).await.unwrap();
        assert_eq!(log[0].subject, "add a private skill and a public tool");
        assert_eq!(log[1].subject, "base");
    }

    #[tokio::test]
    async fn export_is_incremental_and_idempotent() {
        let (tmp, git) = repo().await;
        assert_eq!(export_public(&git).await.unwrap().commits, 1);
        assert_eq!(export_public(&git).await.unwrap().commits, 0, "nothing new");

        std::fs::write(tmp.path().join("more.txt"), "x\n").unwrap();
        git.add_all_and_commit("more").await.unwrap();
        assert_eq!(export_public(&git).await.unwrap().commits, 1);
        assert_eq!(git.log("public", 10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn marking_private_later_stops_future_exports_only() {
        let (tmp, git) = repo().await;
        let dir = tmp.path().join("skills/notes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "v1 was public\n").unwrap();
        git.add_all_and_commit("public at first").await.unwrap();
        export_public(&git).await.unwrap();
        assert!(git
            .tree_files("public")
            .await
            .unwrap()
            .contains(&"skills/notes/SKILL.md".to_string()));

        // Now mark it private and change it.
        std::fs::write(dir.join(PRIVATE_MARKER), "").unwrap();
        std::fs::write(dir.join("SKILL.md"), "v2 is private\n").unwrap();
        git.add_all_and_commit("goes private").await.unwrap();
        export_public(&git).await.unwrap();

        let published = git.tree_files("public").await.unwrap();
        assert!(
            !published.contains(&"skills/notes/SKILL.md".to_string()),
            "the tip of public no longer carries it"
        );
        // ...but history does, as documented: scrubbing is a manual act.
        let old = git
            .tree_files("public~1")
            .await
            .unwrap()
            .contains(&"skills/notes/SKILL.md".to_string());
        assert!(old, "already-exported history is untouched");
    }

    #[tokio::test]
    async fn only_the_filtered_export_can_become_the_remote_main() {
        let (tmp, git) = repo().await;
        install_push_guard(&git).await.unwrap();

        // A bare "remote" to push at.
        let remote = tmp.path().join("remote.git");
        git.run_raw(&["init", "--bare", remote.to_str().unwrap()])
            .await
            .unwrap();
        git.run_raw(&["remote", "add", "origin", remote.to_str().unwrap()])
            .await
            .unwrap();

        // Local main is trunk and may carry private paths, so pushing it is
        // refused even though the remote branch it would land on is called
        // main too. (The guard is a hook, so this must go hooked.)
        let refused = git.run_hooked(&["push", "origin", "main"], &[]).await;
        assert!(refused.is_err(), "pushing trunk must be refused");

        // ...but the filtered export becomes the remote's main, which is what
        // "published" means here.
        export_public(&git).await.unwrap();
        git.run_hooked(&["push", "origin", "public:main"], &[])
            .await
            .unwrap();
        let published = git
            .run_raw(&["ls-remote", "origin", "refs/heads/main"])
            .await
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&published.stdout).trim().is_empty(),
            "the remote's main must now exist"
        );

        // The benchmark results branch is allowed through too. It is not a
        // filtered view of anything: `bench::publish_bench` builds each commit
        // file by file from the generator's staging directory, so it carries no
        // trunk history and shares no objects with it. If this is ever refused,
        // publishing from /admin silently stops recording datapoints -- the
        // failure is best-effort and only shows up as a line in the message.
        git.run_raw(&["update-ref", crate::bench::BENCH_REF, "public"])
            .await
            .unwrap();
        git.run_hooked(&["push", "origin", "bench-results:bench-results"], &[])
            .await
            .unwrap();

        // ...and the documented override works.
        git.run_hooked(&["push", "origin", "main"], &[("THETIS_ALLOW_PUSH", "1")])
            .await
            .unwrap();
    }
}
