//! Live end-to-end check of the sandbox-branch workflow over the websocket.
//!
//! Ignored by default: it needs a running gateway (mock LLM behind it) whose
//! root it can also touch directly, so it must only ever run against a
//! scratch instance:
//!   THETIS_WS_URL=ws://127.0.0.1:7797/ws \
//!   THETIS_SMOKE_ROOT=/path/to/scratch/thetis-smoke \
//!     cargo test -p thetis --test ws_branch -- --ignored --nocapture
//!
//! The arc it proves: a conversation materializes its own branch at first
//! message; work in the branch becomes commits; the branch squashes to one
//! commit and merges to trunk by fast-forward; a conflicting trunk makes an
//! update stop on conflicts that
//! an abort cleans up; and reset restores an earlier state. All of it driven
//! through the same frames the browser sends.

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

struct Env {
    url: String,
    root: std::path::PathBuf,
}

fn env() -> Option<Env> {
    let url = std::env::var("THETIS_WS_URL").ok().filter(|v| !v.is_empty())?;
    let root = std::env::var("THETIS_SMOKE_ROOT").ok().filter(|v| !v.is_empty())?;
    Some(Env {
        url,
        root: root.into(),
    })
}

#[tokio::test]
#[ignore]
async fn a_conversation_branches_works_merges_and_survives_conflicts() {
    let Some(env) = env() else {
        eprintln!("skipped: set THETIS_WS_URL and THETIS_SMOKE_ROOT");
        return;
    };

    let (mut socket, _) = tokio_tungstenite::connect_async(&env.url)
        .await
        .expect("connecting to the gateway websocket");

    // A fresh conversation has no branch yet.
    socket
        .send(Message::Text(r#"{"type":"new","title":"ws-branch smoke"}"#.into()))
        .await
        .unwrap();
    let session = wait_for(&mut socket, "a session id", 30, |f| {
        (f["type"] == "opened").then(|| f["session"].as_str().unwrap_or("").to_string())
    })
    .await;

    send(&mut socket, json!({ "type": "branch-status", "id": session })).await;
    let unmaterialized = wait_for(&mut socket, "pre-branch status", 30, |f| {
        (f["type"] == "branch-status").then(|| f["materialized"].as_bool().unwrap_or(true))
    })
    .await;
    assert!(!unmaterialized, "no branch before the first message");

    // The graph is present before the branch exists: trunk rail only.
    send(&mut socket, json!({ "type": "branch-graph", "id": session })).await;
    let graph = wait_for(&mut socket, "the pre-branch graph", 30, |f| {
        (f["type"] == "branch-graph").then(|| f.clone())
    })
    .await;
    assert!(graph["branch_name"].is_null(), "no branch lane yet");
    assert!(
        graph["trunk"].as_array().map(Vec::len).unwrap_or(0) > 0,
        "trunk commits populate the graph"
    );

    // The starting-point picker's data source works.
    send(&mut socket, json!({ "type": "branch-trunk-log", "limit": 5 })).await;
    let trunk_commits = wait_for(&mut socket, "trunk log", 30, |f| {
        (f["type"] == "branch-trunk-log").then(|| f["commits"].as_array().map(Vec::len).unwrap_or(0))
    })
    .await;
    assert!(trunk_commits > 0, "trunk has history to offer");

    // First message: branch, worktree, and worker materialize.
    send(
        &mut socket,
        json!({ "type": "send", "id": session, "text": "hello sandbox" }),
    )
    .await;
    wait_for(&mut socket, "first turn to finish", 900, |f| {
        (f["type"] == "event" && f["kind"] == "turn-finished").then_some(())
    })
    .await;

    send(&mut socket, json!({ "type": "branch-status", "id": session })).await;
    let status = wait_for(&mut socket, "materialized status", 60, |f| {
        (f["type"] == "branch-status" && f["materialized"] == true).then(|| f.clone())
    })
    .await;
    let branch = status["branch"].as_str().unwrap_or_default().to_string();
    assert!(branch.starts_with("conv/"), "branch is {branch}");

    // The graph now has the branch lane, forked at a real base.
    send(&mut socket, json!({ "type": "branch-graph", "id": session })).await;
    let graph = wait_for(&mut socket, "the materialized graph", 30, |f| {
        (f["type"] == "branch-graph" && !f["branch_name"].is_null()).then(|| f.clone())
    })
    .await;
    assert_eq!(graph["branch_name"].as_str().unwrap_or(""), branch);
    assert!(graph["base"].as_str().is_some(), "the fork point is named");

    // Work lands in the branch: put a skill file into the worktree and let
    // the turn-end checkpoint commit it.
    let worktree = env.root.join("worktrees").join(branch.replace('/', "-"));
    assert!(worktree.is_dir(), "worktree exists at {}", worktree.display());
    let skill_dir = worktree.join("skills").join("smoke-note");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname = \"Smoke note\"\nbrief = \"A note from the smoke test.\"\nwhen_to_use = \"Never.\"\n---\n\nBody.\n",
    )
    .unwrap();

    send(
        &mut socket,
        json!({ "type": "send", "id": session, "text": "carry on" }),
    )
    .await;
    wait_for(&mut socket, "second turn to finish", 120, |f| {
        (f["type"] == "event" && f["kind"] == "turn-finished").then_some(())
    })
    .await;

    send(&mut socket, json!({ "type": "branch-status", "id": session })).await;
    let ahead = wait_for(&mut socket, "the branch to be ahead", 60, |f| {
        (f["type"] == "branch-status" && f["ahead"].as_u64().unwrap_or(0) > 0)
            .then(|| f["ahead"].as_u64().unwrap())
    })
    .await;
    assert!(ahead >= 1, "the checkpoint committed the skill");

    // Merge to trunk: fast-forward, then the branch is in step again.
    send(&mut socket, json!({ "type": "branch-merge", "id": session })).await;
    let merged = wait_for(&mut socket, "the merge result", 120, |f| {
        (f["type"] == "branch-result" && f["op"] == "merge").then(|| f["ok"].as_bool().unwrap_or(false))
    })
    .await;
    assert!(merged, "the merge fast-forwarded");
    assert!(
        env.root.join("skills/smoke-note/SKILL.md").is_file(),
        "trunk's checkout now holds the branch's skill"
    );

    // The merge is squashed: the branch's turn checkpoints do not reach trunk,
    // and it is left exactly one commit ahead of where it forked.
    send(&mut socket, json!({ "type": "branch-status", "id": session })).await;
    let after_merge = wait_for(&mut socket, "post-merge status", 60, |f| {
        (f["type"] == "branch-status" && f["materialized"] == true).then(|| f.clone())
    })
    .await;
    assert_eq!(
        after_merge["ahead"].as_u64().unwrap_or(1),
        0,
        "the branch is in step with trunk after merging"
    );
    let trunk_subjects = trunk_log(&env.root).await;
    assert!(
        !trunk_subjects.iter().any(|s| s.starts_with("checkpoint:")),
        "no branch checkpoints reached trunk: {trunk_subjects:?}"
    );

    // Conflict: trunk and branch both edit the same file.
    let subject = "smoke: conflicting trunk change";
    std::fs::write(
        env.root.join("skills/smoke-note/SKILL.md"),
        "---\nname = \"Smoke note\"\nbrief = \"Trunk's version.\"\nwhen_to_use = \"Never.\"\n---\n\nTrunk body.\n",
    )
    .unwrap();
    git(&env.root, &["add", "-A"]).await;
    git(&env.root, &["commit", "-m", subject]).await;

    std::fs::write(
        worktree.join("skills/smoke-note/SKILL.md"),
        "---\nname = \"Smoke note\"\nbrief = \"The branch's version.\"\nwhen_to_use = \"Never.\"\n---\n\nBranch body.\n",
    )
    .unwrap();
    send(
        &mut socket,
        json!({ "type": "send", "id": session, "text": "checkpoint please" }),
    )
    .await;
    wait_for(&mut socket, "third turn to finish", 120, |f| {
        (f["type"] == "event" && f["kind"] == "turn-finished").then_some(())
    })
    .await;

    send(&mut socket, json!({ "type": "branch-update", "id": session })).await;
    let conflicts = wait_for(&mut socket, "the conflicted update", 120, |f| {
        (f["type"] == "branch-result" && f["op"] == "update")
            .then(|| f["conflicts"].as_array().map(Vec::len).unwrap_or(0))
    })
    .await;
    assert!(conflicts >= 1, "the update reported its conflicts");

    let text = std::fs::read_to_string(worktree.join("skills/smoke-note/SKILL.md")).unwrap();
    assert!(text.contains("<<<<<<<"), "markers are in the working tree");

    // Abort restores the pre-merge branch.
    send(&mut socket, json!({ "type": "branch-abort", "id": session })).await;
    wait_for(&mut socket, "the abort result", 60, |f| {
        (f["type"] == "branch-result" && f["op"] == "abort").then(|| assert!(f["ok"].as_bool().unwrap()))
    })
    .await;
    let text = std::fs::read_to_string(worktree.join("skills/smoke-note/SKILL.md")).unwrap();
    assert!(text.contains("The branch's version"), "abort restored the branch side");

    // Reset restores an earlier state, as a new commit.
    send(&mut socket, json!({ "type": "branch-log", "id": session })).await;
    let commits = wait_for(&mut socket, "the branch log", 60, |f| {
        (f["type"] == "branch-log").then(|| f["commits"].as_array().cloned().unwrap_or_default())
    })
    .await;
    assert!(commits.len() >= 2, "history has depth: {}", commits.len());
    let earlier = commits[1]["rev"].as_str().unwrap().to_string();
    send(
        &mut socket,
        json!({ "type": "branch-reset", "id": session, "rev": earlier }),
    )
    .await;
    wait_for(&mut socket, "the reset result", 120, |f| {
        (f["type"] == "branch-result" && f["op"] == "reset").then(|| assert!(f["ok"].as_bool().unwrap()))
    })
    .await;
}

async fn send(
    socket: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    frame: Value,
) {
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

/// Trunk's recent commit subjects, read straight from the root checkout.
async fn trunk_log(dir: &std::path::Path) -> Vec<String> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["log", "--format=%s", "-n", "20", "main"])
        .output()
        .await
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

async fn git(dir: &std::path::Path, args: &[&str]) {
    let status = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .await
        .unwrap();
    assert!(status.success(), "git {args:?} in {}", dir.display());
}

async fn wait_for<T>(
    socket: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
              + SinkExt<Message>
              + Unpin),
    what: &str,
    secs: u64,
    pick: impl Fn(&Value) -> Option<T>,
) -> T {
    let step = async {
        loop {
            let Some(Ok(message)) = socket.next().await else {
                panic!("socket closed while waiting for {what}");
            };
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            // Every frame, for post-mortems under --nocapture. Deltas are
            // too chatty to be useful.
            if frame["kind"] != "delta" {
                eprintln!("<- {frame}");
            }
            if let Some(found) = pick(&frame) {
                return found;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(secs), step)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}
