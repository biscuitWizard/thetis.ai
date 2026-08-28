//! Live check of kernel-in-branch: a conversation "rebuilds" its kernel,
//! asks for a restart, and comes back on its own binary — no other process
//! involved, verified through the same surfaces a person would use.
//!
//! Ignored by default; scratch instances only (it stands a fake built kernel
//! into the worktree):
//!   THETIS_WS_URL=ws://127.0.0.1:7797/ws \
//!   THETIS_SMOKE_ROOT=... THETIS_ADMIN_URL=http://127.0.0.1:7797/admin \
//!     cargo test -p thetis --test ws_kernel -- --ignored --nocapture

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
#[ignore]
async fn a_branch_built_kernel_is_probed_adopted_and_run() {
    let (Some(url), Some(root), Some(admin)) = (
        std::env::var("THETIS_WS_URL").ok(),
        std::env::var("THETIS_SMOKE_ROOT").ok().map(std::path::PathBuf::from),
        std::env::var("THETIS_ADMIN_URL").ok(),
    ) else {
        eprintln!("skipped: set THETIS_WS_URL, THETIS_SMOKE_ROOT, THETIS_ADMIN_URL");
        return;
    };

    let (mut socket, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connecting");

    socket
        .send(Message::Text(r#"{"type":"new","title":"ws-kernel smoke"}"#.into()))
        .await
        .unwrap();
    let session = wait_for(&mut socket, "a session id", 30, |f| {
        (f["type"] == "opened").then(|| f["session"].as_str().unwrap_or("").to_string())
    })
    .await;

    send(&mut socket, json!({ "type": "send", "id": session, "text": "hello" })).await;
    wait_for(&mut socket, "first turn", 900, |f| {
        (f["type"] == "event" && f["kind"] == "turn-finished").then_some(())
    })
    .await;

    send(&mut socket, json!({ "type": "branch-status", "id": session })).await;
    let branch = wait_for(&mut socket, "branch name", 60, |f| {
        (f["type"] == "branch-status" && f["materialized"] == true)
            .then(|| f["branch"].as_str().unwrap_or("").to_string())
    })
    .await;

    // Stand a "rebuilt kernel" into the worktree, exactly where a real
    // `cargo build --release -p thetis` in that worktree would leave one.
    // This binary IS a thetis build, so the probe genuinely exercises it.
    let worktree = root.join("worktrees").join(branch.replace('/', "-"));
    let release = worktree.join("target/release");
    std::fs::create_dir_all(&release).unwrap();
    let this_kernel = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("thetis");
    std::fs::copy(&this_kernel, release.join("thetis")).expect("staging the kernel");

    // The mock model answers this by calling restart_orchestrator.
    send(
        &mut socket,
        json!({ "type": "send", "id": session, "text": "please restart yourself" }),
    )
    .await;
    wait_for(&mut socket, "the restarting turn to end", 120, |f| {
        (f["type"] == "event" && (f["kind"] == "turn-finished" || f["kind"] == "incident"))
            .then_some(())
    })
    .await;

    // The bounce takes: restart delay, probe, adopt, shutdown, respawn.
    // Prove it landed by asking /admin (a human's view) until the branch
    // shows a non-trunk kernel, then prove the conversation still works.
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut adopted = false;
    while std::time::Instant::now() < deadline {
        let page = reqwest_get(&admin).await;
        if let Some(row) = page
            .lines()
            .find(|l| l.contains(&branch))
        {
            if !row.contains(">trunk<") {
                adopted = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    assert!(adopted, "/admin shows the branch on its own kernel");

    send(
        &mut socket,
        json!({ "type": "send", "id": session, "text": "are you back" }),
    )
    .await;
    let finished = wait_for(&mut socket, "a turn on the new kernel", 300, |f| {
        (f["type"] == "event" && (f["kind"] == "turn-finished" || f["kind"] == "incident"))
            .then(|| f["kind"] == "turn-finished")
    })
    .await;
    assert!(finished, "the conversation runs on the adopted kernel");
}

/// Plain HTTP GET without pulling a client crate into the tests: /admin is
/// same-host and tiny.
async fn reqwest_get(url: &str) -> String {
    let out = tokio::process::Command::new("curl")
        .arg("-s")
        .arg(url)
        .output()
        .await
        .expect("curl");
    String::from_utf8_lossy(&out.stdout).to_string()
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
            if let Some(found) = pick(&frame) {
                return found;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(secs), step)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}
