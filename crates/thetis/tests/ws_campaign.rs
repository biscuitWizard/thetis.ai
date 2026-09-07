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
            "system":"d20","themes":["mystery","exploration"],"tone":"Noir"
        }),
    )
    .await;
    wait_state(&mut socket, &campaign, "building").await;
    wait_for(&mut socket, "architect turn", |f| {
        (f["type"] == "play-turn-done").then_some(())
    })
    .await;

    send(
        &mut socket,
        json!({"type":"play-state","campaign":campaign}),
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
    let initial = wait_for(&mut socket, "settled player combat turn", |f| {
        let state = &f["state"];
        let c = &state["combat"];
        let current = c["turn_index"]
            .as_u64()
            .and_then(|i| c["order"].get(i as usize));
        (f["type"] == "play-state"
            && f["campaign"] == campaign
            && state["phase"] == "combat"
            && state["busy"] == false
            && current == Some(&json!("pc")))
        .then(|| state.clone())
    })
    .await;
    let actors = initial["combat"]["actors"].as_array().unwrap();
    let pc = actors.iter().find(|a| a["id"] == "pc").unwrap()["position_m"]
        .as_i64()
        .unwrap();
    let foe = actors.iter().find(|a| a["id"] == "sentinel").unwrap()["position_m"]
        .as_i64()
        .unwrap();
    let direction = if foe >= pc { 1 } else { -1 };
    let destination = if (foe - pc).abs() > 2 {
        foe - direction * 2
    } else {
        pc + direction
    };
    send(&mut socket,json!({"type":"play-combat-act","campaign":campaign,"actor":"pc","action":"move","position_m":destination})).await;
    wait_for(&mut socket, "resolved movement", |f| {
        let player = f["state"]["combat"]["actors"]
            .as_array()
            .and_then(|a| a.iter().find(|a| a["id"] == "pc"));
        (f["type"] == "play-state"
            && f["campaign"] == campaign
            && f["state"]["busy"] == false
            && player.is_some_and(|a| a["position_m"] == destination))
        .then_some(())
    })
    .await;
    let mut attacks = 0;
    for _ in 0..60 {
        send(
            &mut socket,
            json!({"type":"play-state","campaign":campaign}),
        )
        .await;
        let state = wait_for(&mut socket, "tactical state", |f| {
            (f["type"] == "play-state" && f["campaign"] == campaign && f["state"]["busy"] == false)
                .then(|| f["state"].clone())
        })
        .await;
        if state["combat"]["outcome"] == "victory" {
            assert_eq!(state["phase"], "narrative");
            break;
        }
        assert_eq!(state["phase"], "combat");
        let player = state["combat"]["actors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == "pc")
            .unwrap();
        assert_eq!(player["position_m"], destination);
        let event_count = state["combat"]["events"].as_array().unwrap().len();
        let action = if state["combat"]["economy"]["action_units_spent"]
            .as_u64()
            .unwrap_or(0)
            >= 2
        {
            "wait"
        } else {
            attacks += 1;
            "attack"
        };
        send(&mut socket,json!({"type":"play-combat-act","campaign":campaign,"actor":"pc","action":action,"target":"sentinel"})).await;
        wait_for(&mut socket, "combat action completion", |f| {
            let local = f["type"] == "play-state"
                && f["state"]["busy"] == false
                && f["state"]["combat"]["events"]
                    .as_array()
                    .is_some_and(|e| e.len() > event_count);
            (local || f["type"] == "play-turn-done").then_some(())
        })
        .await;
    }
    assert!(attacks > 0, "player must attack");
    send(
        &mut socket,
        json!({"type":"play-state","campaign":campaign}),
    )
    .await;
    wait_for(&mut socket, "combat victory", |f| {
        (f["type"] == "play-state" && f["state"]["combat"]["outcome"] == "victory").then_some(())
    })
    .await;

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
