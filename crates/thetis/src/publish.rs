//! The publish boundary: what may leave this machine.
//!
//! Tools and skills marked private — a `.thetis-private` file in their
//! directory — stay fully tracked locally (branching, merging, and resets all
//! need them), so privacy is enforced where it actually matters: at the push.
//! A derived `public` branch mirrors trunk commit-for-commit with private
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
pub const PUBLIC_REF: &str = "refs/heads/public";
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
# Only the filtered `public` branch may leave this machine: trunk and the
# conversation branches can carry private tools and skills. Export from
# /admin, then push `public`. Set THETIS_ALLOW_PUSH=1 to override once.
if [ -n "$THETIS_ALLOW_PUSH" ]; then exit 0; fi
while read local_ref local_sha remote_ref remote_sha; do
  case "$local_ref" in
    refs/heads/public|"") ;;
    *)
      echo "thetis: refusing to push $local_ref — only 'public' leaves this machine." >&2
      echo "thetis: export it from /admin, or set THETIS_ALLOW_PUSH=1 to override." >&2
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
    async fn the_push_guard_blocks_everything_but_public() {
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

        // main is refused... (the guard is a hook, so this must go hooked)
        let refused = git.run_hooked(&["push", "origin", "main"], &[]).await;
        assert!(refused.is_err(), "pushing main must be refused");

        // ...public goes through.
        export_public(&git).await.unwrap();
        git.run_hooked(&["push", "origin", "public"], &[]).await.unwrap();

        // ...and the documented override works.
        git.run_hooked(&["push", "origin", "main"], &[("THETIS_ALLOW_PUSH", "1")])
            .await
            .unwrap();
    }
}
