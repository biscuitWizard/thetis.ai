//! Read the full text of web pages through Exa.
//!
//! Talks to `api.exa.ai/contents` over `wasi:http`. The host terminates TLS.
//!
//! The API key comes from this tool's own `[tools.web-content]` block in the
//! config. Put it in `thetis.local.toml`, which is gitignored, rather than in
//! `thetis.toml`, which is committed.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

use thetis::grip::sys;
use thetis::grip::types::LogLevel;
use serde_json::{json, Value};
use std::time::Duration;

/// Fetching many pages at once is the fastest way to blow the context window.
const MAX_URLS: usize = 10;
/// Per page. The host caps tool output as well, so this is the polite limit
/// rather than the enforced one.
const DEFAULT_CHARS: i64 = 6_000;
const MAX_CHARS: i64 = 40_000;

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "web-content".to_string(),
            description: "Fetch and read the full text of one or more web pages by URL. Use \
                          this after web-search to actually read a page, or directly when you \
                          already know the URL. Returns cleaned article text, not raw HTML."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "urls": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": format!("Page URLs to read, at most {MAX_URLS}.")
                    },
                    "max_characters": {
                        "type": "integer",
                        "description": format!("Characters of text per page. Defaults to \
                                                {DEFAULT_CHARS}, maximum {MAX_CHARS}.")
                    },
                    "livecrawl": {
                        "type": "string",
                        "enum": ["always", "fallback", "never"],
                        "description": "Whether to fetch the page fresh. 'fallback' (the \
                                        default) uses Exa's cache and crawls only if it has \
                                        nothing. Use 'always' when the page changes often."
                    }
                },
                "required": ["urls"],
                "additionalProperties": false
            })
            .to_string(),
            // Fetching a page only reads; see web-search for the rationale.
            capabilities: vec!["http".to_string(), "read-only".to_string()],
        }
    }

    fn invoke(_session_id: String, args_json: String, config_json: String) -> Result<String, String> {
        let args: Value = serde_json::from_str(&args_json)
            .map_err(|e| format!("arguments were not valid JSON: {e}"))?;
        let config: Value = serde_json::from_str(&config_json).unwrap_or(json!({}));

        let api_key = api_key(&config)?;

        let urls: Vec<&str> = args
            .get("urls")
            .and_then(Value::as_array)
            .ok_or("missing required argument 'urls'")?
            .iter()
            .filter_map(Value::as_str)
            .filter(|u| !u.trim().is_empty())
            .collect();

        if urls.is_empty() {
            return Err("'urls' must contain at least one URL".to_string());
        }
        if urls.len() > MAX_URLS {
            return Err(format!(
                "{} URLs requested but at most {MAX_URLS} can be read at once; split the call",
                urls.len()
            ));
        }

        let max_chars = args
            .get("max_characters")
            .and_then(Value::as_i64)
            .unwrap_or(DEFAULT_CHARS)
            .clamp(200, MAX_CHARS);

        let mut body = json!({
            "urls": urls,
            "text": { "maxCharacters": max_chars },
        });
        if let Some(mode) = args.get("livecrawl").and_then(Value::as_str) {
            if matches!(mode, "always" | "fallback" | "never") {
                body["livecrawl"] = json!(mode);
            }
        }

        sys::log(
            LogLevel::Debug,
            &format!("web-content: {} url(s)", urls.len()),
        );

        let response = exa_post("/contents", api_key, &body)?;
        Ok(format_contents(&response, &urls))
    }
}

/// Reads the key from this tool's config block, with an error that says where
/// to put it rather than just reporting its absence.
fn api_key(config: &Value) -> Result<&str, String> {
    config
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|k| !k.trim().is_empty())
        .ok_or_else(|| {
            "no Exa API key configured. Add this to thetis.local.toml, which is gitignored:\n\
             \n\
             [tools.web-content]\n\
             api_key = \"your-exa-key\"\n\
             \n\
             Then restart the orchestrator."
                .to_string()
        })
}

/// One POST to Exa, with its errors turned into something a model can act on.
fn exa_post(path: &str, api_key: &str, body: &Value) -> Result<Value, String> {
    let response = waki::Client::new()
        .post(&format!("https://api.exa.ai{path}"))
        .header("x-api-key", api_key)
        .header("Content-Type", "application/json")
        .body(body.to_string().into_bytes())
        .connect_timeout(Duration::from_secs(20))
        .send()
        .map_err(|e| format!("could not reach api.exa.ai: {e}"))?;

    let status = response.status_code();
    let bytes = response
        .body()
        .map_err(|e| format!("could not read the response from Exa: {e}"))?;
    let text = String::from_utf8_lossy(&bytes);

    if status != 200 {
        return Err(match status {
            401 => "Exa rejected the API key (401). Check api_key in the config.".to_string(),
            402 => "The Exa account is out of credit (402).".to_string(),
            429 => "Exa is rate limiting these requests (429). Try again shortly.".to_string(),
            _ => format!("Exa returned {status}: {}", truncate(&text, 400)),
        });
    }

    serde_json::from_str(&text)
        .map_err(|e| format!("Exa's response was not JSON: {e}: {}", truncate(&text, 300)))
}

fn format_contents(response: &Value, requested: &[&str]) -> String {
    let results = response
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if results.is_empty() {
        return "Exa returned no content for those URLs. They may be unreachable or \
                blocked to crawlers."
            .to_string();
    }

    let mut out = String::new();
    for result in &results {
        let title = result
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("(untitled)");
        let url = result.get("url").and_then(Value::as_str).unwrap_or("");

        out.push_str(&format!("=== {title} ===\n{url}\n"));
        if let Some(date) = result.get("publishedDate").and_then(Value::as_str) {
            out.push_str(&format!("published: {}\n", iso_day(date)));
        }
        out.push('\n');

        match result.get("text").and_then(Value::as_str) {
            Some(text) if !text.trim().is_empty() => {
                out.push_str(text.trim());
                out.push('\n');
            }
            _ => out.push_str("(no text could be extracted from this page)\n"),
        }
        out.push('\n');
    }

    // Say plainly which URLs produced nothing, rather than letting the model
    // infer it from a shorter list than it asked for.
    let returned: Vec<&str> = results
        .iter()
        .filter_map(|r| r.get("url").and_then(Value::as_str))
        .collect();
    let missing: Vec<&&str> = requested
        .iter()
        .filter(|u| !returned.iter().any(|r| r == *u))
        .collect();
    if !missing.is_empty() {
        out.push_str(&format!(
            "No content returned for: {}\n",
            missing
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    out
}

/// The day part of an ISO timestamp, without slicing into a multi-byte char.
fn iso_day(date: &str) -> String {
    date.chars().take(10).collect()
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}...")
}

export!(Component);
