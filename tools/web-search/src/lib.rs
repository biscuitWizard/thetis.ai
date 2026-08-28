//! Search the web through Exa.
//!
//! Talks to `api.exa.ai/search` over `wasi:http`. The host terminates TLS, so
//! nothing here needs a crypto stack — which is just as well, since no TLS
//! crate builds for wasm32-wasip2.
//!
//! The API key comes from this tool's own `[tools.web-search]` block in the
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

/// Exa rejects anything larger, and a model cannot use more than this anyway.
const MAX_RESULTS: i64 = 25;
const DEFAULT_RESULTS: i64 = 8;
/// Enough text to judge a result by, short enough that ten of them still fit.
const SNIPPET_CHARS: i64 = 1_000;

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "web-search".to_string(),
            description: "Search the web and get back ranked results with titles, URLs, \
                          publication dates and a text snippet from each page. Use this to \
                          find pages; use web-content to read one in full, or web-summarize \
                          to get a direct answer with citations."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What to search for. A natural-language description \
                                        works better here than keywords."
                    },
                    "num_results": {
                        "type": "integer",
                        "description": format!("How many results to return, 1-{MAX_RESULTS}. \
                                                Defaults to {DEFAULT_RESULTS}.")
                    },
                    "domains": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Restrict results to these domains, e.g. \
                                        [\"arxiv.org\"]. Omit to search everywhere."
                    },
                    "start_published_date": {
                        "type": "string",
                        "description": "Only include pages published on or after this ISO \
                                        date, e.g. '2024-01-01'."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            })
            .to_string(),
            // "read-only" tells the agent this tool changes nothing outside the
            // conversation, so it survives a read-only mode instead of being
            // withheld with everything else opaque. Searching only reads.
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

        let num_results = args
            .get("num_results")
            .and_then(Value::as_i64)
            .unwrap_or(DEFAULT_RESULTS)
            .clamp(1, MAX_RESULTS);

        let mut body = json!({
            "query": query,
            "numResults": num_results,
            "contents": { "text": { "maxCharacters": SNIPPET_CHARS } },
        });

        if let Some(domains) = args.get("domains").and_then(Value::as_array) {
            let domains: Vec<&str> = domains.iter().filter_map(Value::as_str).collect();
            if !domains.is_empty() {
                body["includeDomains"] = json!(domains);
            }
        }
        if let Some(date) = args.get("start_published_date").and_then(Value::as_str) {
            body["startPublishedDate"] = json!(date);
        }

        sys::log(LogLevel::Debug, &format!("web-search: {query}"));

        let response = exa_post("/search", api_key, &body)?;
        Ok(format_results(&response, query))
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
             [tools.web-search]\n\
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
        // The message matters more than the code: 401 means the key is wrong,
        // 402 means the account is out of credit, and the model can only react
        // sensibly if it can tell those apart.
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

fn format_results(response: &Value, query: &str) -> String {
    let Some(results) = response.get("results").and_then(Value::as_array) else {
        return format!("No results for {query:?}.");
    };
    if results.is_empty() {
        return format!("No results for {query:?}.");
    }

    let mut out = format!("{} result(s) for {query:?}\n", results.len());
    for (i, result) in results.iter().enumerate() {
        let title = result
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("(untitled)");
        let url = result.get("url").and_then(Value::as_str).unwrap_or("");

        out.push_str(&format!("\n{}. {title}\n   {url}\n", i + 1));

        if let Some(date) = result.get("publishedDate").and_then(Value::as_str) {
            out.push_str(&format!("   published: {}\n", iso_day(date)));
        }
        if let Some(author) = result.get("author").and_then(Value::as_str) {
            if !author.trim().is_empty() {
                out.push_str(&format!("   author: {author}\n"));
            }
        }
        if let Some(text) = result.get("text").and_then(Value::as_str) {
            let snippet = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if !snippet.is_empty() {
                out.push_str(&format!("   {}\n", truncate(&snippet, 400)));
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
