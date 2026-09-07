//! A stand-in for OpenRouter, for development and for the validation gate.
//!
//! Speaks enough of the streaming chat-completions protocol to exercise the
//! whole loop — token deltas, tool calls, usage accounting — without a network
//! call or a cent of spend.
//!
//! Run it, then point the orchestrator at it:
//!
//! ```text
//! cargo run --bin mock-llm
//! OPENROUTER_API_KEY=test OPENROUTER_BASE_URL=http://127.0.0.1:7788 cargo run --bin thetis
//! ```
//!
//! Behaviour is driven by the last user message so tests can steer it:
//!   - contains "remember"  -> emits a `remember` tool call
//!   - contains "recall"    -> emits a `recall` tool call
//!   - contains "slow"      -> streams with delays, for testing nudges
//!   - anything else        -> streams a plain reply

use axum::response::sse::{Event, Sse};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::stream::Stream;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = std::env::var("MOCK_LLM_BIND").unwrap_or_else(|_| "127.0.0.1:7788".to_string());
    let app = Router::new().route("/chat/completions", post(completions));
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("mock llm listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn completions(Json(body): Json<Value>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let last_user_content = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|msgs| {
            msgs.iter()
                .rev()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        })
        .and_then(|m| m.get("content"))
        .cloned()
        .unwrap_or(Value::Null);

    let (text, images) = read_user_content(&last_user_content);
    let last_user = text.to_lowercase();

    let system_prompt = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|msgs| msgs.iter().find(|m| {
            m.get("role").and_then(Value::as_str) == Some("system")
        }))
        .and_then(|m| m.get("content").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();

    let tool_names: Vec<String> = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| {
                    t.get("function")?.get("name")?.as_str().map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();

    // If the most recent message is a tool result, this call is the follow-up
    // within a turn, so answer normally instead of looping on another call.
    // Older tool results belong to previous turns and must not suppress new ones.
    let already_used_tool = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|msgs| msgs.last())
        .map(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        .unwrap_or(false);

    let script = if already_used_tool {
        Script::Text("Done — I used a tool and here is the result.".into(), false)
    } else if last_user.contains("remember") {
        Script::ToolCall("remember", json!({ "key": "note", "value": "the user said remember" }))
    } else if last_user.contains("recall") {
        Script::ToolCall("recall", json!({}))
    } else if last_user.contains("slow") {
        Script::Text(
            "Working on it, one word at a time so you can interrupt me.".into(),
            true,
        )
    } else if last_user.contains("restart yourself") {
        // resume:false, or the resumed turn would just call restart again.
        Script::ToolCall(
            "restart_orchestrator",
            json!({ "reason": "the smoke test asked for a restart", "resume": false }),
        )
    } else if last_user.contains("new tool") {
        Script::ToolCall(
            "new_tool",
            json!({ "name": "greeter", "description": "Greets a person by name" }),
        )
    } else if last_user.contains("break the tool") {
        // Deliberately invalid Rust: the model should get the compiler's
        // complaint back in the same turn.
        Script::ToolCall(
            "write_code",
            json!({
                "target": "tool:greeter",
                "path": "src/lib.rs",
                "contents": "this is not rust at all;\n"
            }),
        )
    } else if last_user.contains("fix the tool") {
        Script::ToolCall(
            "write_code",
            json!({ "target": "tool:greeter", "path": "src/lib.rs", "contents": GREETER_SOURCE }),
        )
    } else if last_user.contains("use the tool") {
        Script::ToolCall("greeter", json!({ "name": "Ada" }))
    } else if last_user.contains("list my code") {
        Script::ToolCall("list_code", json!({ "target": "tool:greeter" }))
    } else if last_user.contains("show history") {
        Script::ToolCall("history", json!({ "target": "tool:greeter" }))
    } else if last_user.contains("undo the tool") {
        Script::ToolCall("rollback", json!({ "target": "tool:greeter" }))
    } else if last_user.contains("modify yourself") {
        // Rewrites the agent's own reply path, so the change is visible in the
        // next message it sends.
        Script::ToolCall(
            "patch_code",
            json!({
                "target": "self",
                "path": "src/lib.rs",
                "old_text": "        }\n        llm::stream_close(stream);\n        Ok(reply)",
                "new_text": "        }\n        llm::stream_close(stream);\n        reply.text.push_str(\"\\n\\n— sent by a self-modified agent\");\n        Ok(reply)"
            }),
        )
    } else if last_user.contains("undo yourself") {
        Script::ToolCall("rollback", json!({ "target": "self" }))
    } else if last_user.contains("what are you working with") {
        // Reports the context it was actually given, so skill attachment and
        // mode filtering can be checked without a real model.
        let skills: Vec<&str> = system_prompt
            .lines()
            .filter(|l| l.starts_with("## "))
            .map(|l| l.trim_start_matches("## "))
            .collect();
        Script::Text(
            format!(
                "skills=[{}] tools={} names=[{}]",
                skills.join(", "),
                tool_names.len(),
                tool_names.join(", ")
            ),
            false,
        )
    } else if images > 0 {
        // Reports what actually arrived, so the multimodal payload is verifiable
        // without a real vision model.
        Script::Text(
            format!(
                "I received {} image{} and the text: {}",
                images,
                if images == 1 { "" } else { "s" },
                body_preview(&last_user)
            ),
            false,
        )
    } else {
        Script::Text(
            format!("Hello. You said: {}", body_preview(&last_user)),
            false,
        )
    };

    Sse::new(script.into_stream())
}

/// A working implementation the scripted "fix the tool" step writes, standing
/// in for what a real model would produce after reading the compiler error.
const GREETER_SOURCE: &str = r##"wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

use serde_json::{json, Value};

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "greeter".to_string(),
            description: "Greets a person by name".to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            })
            .to_string(),
            capabilities: vec![],
        }
    }

    fn invoke(_session_id: String, args_json: String) -> Result<String, String> {
        let args: Value = serde_json::from_str(&args_json).map_err(|e| e.to_string())?;
        let name = args.get("name").and_then(Value::as_str).unwrap_or("stranger");
        Ok(format!("Hello, {name}! Greetings from a tool that wrote itself."))
    }
}

export!(Component);
"##;

/// Reads a chat message's `content`, which is either a plain string or the
/// multi-part array used when a message carries attachments.
fn read_user_content(content: &Value) -> (String, usize) {
    match content {
        Value::String(text) => (text.clone(), 0),
        Value::Array(parts) => {
            let mut text = String::new();
            let mut images = 0;
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                text.push(' ');
                            }
                            text.push_str(t);
                        }
                    }
                    Some("image_url") => {
                        let is_data_url = part
                            .get("image_url")
                            .and_then(|u| u.get("url"))
                            .and_then(Value::as_str)
                            .is_some_and(|u| u.starts_with("data:image/"));
                        if is_data_url {
                            images += 1;
                        }
                    }
                    _ => {}
                }
            }
            (text, images)
        }
        _ => (String::new(), 0),
    }
}

fn body_preview(text: &str) -> String {
    let t = text.trim();
    if t.is_empty() {
        "nothing at all".to_string()
    } else {
        t.chars().take(120).collect()
    }
}

enum Script {
    /// Reply text, and whether to stream it slowly.
    Text(String, bool),
    ToolCall(&'static str, Value),
}

impl Script {
    fn into_stream(self) -> impl Stream<Item = Result<Event, Infallible>> {
        let frames = match self {
            Script::Text(text, slow) => text_frames(&text, slow),
            Script::ToolCall(name, args) => tool_frames(name, &args),
        };

        futures_util::stream::unfold(frames.into_iter(), |mut it| async move {
            let (payload, delay) = it.next()?;
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            Some((Ok(Event::default().data(payload)), it))
        })
    }
}

fn chunk(delta: Value, finish: Option<&str>) -> String {
    json!({
        "id": "mock-1",
        "model": "mock/echo",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
    })
    .to_string()
}

fn usage_chunk() -> String {
    json!({
        "id": "mock-1",
        "model": "mock/echo",
        "choices": [],
        "usage": { "prompt_tokens": 42, "completion_tokens": 17, "cost": 0.00012 },
    })
    .to_string()
}

fn text_frames(text: &str, slow: bool) -> Vec<(String, Duration)> {
    let gap = if slow {
        Duration::from_millis(220)
    } else {
        Duration::from_millis(15)
    };

    let mut frames = Vec::new();
    for word in text.split_inclusive(' ') {
        frames.push((chunk(json!({ "content": word }), None), gap));
    }
    frames.push((chunk(json!({}), Some("stop")), Duration::ZERO));
    frames.push((usage_chunk(), Duration::ZERO));
    frames.push(("[DONE]".to_string(), Duration::ZERO));
    frames
}

fn tool_frames(name: &str, args: &Value) -> Vec<(String, Duration)> {
    let args_text = args.to_string();
    // Split the arguments mid-string to prove the host reassembles fragments.
    let split = args_text.len() / 2;

    vec![
        (
            chunk(
                json!({ "tool_calls": [{
                    "index": 0,
                    "id": "call_mock_1",
                    "type": "function",
                    "function": { "name": name, "arguments": &args_text[..split] },
                }]}),
                None,
            ),
            Duration::from_millis(10),
        ),
        (
            chunk(
                json!({ "tool_calls": [{
                    "index": 0,
                    "function": { "arguments": &args_text[split..] },
                }]}),
                None,
            ),
            Duration::from_millis(10),
        ),
        (chunk(json!({}), Some("tool_calls")), Duration::ZERO),
        (usage_chunk(), Duration::ZERO),
        ("[DONE]".to_string(), Duration::ZERO),
    ]
}
