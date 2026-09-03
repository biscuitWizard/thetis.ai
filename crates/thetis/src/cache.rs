//! Prompt caching.
//!
//! Providers differ in kind, not just in syntax, so this decides per vendor:
//!
//! * **Anthropic** caches only where you mark a breakpoint. Nothing is cached
//!   without one, so the marks below are what make repeat turns cheap.
//! * **OpenAI** caches long prefixes automatically. Marking is unnecessary, and
//!   the only thing that matters is that the prefix stays byte-identical
//!   between turns.
//! * **Google** caches implicitly on recent models. Explicit breakpoints are
//!   accepted but bill a write at full input price plus storage, and only the
//!   last one counts — so marking a prefix that moves every turn costs more
//!   than it saves. Left implicit by default.
//!
//! The shape of an Anthropic-cached request is `tools -> system -> messages`,
//! each level invalidating the ones after it. A breakpoint writes one entry
//! covering everything up to and including that block; a later request hashes
//! its own prefix at each breakpoint and walks back **at most twenty blocks**
//! looking for a match. That window is the whole reason for the anchors below:
//! a turn that runs a dozen tools can add more than twenty blocks at once, and
//! a single breakpoint at the end would sail straight past the previous entry
//! and re-read the entire conversation at full price.

use serde_json::{json, Value};

use crate::config::{CacheSettings, CacheStrategy};

/// Anthropic accepts no more than this many, and errors above it.
const MAX_BREAKPOINTS: usize = 4;

/// Which provider a model id belongs to.
///
/// OpenRouter ids are `vendor/model`; a bare id is assumed to be whatever the
/// configured default vendor is, since that is how a direct endpoint looks.
pub fn vendor_of(model: &str) -> &str {
    model.split('/').next().unwrap_or("").trim()
}

/// Applies the configured caching strategy to a request body in place.
///
/// Returns the number of breakpoints written, which is zero whenever the
/// provider caches on its own.
pub fn apply(body: &mut Value, model: &str, cfg: &CacheSettings) -> usize {
    if !cfg.enabled {
        return 0;
    }
    if cfg.strategy_for(vendor_of(model)) != CacheStrategy::Breakpoints {
        return 0;
    }

    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    if messages.is_empty() {
        return 0;
    }

    let mut marked = 0;
    for index in breakpoint_positions(messages, cfg.anchor_stride) {
        if marked >= MAX_BREAKPOINTS {
            break;
        }
        if mark(&mut messages[index], &cfg.ttl) {
            marked += 1;
        }
    }
    marked
}

/// Which messages to mark, in prefix order.
///
/// Three kinds of position, and the reasoning for each:
///
/// * the **last system message**, which sits behind the tools and never
///   changes within a conversation — the cheapest and most reliable hit;
/// * **anchors** on a fixed stride, which stay put for several turns and so
///   remain valid targets when a turn adds more than the twenty-block lookback;
/// * the **final message**, which writes the newest prefix so the next turn can
///   read almost all of it back.
fn breakpoint_positions(messages: &[Value], stride: usize) -> Vec<usize> {
    let last = messages.len() - 1;
    let mut positions = Vec::new();

    if let Some(system) = messages
        .iter()
        .rposition(|m| m.get("role").and_then(Value::as_str) == Some("system"))
    {
        positions.push(system);
    }

    // Anchors land on multiples of the stride, so they hold still while the
    // conversation grows around them. Two of them keep a warm entry available
    // even when the newest one has just moved.
    if stride > 0 {
        let mut anchor = last / stride * stride;
        let mut taken = 0;
        while taken < 2 && anchor > 0 {
            if anchor < last {
                positions.push(anchor);
                taken += 1;
            }
            anchor = anchor.saturating_sub(stride);
        }
    }

    positions.push(last);

    // In prefix order, without duplicates: a breakpoint is only meaningful once
    // per position, and Anthropic counts repeats against the limit.
    positions.sort_unstable();
    positions.dedup();

    // Keep the newest when there are too many: the older a prefix is, the more
    // likely it is still covered by an entry the lookback can reach anyway.
    if positions.len() > MAX_BREAKPOINTS {
        positions.drain(..positions.len() - MAX_BREAKPOINTS);
    }
    positions
}

/// Attaches a breakpoint to a message, converting its content to blocks if it
/// is still a plain string.
///
/// Returns whether anything was marked: an empty or unrecognisable message is
/// skipped rather than being turned into something the provider will reject.
fn mark(message: &mut Value, ttl: &str) -> bool {
    let control = if ttl.is_empty() || ttl == "5m" {
        json!({ "type": "ephemeral" })
    } else {
        json!({ "type": "ephemeral", "ttl": ttl })
    };

    let Some(content) = message.get_mut("content") else {
        return false;
    };

    match content {
        Value::String(text) => {
            if text.trim().is_empty() {
                return false;
            }
            *content = json!([{ "type": "text", "text": text, "cache_control": control }]);
            true
        }
        Value::Array(blocks) => {
            // The mark belongs on the last block, so the entry covers the whole
            // message rather than part of it.
            match blocks.last_mut().and_then(Value::as_object_mut) {
                Some(block) => {
                    block.insert("cache_control".into(), control);
                    true
                }
                None => false,
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn settings() -> CacheSettings {
        Config::load().unwrap().cache
    }

    fn body(roles: &[&str]) -> Value {
        json!({
            "model": "anthropic/claude-sonnet-4.5",
            "messages": roles
                .iter()
                .map(|r| json!({ "role": r, "content": format!("{r} content") }))
                .collect::<Vec<_>>(),
        })
    }

    /// Indices of every message carrying a breakpoint.
    fn marked(body: &Value) -> Vec<usize> {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|b| b.get("cache_control").is_some()))
            })
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn vendors_come_from_the_model_id() {
        assert_eq!(vendor_of("anthropic/claude-sonnet-4.5"), "anthropic");
        assert_eq!(vendor_of("openai/gpt-4o"), "openai");
        assert_eq!(vendor_of("google/gemini-2.5-pro"), "google");
        assert_eq!(vendor_of("mock/echo"), "mock");
    }

    #[test]
    fn anthropic_gets_the_system_prompt_and_the_latest_message() {
        let mut b = body(&["system", "user", "assistant", "user"]);
        let n = apply(&mut b, "anthropic/claude-sonnet-4.5", &settings());

        assert!(n >= 2);
        let positions = marked(&b);
        assert!(positions.contains(&0), "the system prompt anchors the prefix");
        assert!(positions.contains(&3), "the newest message writes the tail");
    }

    #[test]
    fn a_provider_that_caches_on_its_own_is_left_alone() {
        for model in ["openai/gpt-4o", "google/gemini-2.5-pro"] {
            let mut b = body(&["system", "user"]);
            assert_eq!(apply(&mut b, model, &settings()), 0, "{model}");
            // Content is untouched, so an automatic cache still sees the same
            // bytes it saw last turn.
            assert!(b["messages"][0]["content"].is_string(), "{model}");
        }
    }

    #[test]
    fn caching_can_be_turned_off_entirely() {
        let mut cfg = settings();
        cfg.enabled = false;
        let mut b = body(&["system", "user"]);
        assert_eq!(apply(&mut b, "anthropic/claude-sonnet-4.5", &cfg), 0);
    }

    #[test]
    fn never_exceeds_the_four_breakpoints_anthropic_allows() {
        let mut cfg = settings();
        cfg.anchor_stride = 2;

        let mut roles = vec!["system"];
        roles.extend(std::iter::repeat("user").take(60));
        let mut b = body(&roles);

        let n = apply(&mut b, "anthropic/claude-sonnet-4.5", &cfg);
        assert!(n <= 4, "wrote {n}");
        assert!(marked(&b).len() <= 4);
    }

    #[test]
    fn anchors_hold_still_while_the_conversation_grows() {
        let mut cfg = settings();
        cfg.anchor_stride = 8;

        // Two consecutive turns should share an anchor, or every turn would pay
        // to write a prefix nothing reads back.
        let mut first = body(&vec!["user"; 20]);
        let mut second = body(&vec!["user"; 21]);
        apply(&mut first, "anthropic/claude-sonnet-4.5", &cfg);
        apply(&mut second, "anthropic/claude-sonnet-4.5", &cfg);

        let shared: Vec<usize> = marked(&first)
            .into_iter()
            .filter(|i| marked(&second).contains(i))
            .collect();
        assert!(!shared.is_empty(), "no anchor survived the turn");
    }

    #[test]
    fn a_string_message_becomes_a_block_carrying_the_mark() {
        let mut m = json!({ "role": "system", "content": "be helpful" });
        assert!(mark(&mut m, "5m"));

        assert_eq!(m["content"][0]["type"], "text");
        assert_eq!(m["content"][0]["text"], "be helpful");
        assert_eq!(m["content"][0]["cache_control"]["type"], "ephemeral");
        // The default TTL is implied, not spelled out.
        assert!(m["content"][0]["cache_control"].get("ttl").is_none());
    }

    #[test]
    fn a_longer_ttl_is_named_explicitly() {
        let mut m = json!({ "role": "system", "content": "be helpful" });
        mark(&mut m, "1h");
        assert_eq!(m["content"][0]["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn multi_part_content_is_marked_on_its_last_block() {
        // An image message is already in block form; the mark has to land at
        // the end so the entry covers the whole message.
        let mut m = json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "what is this" },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAA" } }
            ]
        });
        assert!(mark(&mut m, "5m"));

        assert!(m["content"][0].get("cache_control").is_none());
        assert!(m["content"][1].get("cache_control").is_some());
    }

    #[test]
    fn an_empty_message_is_not_marked() {
        let mut m = json!({ "role": "assistant", "content": "" });
        assert!(!mark(&mut m, "5m"));
        assert!(m["content"].is_string(), "left exactly as it was");
    }

    #[test]
    fn a_request_without_messages_is_left_alone() {
        let mut b = json!({ "model": "anthropic/claude-sonnet-4.5" });
        assert_eq!(apply(&mut b, "anthropic/claude-sonnet-4.5", &settings()), 0);
    }
}
