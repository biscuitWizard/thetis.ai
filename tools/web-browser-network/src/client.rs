//! The shared half of every `web-browser-*` tool: one POST to the sidecar, and
//! one renderer for what comes back.
//!
//! Duplicated verbatim in each crate rather than shared, because a tool is a
//! standalone package with no path dependencies allowed. Keep the copies
//! identical; a change here is a change to all of them.

use serde_json::{json, Map, Value};
use std::time::Duration;

/// Parses the model's arguments into an object we can add `op` and `session` to.
pub fn args(args_json: &str) -> Result<Map<String, Value>, String> {
    if args_json.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(args_json) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(Value::Null) => Ok(Map::new()),
        Ok(other) => Err(format!(
            "arguments must be a JSON object, got {other}"
        )),
        Err(e) => Err(format!("arguments were not valid JSON: {e}")),
    }
}

/// Where the sidecar is and what token it wants.
///
/// Both are injected by the kernel into every tool whose name starts with
/// `web-browser` (`Config::tool_config_json`), because the port is
/// settings-derived and the token is generated fresh each boot. Neither is
/// something a user should have to write into a config file.
struct Endpoint {
    base: String,
    token: String,
    enabled: bool,
}

fn endpoint(config_json: &str) -> Endpoint {
    let cfg: Value = serde_json::from_str(config_json).unwrap_or_else(|_| json!({}));
    Endpoint {
        base: cfg
            .get("endpoint")
            .and_then(|v| v.as_str())
            .unwrap_or("http://127.0.0.1:39412")
            .trim_end_matches('/')
            .to_string(),
        token: cfg
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        // Absent means enabled: a tool that loaded without a config block
        // should try and report a real connection error, not refuse.
        enabled: cfg.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
    }
}

/// Posts one operation to the sidecar and renders the result for a model.
///
/// `session_id` becomes the sidecar's context key, which is Playwright's
/// isolation boundary — so each conversation browses with its own cookies and
/// storage without any tool having to think about it.
pub fn call(
    op: &str,
    session_id: &str,
    mut args: Map<String, Value>,
    config_json: &str,
) -> Result<String, String> {
    let ep = endpoint(config_json);
    if !ep.enabled {
        return Err("the headless browser is turned off in this deployment \
                    (browser.enabled = false in thetis.toml). Turn it on and restart \
                    the orchestrator to use the web-browser-* tools."
            .to_string());
    }

    args.insert("op".to_string(), json!(op));
    // A blank session id would collide with the sidecar's own 'default' key.
    args.insert(
        "session".to_string(),
        json!(if session_id.is_empty() { "default" } else { session_id }),
    );
    let body = Value::Object(args).to_string();

    let mut request = waki::Client::new()
        .post(&format!("{}/op", ep.base))
        .header("content-type", "application/json");
    if !ep.token.is_empty() {
        request = request.header("x-thetis-token", &ep.token);
    }

    let response = request
        .body(body.into_bytes())
        // Generous: a slow page can legitimately take the sidecar's own 15s
        // operation timeout, and this must not fire first.
        .connect_timeout(Duration::from_secs(60))
        .send()
        .map_err(|e| unreachable_message(&ep.base, &e.to_string()))?;

    let status = response.status_code();
    let bytes = response
        .body()
        .map_err(|e| format!("could not read the browser sidecar's response: {e}"))?;
    let text = String::from_utf8_lossy(&bytes).to_string();

    if status == 403 {
        return Err("the browser sidecar rejected this tool's token. It is generated at \
                    boot and handed to tools automatically, so a mismatch usually means \
                    the sidecar outlived a restart — restart the orchestrator."
            .to_string());
    }
    if status != 200 {
        return Err(format!(
            "the browser sidecar returned {status}: {}",
            truncate(&text, 500)
        ));
    }

    let parsed: Value = serde_json::from_str(&text)
        .map_err(|e| format!("the browser sidecar's response was not JSON: {e}: {}", truncate(&text, 300)))?;

    // The sidecar reports operation failures as 200 with ok:false, keeping
    // transport errors and page errors distinguishable.
    if parsed.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        let err = parsed
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("the browser reported a failure with no message");
        return Err(err.to_string());
    }

    Ok(render(&parsed))
}

fn unreachable_message(base: &str, err: &str) -> String {
    format!(
        "could not reach the browser sidecar at {base}: {err}\n\n\
         The kernel starts it at boot and supervises it. If this persists, it \
         failed to start — check the orchestrator log for 'browser' or 'playwright', \
         and confirm node is installed and services/playwright-sidecar has its \
         node_modules."
    )
}

/// Renders the sidecar's JSON as flat text.
///
/// Deliberately not pretty-printed JSON: every response carries page context
/// (url, title, tab count) that is noise to re-read on each call, so the
/// context goes on one line and the payload gets the room. Keys are emitted in
/// a fixed order so consecutive calls read as a diff rather than a reshuffle.
fn render(v: &Value) -> String {
    let obj = match v.as_object() {
        Some(o) => o,
        None => return v.to_string(),
    };

    let mut out = String::new();

    // The page context line, when there is one.
    let url = obj.get("url").and_then(|v| v.as_str()).unwrap_or("");
    if !url.is_empty() {
        let title = obj.get("title").and_then(|v| v.as_str()).unwrap_or("");
        if title.is_empty() {
            out.push_str(&format!("page: {url}"));
        } else {
            out.push_str(&format!("page: {title} — {url}"));
        }
        let tabs = obj.get("tabs").and_then(|v| v.as_u64()).unwrap_or(1);
        if tabs > 1 {
            let active = obj.get("activeTab").and_then(|v| v.as_u64()).unwrap_or(0);
            out.push_str(&format!(" [tab {active} of {tabs}]"));
        }
        out.push('\n');
    }

    // Everything else, skipping what the context line already said.
    const CONTEXT: &[&str] = &["ok", "url", "title", "tabs", "activeTab"];
    // Scalars and short notes read better before a long snapshot or list.
    const LAST: &[&str] = &["snapshot", "snapshotNote", "hint"];

    let mut emit = |key: &str, val: &Value| {
        if CONTEXT.contains(&key) {
            return;
        }
        match val {
            // A list of preformatted lines: the sidecar already numbered them.
            Value::Array(items) if items.iter().all(|i| i.is_string()) => {
                out.push_str(&format!("\n{key} ({}):\n", items.len()));
                for item in items {
                    out.push_str(&format!("  {}\n", item.as_str().unwrap_or_default()));
                }
            }
            Value::Array(items) => {
                out.push_str(&format!(
                    "\n{key} ({}):\n{}\n",
                    items.len(),
                    serde_json::to_string_pretty(val).unwrap_or_else(|_| val.to_string())
                ));
            }
            Value::Object(_) => {
                out.push_str(&format!(
                    "\n{key}:\n{}\n",
                    serde_json::to_string_pretty(val).unwrap_or_else(|_| val.to_string())
                ));
            }
            Value::String(s) if s.contains('\n') => {
                out.push_str(&format!("\n{key}:\n{s}\n"));
            }
            Value::String(s) => out.push_str(&format!("{key}: {s}\n")),
            other => out.push_str(&format!("{key}: {other}\n")),
        }
    };

    for (key, val) in obj {
        if !LAST.contains(&key.as_str()) {
            emit(key, val);
        }
    }
    for key in LAST {
        if let Some(val) = obj.get(*key) {
            emit(key, val);
        }
    }

    if out.trim().is_empty() {
        return "the browser reported success with nothing to show.".to_string();
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}
