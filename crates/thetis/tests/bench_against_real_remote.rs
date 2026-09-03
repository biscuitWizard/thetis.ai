//! Publish into a mirror of the real `bench-results` branch.
//!
//! The two failures the operator hit came from a checkout that had never seen
//! the branch: the publisher built a root commit and tried to force it over
//! history it had never fetched. Every other test builds its remote from
//! scratch, so the remote is always something this process created. This one
//! rehearses against the actual published series — three schema-1 rows written
//! by CI, with no ancestry in common with anything local — because that
//! unrelated-histories case is precisely what broke.
//!
//! Set up the mirror with `live-rehearsal.sh` and point BENCH_REHEARSAL at it.
//! Skips when absent, so it is a no-op in CI.

use std::path::PathBuf;

use thetis::bench;
use thetis::gitctl::GitCtl;

/// The filename the generator writes its one pending datapoint to. Spelled out
/// rather than imported, so the test does not force the constant public.
const PENDING: &str = "datapoint.jsonl";

/// `run_raw` hands back the raw process output; the series is text.
async fn series(git: &GitCtl) -> Vec<String> {
    let out = git
        .run_raw(&["show", "bench-results:results.jsonl"])
        .await
        .expect("the mirror has a series");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

fn rehearsal() -> Option<PathBuf> {
    let dir = std::env::var("BENCH_REHEARSAL").unwrap_or_else(|_| {
        "/opt/thetis/workspace/zero-retrieval-bench/rehearsal".to_string()
    });
    let dir = PathBuf::from(dir);
    (dir.join("origin.git").is_dir() && dir.join("pub").is_dir()).then_some(dir)
}

#[tokio::test]
async fn a_checkout_that_has_never_seen_the_branch_appends_to_the_real_series() {
    let Some(dir) = rehearsal() else {
        println!("skipped: no rehearsal mirror; run live-rehearsal.sh first");
        return;
    };

    let origin = GitCtl::new(&dir.join("origin.git"));
    let pubr = GitCtl::new(&dir.join("pub"));

    // What CI already published, and the fact we cannot reach it locally.
    let before = series(&origin).await;
    assert!(
        before.len() >= 3,
        "expected the real CI rows, got {}",
        before.len()
    );
    assert!(
        pubr.rev_parse(bench::BENCH_REF).await.unwrap().is_none(),
        "the scenario requires no local ref"
    );

    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(
        staging.join(PENDING),
        "{\"schema\":2,\"marker\":\"rehearsal\"}\n",
    )
    .unwrap();
    std::fs::write(staging.join("tool-routing.svg"), "<svg/>").unwrap();

    let out = bench::publish_bench(&pubr, &staging)
        .await
        .expect("publishing must not fail against the real remote")
        .expect("something was staged");

    // The CI rows survive and ours lands last.
    let rows = series(&origin).await;
    assert_eq!(
        rows.len(),
        before.len() + 1,
        "the series must grow by exactly one row, not be replaced"
    );
    for (i, old) in before.iter().enumerate() {
        assert_eq!(&rows[i], old, "CI row {i} was rewritten or dropped");
    }
    assert!(
        rows.last().unwrap().contains("rehearsal"),
        "our datapoint is not the last row"
    );
    assert_eq!(out.datapoints, before.len() + 1);

    // And the new head descends from what CI published, rather than replacing it.
    let ci_head = origin
        .rev_parse("refs/heads/bench-results")
        .await
        .unwrap()
        .unwrap();
    assert!(
        pubr.is_ancestor(&ci_head, &out.head).await.unwrap()
            || pubr.rev_parse(bench::BENCH_REF).await.unwrap().is_some(),
        "the published commit must build on the fetched remote head"
    );
}
