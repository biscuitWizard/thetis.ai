//! Publishes the *real* generator output through the real git path.
//!
//! The unit tests in `bench.rs` use fixture files with contrived contents, which
//! is right for testing the append logic but leaves one gap: they never see what
//! `publish-graphs.sh` actually produces. Real SVGs are ~60KB of markup with
//! `<`, `&`, `#` and non-ASCII glyphs in them, and the real datapoint is a long
//! single line with no trailing newline guarantee. Those are exactly the shapes
//! that break shell-quoting or line-joining bugs, so this test drives the
//! genuine artifacts if they are present.
//!
//! Skipped, not failed, when the generator has not been run on this machine:
//! the staging directory lives outside the repo and CI need not have it.

use std::path::{Path, PathBuf};

use thetis::bench;
use thetis::gitctl::GitCtl;

fn staging() -> Option<PathBuf> {
    let dir = std::env::var("BENCH_STAGING")
        .unwrap_or_else(|_| "/opt/thetis/workspace/zero-retrieval-bench/staging".into());
    let dir = PathBuf::from(dir);
    // Require the artifacts that matter, so a half-written directory skips
    // rather than producing a green run that proved nothing.
    let needed = ["datapoint.jsonl", "tool-routing.svg", "skill-retrieval.svg"];
    if needed.iter().all(|f| dir.join(f).is_file()) {
        Some(dir)
    } else {
        None
    }
}

async fn repo_with_remote(root: &Path) -> GitCtl {
    let git = GitCtl::new(root.to_path_buf());
    git.run_raw(&["init", "-b", "main"]).await.unwrap();
    git.run_raw(&["config", "user.name", "t"]).await.unwrap();
    git.run_raw(&["config", "user.email", "t@e"]).await.unwrap();
    std::fs::write(root.join("f.txt"), "base\n").unwrap();
    git.add_all_and_commit("base").await.unwrap();

    let remote = root.join("remote.git");
    git.run_raw(&["init", "--bare", remote.to_str().unwrap()])
        .await
        .unwrap();
    git.run_raw(&["remote", "add", "origin", remote.to_str().unwrap()])
        .await
        .unwrap();
    git
}

#[tokio::test]
async fn the_real_charts_and_datapoint_survive_the_publish_path() {
    let Some(staging) = staging() else {
        eprintln!("skipped: no generator output staged; run publish-graphs.sh first");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let git = repo_with_remote(tmp.path()).await;

    let first = bench::publish_bench(&git, &staging)
        .await
        .unwrap()
        .expect("real artifacts must publish");
    assert_eq!(first.datapoints, 1, "the series starts at one datapoint");

    // Publishing twice is what actually happens over a project's life, and it is
    // where an append bug would show: the second run must add a line, not
    // replace the file or concatenate two objects onto one line.
    let second = bench::publish_bench(&git, &staging)
        .await
        .unwrap()
        .expect("second publish must land");
    assert_eq!(second.datapoints, 2, "the series accumulates");

    // Read the series back off the branch and confirm every line is a parseable
    // JSON object carrying the metrics we claim to trend. A line that joined two
    // datapoints, or lost its metrics, fails here rather than silently producing
    // an unplottable history.
    let out = git
        .run_raw(&["show", "bench-results:results.jsonl"])
        .await
        .unwrap();
    let series = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = series.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 2, "two datapoints, one per line");

    for line in &lines {
        let v: serde_json::Value = serde_json::from_str(line).expect("each line is one JSON object");
        for key in [
            "schema",
            "commit",
            "mode",
            "skills_dense_ndcg",
            "skills_dense_hit1",
            "skills_dense_mrr",
            "skills_dense_recall",
            "ext_dense_ndcg",
            "ext_dense_recall",
            "route_tags_f1",
        ] {
            assert!(
                v.get(key).is_some(),
                "datapoint is missing {key}, so the series cannot trend it"
            );
        }
    }

    // The charts are replaced wholesale rather than appended, and must arrive
    // byte-identical: an SVG mangled in transit renders as a broken picture on
    // the branch, which is the artifact people actually look at.
    for chart in ["tool-routing.svg", "skill-retrieval.svg"] {
        let out = git
            .run_raw(&["show", &format!("bench-results:{chart}")])
            .await
            .unwrap();
        let on_branch = out.stdout;
        let on_disk = std::fs::read(staging.join(chart)).unwrap();
        assert_eq!(
            on_branch.len(),
            on_disk.len(),
            "{chart} changed size on the way to the branch"
        );
        assert_eq!(on_branch, on_disk, "{chart} is not byte-identical");
        let text = String::from_utf8_lossy(&on_branch);
        assert!(text.contains("<svg"), "{chart} must still be an SVG");
    }

    // The staging directory is generator output, not a commit, so nothing from
    // the host repository may ride along.
    let files = git.tree_files("bench-results").await.unwrap();
    assert!(
        !files.iter().any(|f| f == "f.txt"),
        "trunk content must not appear on the results branch: {files:?}"
    );
}
