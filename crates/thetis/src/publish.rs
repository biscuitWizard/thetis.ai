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

use anyhow::{Context, Result};

use crate::gitctl::GitCtl;

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

/// `commit`'s tree with the private paths of *that commit* removed, built in
/// a temporary index so the working tree and real index are never touched.
async fn filtered_tree(git: &GitCtl, commit: &str) -> Result<String> {
    let private = private_dirs(git, commit).await?;

    let tmp = tempfile_path(git, commit)?;
    let tmp_str = tmp.to_string_lossy().to_string();
    let env: &[(&str, &str)] = &[("GIT_INDEX_FILE", &tmp_str)];

    let result = async {
        git.run_with_env(&["read-tree", commit], env).await?;
        if !private.is_empty() {
            let mut args: Vec<&str> = vec!["rm", "-r", "-q", "--cached", "--ignore-unmatch", "--"];
            for dir in &private {
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
    result.with_context(|| format!("filtering commit {commit}"))
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
if [ -n "$THETIS_ALLOW_PUSH" ]; then exit 0; fi
while read local_ref local_sha remote_ref remote_sha; do
  case "$local_ref" in
    refs/heads/public|"") ;;
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
/// filtered history goes. It is not a fast-forward of anything local — the
/// filtered chain is derived, and rewriting trunk's history rewrites it too —
/// so publishing necessarily replaces what is there. The lease is what keeps
/// that honest: it refuses if the remote has moved since we last saw it, so
/// this can overwrite our own previous publish and never somebody else's.
pub async fn push_public(git: &GitCtl) -> Result<()> {
    // The lease is measured against `refs/remotes/<remote>/<branch>`, not
    // against the remote itself, so it is only as trustworthy as our last
    // successful fetch. A fetch that fails leaves that ref pointing at
    // whatever it happened to hold, and the push is then refused for "stale
    // info" — a message about the lease, blaming the wrong thing entirely.
    // This used to be fire-and-forget, which meant the real error (git could
    // not write the tracking ref) was discarded and only its consequence was
    // ever shown.
    let fetch = git
        .run_hooked_status(&["fetch", "origin", REMOTE_BRANCH], &[])
        .await?;
    if !fetch.status.success() {
        let err = String::from_utf8_lossy(&fetch.stderr);
        let err = err.trim();
        // A remote nobody has published to yet has no such branch. There is
        // then nothing to lease against, and the push below is a create
        // rather than a replace, which git allows under a lease.
        if !missing_remote_branch(err) {
            anyhow::bail!("{}", fetch_failure(err));
        }
    }

    let refspec = format!("public:{REMOTE_BRANCH}");
    let lease = format!("--force-with-lease={REMOTE_BRANCH}");
    let push = git
        .run_hooked_status(&["push", &lease, "origin", &refspec], &[])
        .await?;
    if !push.status.success() {
        let err = String::from_utf8_lossy(&push.stderr);
        anyhow::bail!("{}", push_failure(err.trim()));
    }
    Ok(())
}

/// Whether a failed fetch means only that the remote has no such branch.
fn missing_remote_branch(stderr: &str) -> bool {
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
             ago and the push, so something else published in the meantime. Nothing was \
             pushed and those commits are intact. Check what is on the remote before \
             publishing again — this push replaces {REMOTE_BRANCH} outright. git said: {stderr}"
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

    /// Another clone publishes to the same remote, moving it out from under
    /// us — the state both the lease and a stale tracking ref have to cope
    /// with.
    async fn somebody_else_publishes(tmp: &TempDir, bare: &GitCtl) {
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
        std::fs::write(dir.join("theirs.txt"), "theirs\n").unwrap();
        other
            .add_all_and_commit("someone else's publish")
            .await
            .unwrap();
        other
            .run_hooked(&["push", "origin", REMOTE_BRANCH], &[])
            .await
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
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

        // ...and the documented override works.
        git.run_hooked(&["push", "origin", "main"], &[("THETIS_ALLOW_PUSH", "1")])
            .await
            .unwrap();
    }
}
