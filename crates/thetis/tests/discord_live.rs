//! Live Discord checks, driving the real `api::Rest` used in production.
//!
//! Ignored by default: these need a token and reach the network. Run with
//!   DISCORD_BOT_TOKEN=... DISCORD_TEST_CHANNEL=... \
//!     cargo test -p thetis --test discord_live -- --ignored --nocapture
//!
//! The point of going through `Rest` rather than curl is that a hand-written
//! request proves nothing about what the connector actually sends.

use std::time::Duration;

use thetis::discord::api::Rest;

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn setup() -> Option<(Rest, String)> {
    let token = env("DISCORD_BOT_TOKEN")?;
    let channel = env("DISCORD_TEST_CHANNEL")?;
    Some((Rest::new(token, Duration::from_secs(30)).ok()?, channel))
}

/// The one that matters: the bot holds MENTION_EVERYONE in the test guild, so
/// if `allowed_mentions` is wrong this pings every member. Discord reports what
/// it actually parsed in `mention_everyone`, so we can assert on its verdict
/// rather than on our own request body.
#[tokio::test]
#[ignore]
async fn an_everyone_ping_in_model_output_is_defanged() {
    let Some((rest, channel)) = setup() else {
        eprintln!("skipped: set DISCORD_BOT_TOKEN and DISCORD_TEST_CHANNEL");
        return;
    };

    let id = rest
        .send_message(&channel, "@everyone @here mention safety check")
        .await
        .expect("send should succeed");

    let msg = rest
        .get_message(&channel, &id)
        .await
        .expect("should be able to read the message back");

    let everyone = msg
        .get("mention_everyone")
        .and_then(|v| v.as_bool())
        .expect("Discord always reports mention_everyone");

    assert!(
        !everyone,
        "the guild was pinged: allowed_mentions failed to suppress @everyone"
    );

    let roles = msg
        .get("mention_roles")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(roles, 0, "a role was pinged");

    rest.delete_message(&channel, &id).await.ok();
}

/// A reply longer than Discord's 2000 character limit must not be rejected.
/// `truncate` keeps the tail, so the assertion is that the send succeeds and
/// the stored content is within the limit.
#[tokio::test]
#[ignore]
async fn an_overlong_reply_is_truncated_rather_than_rejected() {
    let Some((rest, channel)) = setup() else {
        eprintln!("skipped: set DISCORD_BOT_TOKEN and DISCORD_TEST_CHANNEL");
        return;
    };

    let long = "x".repeat(5000);
    let id = rest
        .send_message(&channel, &long)
        .await
        .expect("an overlong message should be truncated, not rejected");

    let msg = rest.get_message(&channel, &id).await.expect("read back");
    let len = msg
        .get("content")
        .and_then(|v| v.as_str())
        .map(|s| s.chars().count())
        .unwrap_or(0);

    assert!(len <= 2000, "content was {len} characters, over the limit");
    rest.delete_message(&channel, &id).await.ok();
}

/// Streaming works by editing one message repeatedly. This is the call the
/// stream loop makes on every tick, so if edits fail every reply would appear
/// frozen at its first fragment.
#[tokio::test]
#[ignore]
async fn a_message_can_be_edited_the_way_streaming_does() {
    let Some((rest, channel)) = setup() else {
        eprintln!("skipped: set DISCORD_BOT_TOKEN and DISCORD_TEST_CHANNEL");
        return;
    };

    let id = rest.send_message(&channel, "first").await.expect("send");
    rest.edit_message(&channel, &id, "first second")
        .await
        .expect("edit should succeed");

    let msg = rest.get_message(&channel, &id).await.expect("read back");
    assert_eq!(
        msg.get("content").and_then(|v| v.as_str()),
        Some("first second"),
        "the edit did not take effect"
    );

    rest.delete_message(&channel, &id).await.ok();
}

/// The typing indicator is refreshed during long turns; a failure here would
/// make the bot look dead while it works.
#[tokio::test]
#[ignore]
async fn the_typing_indicator_is_accepted() {
    let Some((rest, channel)) = setup() else {
        eprintln!("skipped: set DISCORD_BOT_TOKEN and DISCORD_TEST_CHANNEL");
        return;
    };
    rest.typing(&channel).await.expect("typing should succeed");
}
