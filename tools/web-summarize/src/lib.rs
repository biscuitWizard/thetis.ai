//! Answer a question from the web, with citations, through Exa.
//!
//! Talks to `api.exa.ai/answer` over `wasi:http`. Exa searches, reads the pages
//! it finds and synthesises an answer, so this is one call rather than a
//! search-then-read loop. The host terminates TLS.
//!
//! The API key comes from this tool's own `[tools.web-summarize]` block in the
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

/// Exa reads several pages before answering, so this is slower than a search.
/// Kept under the host's tool budget so the timeout comes from here, with a
/// message, rather than from the watchdog.
const TIMEOUT_SECS: u64 = 25;
/// Per citation. Enough to check the answer against its source.
const CITATION_CHARS: usize = 600;

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "web-summarize".to_string(),
            description: "Ask a question and get a synthesised answer drawn from current web \
                          sources, with the citations it was built from. Use this when you \
                          want the answer rather than a list of links; use web-search when \
                          you want to choose the sources yourself."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The question to answer. A full question works better \
                                        than keywords."
                    },
                    "include_sources": {
                        "type": "boolean",
                        "description": "Include an excerpt from each cited page as well as \
                                        its title and URL. Defaults to true."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            })
            .to_string(),
            // Summarising only reads; see web-search for the rationale.
            capabilities: vec!["http".to_string(), "read-only".to_string()],
        }
    }

    fn invoke(_session_id: String, args_json: String, config_json: String) -> Result<String, String> {
        let args: Value = serde_json::from_str(&args_json)
            .map_err(|e| format!("arguments were not valid JSON: {e}"))?;
        let config: Value = serde_json::from_str(&config_json).unwrap_or(json!({}));

        let api_key = api_key(&config)?;
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .filter(|q| !q.trim().is_empty())
            .ok_or("missing required argument 'query'")?;

        let include_sources = args
            .get("include_sources")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        // `text` asks Exa to return the source text alongside the answer, which
        // is what makes the citations checkable rather than merely listed.
        let body = json!({ "query": query, "text": include_sources });

        sys::log(LogLevel::Debug, &format!("web-summarize: {query}"));

        let response = exa_post("/answer", api_key, &body)?;
        Ok(format_answer(&response, query, include_sources))
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
             [tools.web-summarize]\n\
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
        .connect_timeout(Duration::from_secs(TIMEOUT_SECS))
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

fn format_answer(response: &Value, query: &str, include_sources: bool) -> String {
    let answer = response
        .get("answer")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();

    if answer.is_empty() {
        return format!("Exa produced no answer for {query:?}.");
    }

    let mut out = format!("{answer}\n");

    let citations = response
        .get("citations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if citations.is_empty() {
        // Worth saying: an uncited answer is one the model should trust less.
        out.push_str("\n(Exa returned no citations for this answer.)\n");
        return out;
    }

    out.push_str(&format!("\nSources ({}):\n", citations.len()));
    for (i, citation) in citations.iter().enumerate() {
        let title = citation
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("(untitled)");
        let url = citation.get("url").and_then(Value::as_str).unwrap_or("");
        out.push_str(&format!("\n{}. {title}\n   {url}\n", i + 1));

        if let Some(date) = citation.get("publishedDate").and_then(Value::as_str) {
            out.push_str(&format!("   published: {}\n", iso_day(date)));
        }
        if include_sources {
            if let Some(text) = citation.get("text").and_then(Value::as_str) {
                let excerpt = text.split_whitespace().collect::<Vec<_>>().join(" ");
                if !excerpt.is_empty() {
                    out.push_str(&format!("   {}\n", truncate(&excerpt, CITATION_CHARS)));
                }
            }
        }
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
