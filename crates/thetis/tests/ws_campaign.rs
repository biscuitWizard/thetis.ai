//! End-to-end campaign mount/protocol verification against a scratch Thetis.
//!
//! The test is ignored because it needs the helper-managed scratch orchestrator:
//!
//! ```text
//! scripts/run-campaign-walkthrough.sh --protocol
//! ```

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
#[ignore]
async fn campaign_protocol_walkthrough() {
    let url = std::env::var("THETIS_CAMPAIGN_WS_URL").expect(
        "THETIS_CAMPAIGN_WS_URL is required; use scripts/run-campaign-walkthrough.sh --protocol",
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect to campaign websocket");

    send(&mut socket, json!({"type":"play-hello"})).await;
    wait_for(&mut socket, "campaign list", |f| {
        (f["type"] == "play-campaigns").then_some(())
    })
    .await;

    send(
        &mut socket,
        json!({"type":"play-new","title":"Protocol Lanterns","system":"d20"}),
    )
    .await;
    let campaign = wait_for(&mut socket, "setup", |f| {
        (f["type"] == "play-state" && f["state"]["phase"] == "setup")
            .then(|| f["campaign"].as_str().unwrap().to_owned())
    })
    .await;

    send(
        &mut socket,
        json!({
            "type":"play-setup-submit","campaign":campaign,"title":"Protocol Lanterns",
            "system":"d20","premise":"A lantern-lit ruin beneath a quiet town."
        }),
    )
    .await;
    wait_state(&mut socket, &campaign, "building").await;
    wait_for(&mut socket, "architect turn", |f| {
        (f["type"] == "play-turn-done").then_some(())
    })
    .await;

    // Build completion is explicitly accepted here. The mock creates the location
    // needed by the scene; exhaustive architect quality belongs to rules tests.
    send(
        &mut socket,
        json!({"type":"play-build-continue","campaign":campaign,"complete":true}),
    )
    .await;
    wait_state(&mut socket, &campaign, "chargen").await;

    send(
        &mut socket,
        json!({"type":"play-chargen-save","campaign":campaign,"sheet":d20_sheet()}),
    )
    .await;
    wait_state(&mut socket, &campaign, "narrative").await;
    wait_for(&mut socket, "opening scene", |f| {
        (f["type"] == "play-scene").then_some(())
    })
    .await;
    wait_for(&mut socket, "opening turn", |f| {
        (f["type"] == "play-turn-done").then_some(())
    })
    .await;

    send(
        &mut socket,
        json!({"type":"play-regenerate","campaign":campaign}),
    )
    .await;
    wait_for(&mut socket, "regenerated turn", |f| {
        (f["type"] == "play-turn-done").then_some(())
    })
    .await;

    send(
        &mut socket,
        json!({"type":"play-act","campaign":campaign,"text":"Start combat with the sentinel."}),
    )
    .await;
    wait_for(&mut socket, "combat start", |f| {
        (f["type"] == "play-phase-hint" && f["tool"] == "rpg-combat-start").then_some(())
    })
    .await;
    wait_for(&mut socket, "combat turn", |f| {
        (f["type"] == "play-turn-done").then_some(())
    })
    .await;
    send(
        &mut socket,
        json!({"type":"play-state","campaign":campaign}),
    )
    .await;
    wait_state(&mut socket, &campaign, "combat").await;
    send(&mut socket, json!({"type":"play-combat-act","campaign":campaign,"actor":"pc","action":"attack","target":"sentinel"})).await;
    wait_for(&mut socket, "resolved combat", |f| {
        (f["type"] == "play-turn-done").then_some(())
    })
    .await;
    send(
        &mut socket,
        json!({"type":"play-state","campaign":campaign}),
    )
    .await;
    wait_state(&mut socket, &campaign, "narrative").await;

    send(
        &mut socket,
        json!({"type":"play-act","campaign":campaign,"text":"Visit the lantern shop."}),
    )
    .await;
    wait_for(&mut socket, "shop", |f| {
        (f["type"] == "play-phase-hint" && f["tool"] == "rpg-shop-open").then_some(())
    })
    .await;
    wait_for(&mut socket, "shop turn", |f| {
        (f["type"] == "play-turn-done").then_some(())
    })
    .await;

    send(
        &mut socket,
        json!({"type":"play-state","campaign":campaign}),
    )
    .await;
    wait_state(&mut socket, &campaign, "shop").await;
    send(
        &mut socket,
        json!({"type":"play-export","campaign":campaign}),
    )
    .await;
    let save_path = wait_for(&mut socket, "export", |f| {
        (f["type"] == "play-export").then(|| {
            f["url"]
                .as_str()
                .unwrap()
                .trim_start_matches("/workspace/file/")
                .to_owned()
        })
    })
    .await;
    assert!(save_path.ends_with(".json"));

    send(&mut socket, json!({"type":"play-import","path":save_path})).await;
    let imported = wait_for(&mut socket, "imported campaign", |f| {
        (f["type"] == "play-state" && f["campaign"] != campaign)
            .then(|| f["campaign"].as_str().unwrap().to_owned())
    })
    .await;
    assert_ne!(imported, campaign);
    eprintln!("ok setup -> build -> chargen -> scene -> regenerate -> combat -> shop -> export -> import ({campaign} -> {imported})");
}

fn d20_sheet() -> Value {
    json!({
        "v":1,"id":"pc","name":"Wren","system":"d20","role":"adventurer",
        "stats":{"str":15,"dex":14,"con":13,"int":12,"wis":10,"cha":8},
        "skills":{},
        "resources":{"hp":12,"hp_max":12,"humanity":null,"cash":100,"ip":null,"conditions":[]},
        "inventory":[],"equipped":[],"features":[],"lifepath":null,"notes":""
    })
}

async fn wait_state<S>(socket: &mut S, campaign: &str, phase: &str)
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    wait_for(socket, phase, |f| {
        (f["type"] == "play-state" && f["campaign"] == campaign && f["state"]["phase"] == phase)
            .then_some(())
    })
    .await
}

async fn send<S>(socket: &mut S, frame: Value)
where
    S: SinkExt<Message> + Unpin,
    S::Error: std::fmt::Debug,
{
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .expect("send campaign frame");
}

async fn wait_for<T, S>(socket: &mut S, what: &str, pick: impl Fn(&Value) -> Option<T>) -> T
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let read = async {
        loop {
            let Some(Ok(message)) = socket.next().await else {
                panic!("campaign socket closed while waiting for {what}")
            };
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame["type"] == "play-error" || frame["type"] == "error" {
                panic!("gateway error while waiting for {what}: {frame}")
            }
            if let Some(value) = pick(&frame) {
                return value;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(600), read)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}
