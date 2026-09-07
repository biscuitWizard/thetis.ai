//! Publishing benchmark results to their own remote branch.
//!
//! The retrieval benchmark produces two things worth keeping: a pair of SVG
//! infographics that say how well skill retrieval and tool routing currently
//! work, and a one-line JSON datapoint recording the numbers behind them. This
//! module puts both on `bench-results` on the remote, as a side effect of
//! publishing from /admin.
//!
//! Three properties are deliberate:
//!
//! - **The branch accumulates.** Each publish commits on top of the previous
//!   `bench-results` head and appends to `results.jsonl`, so the file grows
//!   into a series that can be plotted over time. Losing it would reduce the
//!   branch to "the latest run", which is the one thing the datapoints are not
//!   for.
//! - **It is best-effort.** A missing embeddings key, an unbuildable harness or
//!   a machine with no network all mean "no new datapoint", not "trunk cannot
//!   be published". The caller reports what happened and carries on.
//! - **It carries no private paths.** Unlike the public export there is nothing
//!   to filter, because the tree is built from a staging directory the
//!   generator wrote, not from a commit. Whatever the generator did not put
//!   there cannot leak.

use anyhow::{Context, Result};

use crate::gitctl::GitCtl;

/// The local ref holding the published result history.
pub const BENCH_REF: &str = "refs/heads/bench-results";

/// The branch results land on, on the remote.
pub const BENCH_REMOTE_BRANCH: &str = "bench-results";

/// The append-only series. Carried forward from the previous commit on every
/// publish, so the branch holds every datapoint ever recorded.
const SERIES: &str = "results.jsonl";

/// What the generator leaves for us to append to `SERIES`: one JSON object on
/// one line. Not committed under this name — its content becomes the new last
/// line of `results.jsonl`.
const PENDING: &str = "datapoint.jsonl";

/// The outcome of a publish attempt, for the operator-facing message.
#[derive(Debug, Clone)]
pub struct BenchPublish {
    /// The commit now on `bench-results`.
    pub head: String,
    /// How many datapoints the series holds after this publish.
    pub datapoints: usize,
    /// Files committed, for the message.
    pub files: usize,
}

/// Commits everything in `staging` to `bench-results` and pushes it.
///
/// Returns `Ok(None)` when there is nothing to publish — no staging directory,
/// or an empty one — which is the normal state on a checkout that has never
/// run the benchmark. Errors are real failures: git refused, or the push did.
pub async fn publish_bench(git: &GitCtl, staging: &std::path::Path) -> Result<Option<BenchPublish>> {
    let mut files = collect(staging)?;
    if files.is_empty() {
        return Ok(None);
    }

    // The series is the one file that is not simply taken from staging: its
    // previous content has to survive, or the branch stops being a history.
    let parent = git.rev_parse(BENCH_REF).await?;
    let pending = files
        .iter()
        .position(|(rel, _)| rel == PENDING)
        .map(|i| files.remove(i));

    let series = match &pending {
        Some((_, path)) => {
            let line = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let previous = match &parent {
                Some(rev) => show_blob(git, rev, SERIES).await?.unwrap_or_default(),
                None => String::new(),
            };
            Some(append_line(&previous, line.trim()))
        }
        None => None,
    };

    // Anything the generator wrote under the series' own name is ignored in
    // favour of the carried-forward one: a generator that only ever sees this
    // run cannot be trusted to reproduce the history it never read.
    if series.is_some() {
        files.retain(|(rel, _)| rel != SERIES);
    }

    let count = files.len() + usize::from(series.is_some());
    let datapoints = series
        .as_deref()
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);

    let tree = build_tree(git, &files, series.as_deref()).await?;
    let message = format!(
        "Publish retrieval benchmark results\n\n{count} file(s), {datapoints} datapoint(s) in \
         {SERIES}."
    );

    let mut args = vec!["commit-tree", &tree];
    if let Some(rev) = &parent {
        args.push("-p");
        args.push(rev);
    }
    args.push("-m");
    args.push(&message);
    let out = git.run_raw(&args).await?;
    let head = String::from_utf8_lossy(&out.stdout).trim().to_string();

    git.update_ref(BENCH_REF, &head).await?;

    // Results are derived and regenerated wholesale, so the remote branch is
    // ours to replace. The lease still guards against clobbering a push we
    // never saw: a second checkout publishing results is a real conflict, and
    // the right answer is to fetch and re-run rather than overwrite.
    let refspec = format!("{BENCH_REMOTE_BRANCH}:{BENCH_REMOTE_BRANCH}");
    let push = git
        .run_hooked_status(&["push", "--force-with-lease", "origin", &refspec], &[])
        .await?;
    if !push.status.success() {
        let err = String::from_utf8_lossy(&push.stderr);
        anyhow::bail!("publishing benchmark results failed: {}", err.trim());
    }

    Ok(Some(BenchPublish {
        head,
        datapoints,
        files: count,
    }))
}

/// Appends `line` to `previous`, keeping exactly one trailing newline and
/// dropping a blank line rather than recording an empty datapoint.
fn append_line(previous: &str, line: &str) -> String {
    let mut out = String::new();
    for existing in previous.lines().filter(|l| !l.trim().is_empty()) {
        out.push_str(existing);
        out.push('\n');
    }
    if !line.is_empty() {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// One file's contents at a revision, or `None` when the path is not there.
async fn show_blob(git: &GitCtl, rev: &str, path: &str) -> Result<Option<String>> {
    let spec = format!("{rev}:{path}");
    let out = git.run_hooked_status(&["show", &spec], &[]).await?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).to_string()))
}

/// Every file under `dir`, as (path relative to `dir`, absolute path), sorted
/// so the tree is built in a stable order.
fn collect(dir: &std::path::Path) -> Result<Vec<(String, std::path::PathBuf)>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    walk(dir, dir, &mut found)?;
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

fn walk(
    root: &std::path::Path,
    dir: &std::path::Path,
    found: &mut Vec<(String, std::path::PathBuf)>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        // Symlinks are not followed: a staging directory is written by a
        // script, and a link out of it would put an arbitrary file on a public
        // branch.
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            tracing::warn!(path = %path.display(), "skipping symlink in bench staging");
            continue;
        }
        if meta.is_dir() {
            walk(root, &path, found)?;
        } else if meta.is_file() {
            let rel = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            found.push((rel, path));
        }
    }
    Ok(())
}

/// Writes the files into a temporary index and returns the tree they form.
///
/// A temporary index rather than `mktree` because it handles nesting for free,
/// and rather than the real index because nothing here should touch the
/// checkout the operator is looking at.
async fn build_tree(
    git: &GitCtl,
    files: &[(String, std::path::PathBuf)],
    series: Option<&str>,
) -> Result<String> {
    let tmp = std::env::temp_dir().join(format!(
        "thetis-bench-index-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let tmp_str = tmp.to_string_lossy().to_string();
    let env: &[(&str, &str)] = &[("GIT_INDEX_FILE", &tmp_str)];

    let result = async {
        for (rel, path) in files {
            let path_str = path.to_string_lossy().to_string();
            let out = git.run_with_env(&["hash-object", "-w", &path_str], env).await?;
            let oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let cacheinfo = format!("100644,{oid},{rel}");
            git.run_with_env(&["update-index", "--add", "--cacheinfo", &cacheinfo], env)
                .await?;
        }

        if let Some(series) = series {
            // The carried-forward series exists only in memory, so it needs a
            // blob of its own before it can be indexed. `--stdin` would be
            // neater but the plumbing here takes arguments, not input.
            let scratch = tmp.with_extension("series");
            std::fs::write(&scratch, series)?;
            let scratch_str = scratch.to_string_lossy().to_string();
            let out = git
                .run_with_env(&["hash-object", "-w", &scratch_str], env)
                .await?;
            let oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let _ = std::fs::remove_file(&scratch);
            let cacheinfo = format!("100644,{oid},{SERIES}");
            git.run_with_env(&["update-index", "--add", "--cacheinfo", &cacheinfo], env)
                .await?;
        }

        let out = git.run_with_env(&["write-tree"], env).await?;
        Ok::<String, anyhow::Error>(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
    .await;

    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("lock"));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appending_keeps_previous_datapoints() {
        let out = append_line("{\"a\":1}\n{\"a\":2}\n", "{\"a\":3}");
        assert_eq!(out, "{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n");
        assert_eq!(out.lines().count(), 3);
    }

    #[test]
    fn appending_to_nothing_starts_the_series() {
        assert_eq!(append_line("", "{\"a\":1}"), "{\"a\":1}\n");
    }

    #[test]
    fn a_missing_trailing_newline_does_not_join_two_datapoints() {
        // A generator that wrote the previous file without a final newline
        // would otherwise produce `{"a":1}{"a":2}` — one unparseable line.
        let out = append_line("{\"a\":1}", "{\"a\":2}");
        assert_eq!(out, "{\"a\":1}\n{\"a\":2}\n");
    }

    #[test]
    fn blank_lines_are_not_datapoints() {
        let out = append_line("{\"a\":1}\n\n\n", "");
        assert_eq!(out, "{\"a\":1}\n");
    }

    #[test]
    fn collecting_a_missing_directory_is_empty_not_an_error() {
        let found = collect(std::path::Path::new("/nonexistent-bench-staging")).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn collect_finds_nested_files_in_a_stable_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("b.svg"), "b").unwrap();
        std::fs::write(dir.path().join("a.svg"), "a").unwrap();
        std::fs::write(dir.path().join("sub/c.json"), "c").unwrap();

        let found = collect(dir.path()).unwrap();
        let names: Vec<&str> = found.iter().map(|(rel, _)| rel.as_str()).collect();
        assert_eq!(names, vec!["a.svg", "b.svg", "sub/c.json"]);
    }

    /// A repo with a bare `origin`, matching `publish::tests::repo_with_remote`
    /// but without the public-export machinery, which results do not use.
    async fn repo_with_remote() -> (tempfile::TempDir, GitCtl, GitCtl) {
        let tmp = tempfile::tempdir().unwrap();
        let git = GitCtl::new(tmp.path());
        git.run_raw(&["init", "-b", "main"]).await.unwrap();
        git.run_raw(&["config", "user.name", "t"]).await.unwrap();
        git.run_raw(&["config", "user.email", "t@example.com"])
            .await
            .unwrap();
        std::fs::create_dir_all(tmp.path().join(".git/info")).unwrap();
        std::fs::write(tmp.path().join(".git/info/exclude"), "remote.git/\nstaging/\n").unwrap();
        std::fs::write(tmp.path().join("f.txt"), "x\n").unwrap();
        git.add_all_and_commit("base").await.unwrap();

        let remote = tmp.path().join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        let bare = GitCtl::new(&remote);
        bare.run_raw(&["init", "--bare", "-b", BENCH_REMOTE_BRANCH])
            .await
            .unwrap();
        git.run_raw(&["remote", "add", "origin", remote.to_str().unwrap()])
            .await
            .unwrap();
        (tmp, git, bare)
    }

    /// Writes a staging directory as the generator would.
    fn stage(dir: &std::path::Path, datapoint: &str, chart: &str) {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(PENDING), format!("{datapoint}\n")).unwrap();
        std::fs::write(dir.join("skill-retrieval.svg"), chart).unwrap();
        std::fs::write(dir.join("tool-routing.svg"), "<svg/>").unwrap();
    }

    #[tokio::test]
    async fn nothing_staged_is_nothing_published() {
        // The state of every checkout that has never run the benchmark. It must
        // not be an error, or it would break publishing trunk.
        let (tmp, git, _bare) = repo_with_remote().await;
        let missing = tmp.path().join("staging");
        assert!(publish_bench(&git, &missing).await.unwrap().is_none());

        std::fs::create_dir_all(&missing).unwrap();
        assert!(publish_bench(&git, &missing).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_series_accumulates_across_publishes() {
        // The property the whole module exists for: datapoint number two must
        // not replace datapoint number one, because a series of length 1 cannot
        // be plotted over time.
        let (tmp, git, bare) = repo_with_remote().await;
        let staging = tmp.path().join("staging");

        stage(&staging, r#"{"n":1}"#, "<svg>first</svg>");
        let first = publish_bench(&git, &staging).await.unwrap().unwrap();
        assert_eq!(first.datapoints, 1);

        stage(&staging, r#"{"n":2}"#, "<svg>second</svg>");
        let second = publish_bench(&git, &staging).await.unwrap().unwrap();
        assert_eq!(second.datapoints, 2, "the first datapoint must survive");

        // Read it back off the remote, not the local ref: that is what a reader
        // of the branch actually sees.
        let series = String::from_utf8_lossy(
            &bare
                .run_raw(&["show", &format!("{BENCH_REMOTE_BRANCH}:{SERIES}")])
                .await
                .unwrap()
                .stdout,
        )
        .to_string();
        assert_eq!(series, "{\"n\":1}\n{\"n\":2}\n");

        // Charts are replaced wholesale, unlike the series.
        let chart = String::from_utf8_lossy(
            &bare
                .run_raw(&["show", &format!("{BENCH_REMOTE_BRANCH}:skill-retrieval.svg")])
                .await
                .unwrap()
                .stdout,
        )
        .to_string();
        assert_eq!(chart, "<svg>second</svg>");

        // And the branch is a history, not a series of orphans.
        let commits = bare.log(BENCH_REMOTE_BRANCH, 10).await.unwrap();
        assert_eq!(commits.len(), 2, "each publish is its own commit");
    }

    #[tokio::test]
    async fn results_do_not_share_history_with_trunk() {
        // The branch is pushed past the privacy guard, so it must be provably
        // disjoint from trunk: no trunk commit reachable from it, and none of
        // trunk's files in its tree.
        let (tmp, git, _bare) = repo_with_remote().await;
        let staging = tmp.path().join("staging");
        stage(&staging, r#"{"n":1}"#, "<svg/>");
        publish_bench(&git, &staging).await.unwrap();

        let trunk = git.rev_parse("refs/heads/main").await.unwrap().unwrap();
        assert!(
            !git.is_ancestor(&trunk, BENCH_REF).await.unwrap(),
            "no trunk commit may be reachable from the results branch"
        );

        let files = git.tree_files(BENCH_REF).await.unwrap();
        assert!(
            !files.iter().any(|f| f == "f.txt"),
            "trunk's files must not appear on the results branch: {files:?}"
        );
        let mut expected = vec![
            "README.md".to_string(),
            SERIES.to_string(),
            "skill-retrieval.svg".to_string(),
            "tool-routing.svg".to_string(),
        ];
        expected.retain(|f| f != "README.md"); // this fixture writes no README
        let mut got = files.clone();
        got.sort();
        expected.sort();
        assert_eq!(got, expected);
    }

    #[tokio::test]
    async fn charts_without_a_datapoint_still_publish() {
        // A generator that drew charts but failed to assemble a datapoint has
        // still produced something worth looking at.
        let (tmp, git, _bare) = repo_with_remote().await;
        let staging = tmp.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("tool-routing.svg"), "<svg/>").unwrap();

        let out = publish_bench(&git, &staging).await.unwrap().unwrap();
        assert_eq!(out.datapoints, 0);
        let files = git.tree_files(BENCH_REF).await.unwrap();
        assert_eq!(files, vec!["tool-routing.svg".to_string()]);
    }

    #[tokio::test]
    async fn a_generator_written_series_cannot_erase_the_history() {
        // If the generator writes `results.jsonl` itself, it only knows about
        // the run it just did. Honouring that file would silently truncate the
        // series to one row -- the exact failure this guards.
        let (tmp, git, _bare) = repo_with_remote().await;
        let staging = tmp.path().join("staging");

        stage(&staging, r#"{"n":1}"#, "<svg/>");
        publish_bench(&git, &staging).await.unwrap();

        stage(&staging, r#"{"n":2}"#, "<svg/>");
        std::fs::write(staging.join(SERIES), "{\"rogue\":true}\n").unwrap();
        let out = publish_bench(&git, &staging).await.unwrap().unwrap();

        assert_eq!(out.datapoints, 2);
        let series = show_blob(&git, BENCH_REF, SERIES).await.unwrap().unwrap();
        assert_eq!(series, "{\"n\":1}\n{\"n\":2}\n");
        assert!(!series.contains("rogue"));
    }

    #[test]
    fn collect_skips_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.svg"), "real").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("leak.txt")).unwrap();

        let found = collect(dir.path()).unwrap();
        let names: Vec<&str> = found.iter().map(|(rel, _)| rel.as_str()).collect();
        assert_eq!(names, vec!["real.svg"]);
    }
}
