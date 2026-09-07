//! Shared Notion API client, formatting and error handling.
//!
//! This file is duplicated verbatim into every `notion-*` tool crate. Each tool
//! is a standalone cargo package built for wasm32-wasip2 — there is no
//! workspace to hold a common library — so the copies are the price of the
//! component boundary. Keep them identical: edit one, copy to all.
//!
//! Everything talks to `api.notion.com` over `wasi:http`. The host terminates
//! TLS, so nothing here needs a crypto stack.

#![allow(dead_code)] // Each tool uses a subset of this module.

// The bindings are generated at the crate root by `wit_bindgen::generate!`.
use crate::thetis::grip::sys;
use crate::thetis::grip::types::LogLevel;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

pub const API_BASE: &str = "https://api.notion.com";

/// The API version this group was written against. Notion *requires* the
/// header, and pinning it is the whole point of their versioning scheme: a
/// floating version would silently change property shapes under us.
pub const DEFAULT_VERSION: &str = "2026-03-11";

/// Markdown longer than this is cut with a note. The host truncates tool output
/// at 32 KiB anyway; cutting it here means we can say *why* it was cut and what
/// to do about it, instead of the text just stopping mid-word.
pub const MAX_MARKDOWN_CHARS: usize = 18_000;

/// A configured client. Cheap to construct; holds no connection.
pub struct Notion {
    token: String,
    version: String,
    beta: Option<String>,
    timeout: Duration,
}

impl Notion {
    /// Builds a client from the tool's own config block.
    ///
    /// The token is read from `[tools.notion]`, which every tool in this group
    /// inherits, so one credential serves all of them. `api_key` and
    /// `auth_token` are accepted as aliases because that is what the other
    /// tools in this tree call the same field, and guessing wrong here is a
    /// confusing failure.
    pub fn from_config(config_json: &str) -> Result<Self, String> {
        let config: Value = serde_json::from_str(config_json).unwrap_or(json!({}));

        let token = ["token", "api_key", "auth_token", "integration_token"]
            .iter()
            .filter_map(|k| config.get(*k).and_then(Value::as_str))
            .map(str::trim)
            .find(|t| !t.is_empty())
            .ok_or_else(|| MISSING_TOKEN.to_string())?
            .to_string();

        let version = config
            .get("version")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(DEFAULT_VERSION)
            .to_string();

        let beta = config
            .get("beta")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string);

        let timeout = config
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(30)
            .clamp(5, 120);

        Ok(Self {
            token,
            version,
            beta,
            timeout: Duration::from_secs(timeout),
        })
    }

    pub fn get(&self, path: &str, query: &[(String, String)]) -> Result<Value, String> {
        self.send("GET", path, query, None)
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.send("POST", path, &[], Some(body))
    }

    pub fn patch(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.send("PATCH", path, &[], Some(body))
    }

    pub fn delete(&self, path: &str) -> Result<Value, String> {
        self.send("DELETE", path, &[], None)
    }

    /// One request, with Notion's errors turned into something a model can act
    /// on rather than a status code.
    pub fn send(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let url = format!("{API_BASE}{path}");
        sys::log(LogLevel::Debug, &format!("notion: {method} {path}"));

        let client = waki::Client::new();
        let mut request = match method {
            "GET" => client.get(&url),
            "POST" => client.post(&url),
            "PATCH" => client.patch(&url),
            "DELETE" => client.delete(&url),
            other => return Err(format!("unsupported HTTP method {other:?}")),
        };

        request = request
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Notion-Version", &self.version)
            .header("Content-Type", "application/json")
            .connect_timeout(self.timeout);

        if let Some(beta) = &self.beta {
            request = request.header("Notion-Beta", beta);
        }
        if !query.is_empty() {
            request = request.query(query);
        }
        if let Some(body) = body {
            request = request.body(body.to_string().into_bytes());
        }

        let response = request
            .send()
            .map_err(|e| format!("could not reach api.notion.com: {e}"))?;

        let status = response.status_code();
        let bytes = response
            .body()
            .map_err(|e| format!("could not read Notion's response: {e}"))?;
        let text = String::from_utf8_lossy(&bytes).to_string();

        if (200..300).contains(&status) {
            if text.trim().is_empty() {
                return Ok(json!({}));
            }
            return serde_json::from_str(&text).map_err(|e| {
                format!("Notion's response was not JSON: {e}: {}", clip(&text, 300))
            });
        }

        Err(explain_error(status, &text))
    }

    /// Walks a paginated endpoint until `has_more` is false or `max` results are
    /// collected, whichever comes first.
    ///
    /// Returns the results and, when the walk stopped early, the cursor to
    /// resume from — so a caller can report an honest "there is more" rather
    /// than implying it saw everything.
    pub fn paginate(
        &self,
        method: &str,
        path: &str,
        base: &Value,
        max: usize,
    ) -> Result<(Vec<Value>, Option<String>), String> {
        let mut collected: Vec<Value> = Vec::new();
        let mut cursor: Option<String> = base
            .get("start_cursor")
            .and_then(Value::as_str)
            .map(str::to_string);

        loop {
            let want = max.saturating_sub(collected.len()).min(100);
            if want == 0 {
                break;
            }

            let response = if method == "GET" {
                let mut query: Vec<(String, String)> = base
                    .as_object()
                    .map(|o| {
                        o.iter()
                            .filter(|(k, _)| *k != "start_cursor" && *k != "page_size")
                            .filter_map(|(k, v)| scalar(v).map(|s| (k.clone(), s)))
                            .collect()
                    })
                    .unwrap_or_default();
                query.push(("page_size".to_string(), want.to_string()));
                if let Some(c) = &cursor {
                    query.push(("start_cursor".to_string(), c.clone()));
                }
                self.get(path, &query)?
            } else {
                let mut body = base.clone();
                body["page_size"] = json!(want);
                match &cursor {
                    Some(c) => body["start_cursor"] = json!(c),
                    None => {
                        if let Some(o) = body.as_object_mut() {
                            o.remove("start_cursor");
                        }
                    }
                }
                self.send(method, path, &[], Some(&body))?
            };

            if let Some(results) = response.get("results").and_then(Value::as_array) {
                collected.extend(results.iter().cloned());
            }

            let has_more = response
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let next = response
                .get("next_cursor")
                .and_then(Value::as_str)
                .map(str::to_string);

            // A cursor is opaque: pass it back verbatim, never parse it.
            match (has_more, next) {
                (true, Some(next)) if collected.len() < max => cursor = Some(next),
                (true, next) => return Ok((collected, next)),
                (false, _) => return Ok((collected, None)),
            }
        }

        Ok((collected, cursor))
    }
}

const MISSING_TOKEN: &str = "no Notion token configured. Create an internal \
connection or personal access token at https://www.notion.so/my-integrations, \
then add this to thetis.local.toml, which is gitignored:\n\
\n\
[tools.notion]\n\
token = \"ntn_...\"\n\
\n\
Then restart the orchestrator. Every notion-* tool inherits this block, so the \
token only has to be set once.";

/// Notion's error body carries a stable `code` and a human `message`. The code
/// is what to branch on; the message is what to show. Both beat a bare status.
fn explain_error(status: u16, text: &str) -> String {
    let parsed: Value = serde_json::from_str(text).unwrap_or(json!({}));
    let code = parsed.get("code").and_then(Value::as_str).unwrap_or("");
    let message = parsed
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_else(|| text.trim());

    // Matched case-insensitively: Notion's wording varies between
    // endpoints ("Multiple matches", "no matches found"), and a hint that
    // depends on capitalisation is a hint that silently stops working.
    let lower = message.to_ascii_lowercase();

    let hint = match (status, code) {
        (401, _) => Some(
            "The token was rejected. Check `token` in [tools.notion] — a personal access token \
             starts with `ntn_`.",
        ),
        (404, "object_not_found") => Some(
            "Either the id is wrong, or the page/database is not shared with this connection. \
             Sharing is per-page in Notion: open the page, ••• menu -> Connections -> add \
             yours. Children inherit access from the page you share.",
        ),
        // The generic hint used to talk about comments whatever the call was,
        // which is actively misleading on a user lookup — that one is a hard
        // limit of the token type, not a setting anybody can turn on.
        (403, "restricted_resource") if lower.contains("user") => Some(
            "A personal access token may only look up its own user. Listing a workspace's \
             people needs an integration token with user-information capability, so names \
             cannot be resolved with this credential; ids still identify people uniquely.",
        ),
        (403, "restricted_resource") => Some(
            "The connection lacks the capability this call needs. Comment reading and writing \
             are off by default; enable them in the connection's Configuration tab.",
        ),
        // A validation_error covers everything from a bad property value to an
        // unmatched search string, so the message decides the advice. A hint
        // about schemas on a failed text edit sends the reader the wrong way.
        (400, "validation_error") if lower.contains("no matches found") => Some(
            "The text to replace is not on the page. Read the page with notion-page-get first \
             and copy the exact wording — whitespace and typographic quotes both matter.",
        ),
        (400, "validation_error") if lower.contains("multiple matches") => Some(
            "That text appears more than once, so the edit is ambiguous. Either extend it until \
             it is unique, or set replace_all on that edit.",
        ),
        (400, "validation_error") if lower.contains("should be defined") => Some(
            "A required field is missing from the request body. The message names it; this is a \
             defect in the tool rather than in how it was called.",
        ),
        (400, "validation_error") => Some(
            "Notion rejected the request body. When writing properties, they must match the \
             parent data source's schema exactly — check notion-database-schema.",
        ),
        (429, _) => Some("Rate limited. Wait a moment and retry; the average ceiling is about three requests a second."),
        (409, "conflict_error") => Some("A concurrent edit conflicted. Retrying usually succeeds."),
        _ => None,
    };

    let mut out = format!("Notion returned {status}");
    if !code.is_empty() {
        out.push_str(&format!(" ({code})"));
    }
    out.push_str(&format!(": {}", clip(message, 600)));
    if let Some(hint) = hint {
        out.push_str(&format!("\n\n{hint}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Turns anything that carries a Notion id into a dashed UUID.
///
/// Accepts a bare id with or without dashes, and any Notion URL — the app
/// slugifies titles with dashes and puts the id last, so both
/// `https://www.notion.so/My-Page-1f2e3d...` and
/// `https://app.notion.com/p/1f2e3d...` reduce to the same thing. URLs are
/// what a person actually has to hand, so accepting them is not a nicety.
pub fn normalize_id(raw: &str, what: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("missing required argument '{what}'"));
    }

    // Drop any query string or fragment, then take the last path segment.
    let core = trimmed
        .split(['?', '#'])
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches('/');
    let segment = core.rsplit('/').next().unwrap_or(core);

    // Dashes are both uuid separators and slug separators, so removing them
    // leaves the id as the trailing 32 hex characters either way.
    let compact: Vec<char> = segment.chars().filter(|c| *c != '-').collect();
    if compact.len() >= 32 {
        let tail: String = compact[compact.len() - 32..].iter().collect();
        if tail.chars().all(|c| c.is_ascii_hexdigit()) {
            let lower = tail.to_ascii_lowercase();
            return Ok(format!(
                "{}-{}-{}-{}-{}",
                &lower[0..8],
                &lower[8..12],
                &lower[12..16],
                &lower[16..20],
                &lower[20..32]
            ));
        }
    }

    Err(format!(
        "{raw:?} does not look like a Notion {what}. Give a 32-character id, a dashed UUID, or \
         the page URL copied from Notion."
    ))
}

// ---------------------------------------------------------------------------
// Rendering
//
// Notion's JSON is deeply wrapped: every value is an object tagged with its own
// type. Handing that to a model verbatim burns context on braces, so these
// helpers flatten it to lines a reader can scan while keeping every id visible,
// because ids are what the next call needs.
// ---------------------------------------------------------------------------

/// Flattens a rich-text array to plain text, keeping link targets.
pub fn rich_text(value: &Value) -> String {
    let Some(items) = value.as_array() else {
        return String::new();
    };
    let mut out = String::new();
    for item in items {
        let text = item
            .get("plain_text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                item.get("text")
                    .and_then(|t| t.get("content"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            });
        out.push_str(&text);
        if let Some(url) = item.get("href").and_then(Value::as_str) {
            if !url.is_empty() {
                out.push_str(&format!(" <{url}>"));
            }
        }
    }
    out
}

/// The title of a page, data source or database, wherever it happens to live.
pub fn title_of(object: &Value) -> String {
    // Databases and data sources carry a top-level title array.
    if let Some(t) = object.get("title") {
        let text = rich_text(t);
        if !text.trim().is_empty() {
            return text;
        }
    }
    // A page's title is whichever property has type "title".
    if let Some(props) = object.get("properties").and_then(Value::as_object) {
        for (_, value) in props {
            if value.get("type").and_then(Value::as_str) == Some("title") {
                let text = rich_text(value.get("title").unwrap_or(&Value::Null));
                if !text.trim().is_empty() {
                    return text;
                }
            }
        }
    }
    "(untitled)".to_string()
}

/// One page property value, flattened to a short string.
///
/// Returns `None` for a property that is genuinely empty, so callers can skip
/// it rather than printing a column of blanks.
pub fn describe_property(value: &Value) -> Option<String> {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    let inner = value.get(kind).unwrap_or(&Value::Null);

    let rendered = match kind {
        "title" | "rich_text" => rich_text(inner),
        "number" => inner.as_f64().map(|n| trim_float(n)).unwrap_or_default(),
        "checkbox" => match inner.as_bool() {
            Some(b) => b.to_string(),
            None => String::new(),
        },
        "select" | "status" => inner
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "multi_select" => names(inner).join(", "),
        "date" => {
            let start = inner.get("start").and_then(Value::as_str).unwrap_or("");
            match inner.get("end").and_then(Value::as_str) {
                Some(end) => format!("{start} -> {end}"),
                None => start.to_string(),
            }
        }
        "people" => inner
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|p| {
                        p.get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_else(|| p.get("id").and_then(Value::as_str).unwrap_or("?"))
                            .to_string()
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        "relation" => {
            let ids: Vec<&str> = inner
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|r| r.get("id").and_then(Value::as_str))
                        .collect()
                })
                .unwrap_or_default();
            ids.join(", ")
        }
        "files" => inner
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|f| f.get("name").and_then(Value::as_str).unwrap_or("file"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        "url" | "email" | "phone_number" | "created_time" | "last_edited_time" => {
            inner.as_str().unwrap_or("").to_string()
        }
        "created_by" | "last_edited_by" => inner
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "unique_id" => {
            let number = inner.get("number").and_then(Value::as_i64);
            match (inner.get("prefix").and_then(Value::as_str), number) {
                (Some(p), Some(n)) => format!("{p}-{n}"),
                (None, Some(n)) => n.to_string(),
                _ => String::new(),
            }
        }
        // Formulas and rollups nest another tagged value inside themselves.
        "formula" => describe_property(inner).unwrap_or_default(),
        "rollup" => match inner.get("type").and_then(Value::as_str) {
            Some("array") => inner
                .get("array")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(describe_property)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default(),
            _ => describe_property(inner).unwrap_or_default(),
        },
        "string" => inner.as_str().unwrap_or("").to_string(),
        "boolean" => inner.as_bool().map(|b| b.to_string()).unwrap_or_default(),
        "verification" => inner
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        // An unknown type is new, not broken: show its JSON rather than hiding
        // it. Notion adds response fields to every API version at once.
        _ => {
            if inner.is_null() {
                String::new()
            } else {
                clip(&inner.to_string(), 200)
            }
        }
    };

    let rendered = rendered.trim().to_string();
    if rendered.is_empty() {
        None
    } else {
        Some(rendered)
    }
}

/// Every non-empty property of a page, as `name (type): value` lines.
pub fn describe_properties(page: &Value, indent: &str) -> String {
    let Some(props) = page.get("properties").and_then(Value::as_object) else {
        return String::new();
    };

    // Sorted, so the same page renders the same way twice: Notion does not
    // guarantee object key order and a moving layout defeats prompt caching.
    let mut names: Vec<&String> = props.keys().collect();
    names.sort();

    let mut out = String::new();
    for name in names {
        let value = &props[name];
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("?");
        if let Some(rendered) = describe_property(value) {
            out.push_str(&format!(
                "{indent}{name} ({kind}): {}\n",
                clip(&rendered, 400)
            ));
        }
    }
    out
}

/// One line identifying a page or data source in a list of results.
pub fn object_line(object: &Value) -> String {
    let kind = object.get("object").and_then(Value::as_str).unwrap_or("?");
    let id = object.get("id").and_then(Value::as_str).unwrap_or("?");
    let mut line = format!("{} [{kind}] {id}", title_of(object));

    if let Some(url) = object.get("url").and_then(Value::as_str) {
        line.push_str(&format!("\n   {url}"));
    }
    if let Some(edited) = object.get("last_edited_time").and_then(Value::as_str) {
        line.push_str(&format!("\n   edited {}", clip(edited, 19)));
    }
    if object.get("in_trash").and_then(Value::as_bool) == Some(true)
        || object.get("archived").and_then(Value::as_bool) == Some(true)
    {
        line.push_str("\n   (in trash / archived)");
    }
    line
}

/// Where an object lives, as a line naming the parent's kind and id.
pub fn parent_line(object: &Value) -> Option<String> {
    let parent = object.get("parent")?;
    let kind = parent.get("type").and_then(Value::as_str).unwrap_or("?");
    let id = parent
        .get(kind)
        .and_then(Value::as_str)
        .or_else(|| parent.get(kind).and_then(|v| v.as_bool()).map(|_| "true"))
        .unwrap_or("");
    Some(if id.is_empty() {
        format!("parent: {kind}")
    } else {
        format!("parent: {kind} {id}")
    })
}

/// A page's markdown body, trimmed to something a context window can hold, with
/// the truncation stated rather than silent.
/// Shortens the signed URLs Notion puts in markdown for uploaded files.
///
/// An S3 link for an image comes back with the whole AWS signature attached —
/// about 1.5kB of credential, expiry and checksum per image. It is worthless to
/// a reader, it expires within the hour, and two of them on one page cost more
/// context than the page's actual prose. So the query string goes and the
/// filename stays, which is the only part that carries meaning.
///
/// Anything that is not one of Notion's own file hosts is left alone: a link to
/// a real web page may well need its query string.
fn shorten_signed_urls(markdown: &str) -> String {
    const SIGNED_HOSTS: &[&str] = &[
        "prod-files-secure.s3",
        "s3.us-west-2.amazonaws.com",
        "amazonaws.com",
        "attachment-secure",
    ];

    let mut out = String::with_capacity(markdown.len());
    let mut rest = markdown;

    // Walk URL starts rather than parsing markdown: the same shortening is
    // wanted whether the link came from an image, a link or bare text.
    while let Some(at) = rest.find("https://") {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        // A URL ends at whitespace or at the closing delimiter of the markdown
        // construct holding it.
        let end = tail
            .find(|c: char| c.is_whitespace() || c == ')' || c == '"' || c == '>')
            .unwrap_or(tail.len());
        let url = &tail[..end];

        match url.split_once('?') {
            Some((base, query))
                if SIGNED_HOSTS.iter().any(|h| base.contains(h)) && query.len() > 80 =>
            {
                out.push_str(base);
                out.push_str("?[signature removed]");
            }
            _ => out.push_str(url),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Display names for the user ids appearing in a payload, where obtainable.
///
/// Comments carry `created_by` as a bare `{object, id}` with no name, so a
/// thread otherwise renders as a wall of UUIDs.
///
/// Resolving them is usually impossible. A **personal access token** is refused
/// by both `/v1/users/{id}` and `/v1/users`:
///
/// ```text
/// 403 restricted_resource
/// Personal access tokens can only retrieve their own authorized user.
/// ```
///
/// So rather than spend a failing request per author, the workspace listing is
/// attempted **once** and the whole map built from it. An integration token
/// with the "read user information" capability gets real names; a personal
/// access token gets an empty map and ids are shortened for display instead.
pub fn resolve_user_names(client: &Notion) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();

    // One request, not one per author. A 403 here is the normal case, not an
    // error worth reporting: the caller degrades to shortened ids.
    let Ok(listing) = client.get("/v1/users", &[("page_size".into(), "100".into())]) else {
        return names;
    };

    for user in listing
        .get("results")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let (Some(id), Some(name)) = (
            user.get("id").and_then(Value::as_str),
            user.get("name").and_then(Value::as_str),
        ) else {
            continue;
        };
        if !name.trim().is_empty() {
            names.insert(id.to_string(), name.to_string());
        }
    }
    names
}

/// A user's display name, falling back to a shortened id.
///
/// A full UUID tells a reader nothing and costs 36 characters, so an
/// unresolvable author shows only the first segment — enough to tell two
/// participants apart in a thread, which is what the name was for.
pub fn user_label(user: &Value, names: &BTreeMap<String, String>) -> String {
    if let Some(name) = user.get("name").and_then(Value::as_str) {
        if !name.trim().is_empty() {
            return name.to_string();
        }
    }
    match user.get("id").and_then(Value::as_str) {
        Some(id) => names
            .get(id)
            .cloned()
            .unwrap_or_else(|| format!("user {}", id.split('-').next().unwrap_or(id))),
        None => "(unknown author)".to_string(),
    }
}

/// How to narrow a body that does not fit: a window, or a search.
///
/// Without this a long page was simply unreadable past the first 18,000
/// characters — the note said the text was cut and offered nothing to do about
/// it. A Notion page has no line numbers a caller can trust, so the window is
/// measured in characters, and the footer reports the offset to resume from.
#[derive(Default, Clone, Copy)]
pub struct BodyWindow<'a> {
    /// First character to return, counting from 0.
    pub offset: usize,
    /// How many characters to return. Zero means the default budget.
    pub limit: usize,
    /// Return only the paragraphs containing this text, instead of a window.
    pub find: Option<&'a str>,
}

pub fn markdown_body(response: &Value) -> String {
    markdown_body_window(response, BodyWindow::default())
}

pub fn markdown_body_window(response: &Value, window: BodyWindow<'_>) -> String {
    let markdown = response
        .get("markdown")
        .and_then(Value::as_str)
        .unwrap_or("");
    // Shorten before clipping, so the budget is spent on prose rather than on
    // AWS signatures.
    let markdown = shorten_signed_urls(markdown);

    let mut out = match window.find {
        Some(needle) => find_in_body(&markdown, needle),
        None => window_of_body(&markdown, window.offset, window.limit),
    };

    if response.get("truncated").and_then(Value::as_bool) == Some(true) {
        out.push_str(
            "\n\n[Notion truncated this page: it exceeds the block limit. \
             The unknown block ids below can be fetched individually.]",
        );
    }
    let unknown: Vec<&str> = response
        .get("unknown_block_ids")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !unknown.is_empty() {
        out.push_str(&format!(
            "\n\n[{} block(s) could not be loaded — unshared, unsupported, or truncated. \
             Ids: {}]",
            unknown.len(),
            unknown
                .iter()
                .take(10)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out
}

/// How much of a page to echo back after writing to it.
///
/// A write tool returns the page so the caller can see the edit landed. That is a
/// confirmation, not a read: echoing a whole long page after appending one line
/// spends thousands of tokens to answer a yes/no question. So the echo is small
/// and names the tool that does reading properly.
const PREVIEW_CHARS: usize = 4_000;

/// The body of a page after a write, capped as a confirmation rather than a read.
pub fn markdown_body_preview(response: &Value) -> String {
    let markdown = response
        .get("markdown")
        .and_then(Value::as_str)
        .unwrap_or("");
    let markdown = shorten_signed_urls(markdown);
    let total = markdown.chars().count();

    if total <= PREVIEW_CHARS {
        return markdown;
    }
    let kept: String = markdown.chars().take(PREVIEW_CHARS).collect();
    format!(
        "{kept}\n\n[{PREVIEW_CHARS} of {total} characters shown — this is a confirmation of the \
write, not the whole page. Read the rest with notion-page-get, which takes \
content_offset/content_limit and find.]"
    )
}

/// Returns a character window of a body, with a footer naming the next offset.
///
/// The footer goes last and always states the truth about what was returned,
/// because that is the line the caller needs in order to continue.
fn window_of_body(text: &str, offset: usize, limit: usize) -> String {
    let total = text.chars().count();
    let limit = if limit == 0 { MAX_MARKDOWN_CHARS } else { limit.min(MAX_MARKDOWN_CHARS) };

    if offset >= total && total > 0 {
        return format!("[offset {offset} is past the end; this page has {total} characters]");
    }

    let kept: String = text.chars().skip(offset).take(limit).collect();
    let end = offset + kept.chars().count();

    if offset == 0 && end >= total {
        return kept;
    }
    let mut out = kept;
    out.push_str(&format!(
        "\n\n[characters {offset}-{end} of {total}. Read on with offset {end}, \
or pass find to jump to the part you want.]"
    ));
    out
}

/// Returns the paragraphs of a body containing `needle`, with their offsets.
///
/// This is the cheaper question most callers actually have: they want the one
/// section that mentions something, not the first N characters of the page.
fn find_in_body(text: &str, needle: &str) -> String {
    let folded = needle.to_lowercase();
    let mut hits = Vec::new();
    let mut at = 0usize;

    for para in text.split("\n\n") {
        let chars = para.chars().count();
        if para.to_lowercase().contains(&folded) {
            hits.push(format!("[at character {at}]\n{}", para.trim_end()));
        }
        // +2 for the blank line the split consumed.
        at += chars + 2;
    }

    if hits.is_empty() {
        let total = text.chars().count();
        return format!(
            "[no paragraph contains {needle:?} in {total} characters. \
Omit find and read with offset/limit to page through the page instead.]"
        );
    }

    let count = hits.len();
    let joined = hits.join("\n\n");
    let body = clip_note(&joined, MAX_MARKDOWN_CHARS);
    format!("[{count} matching paragraph(s) for {needle:?}]\n\n{body}")
}

/// A footer stating how much of a list was seen and how to see the rest. An
/// agent that cannot tell "all of them" from "the first hundred" will draw
/// confident conclusions from partial data.
pub fn pagination_note(shown: usize, next_cursor: Option<&String>) -> String {
    match next_cursor {
        Some(cursor) => format!(
            "\n{shown} shown, and there are more. Pass start_cursor to continue:\n  {cursor}\n"
        ),
        None => format!("\n{shown} shown; that is all of them.\n"),
    }
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

pub fn args_of(args_json: &str) -> Result<Value, String> {
    serde_json::from_str(args_json).map_err(|e| format!("arguments were not valid JSON: {e}"))
}

/// A required string argument, rejecting one that is present but blank.
pub fn required_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("missing required argument '{key}'"))
}

pub fn optional_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A required id argument, accepting any of the forms `normalize_id` takes.
pub fn required_id(args: &Value, key: &str) -> Result<String, String> {
    let raw = required_str(args, key)?;
    normalize_id(&raw, key)
}

pub fn optional_id(args: &Value, key: &str) -> Result<Option<String>, String> {
    match optional_str(args, key) {
        Some(raw) => normalize_id(&raw, key).map(Some),
        None => Ok(None),
    }
}

pub fn page_size(args: &Value, default: u64) -> usize {
    args.get("page_size")
        .and_then(Value::as_u64)
        .unwrap_or(default)
        .clamp(1, 100) as usize
}

/// How many results to gather across pages. Bounded, because an unbounded walk
/// of a large data source would blow the context window and the rate limit.
pub fn limit(args: &Value, default: u64, max: u64) -> usize {
    args.get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(default)
        .clamp(1, max) as usize
}

/// Turns a JSON value into a query-string scalar, skipping anything structured.
fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn names(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// A float without a trailing `.0`, so counts read as counts.
fn trim_float(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// Truncates on a character boundary. Never slices a multi-byte char in half.
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}...")
}

/// Like `clip`, but says how much was dropped.
fn clip_note(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!(
        "{kept}\n\n[cut here: {max} of {total} characters shown]",
    )
}

// ---------------------------------------------------------------------------
// Writing properties
//
// Notion's write format wraps every value in its own type: a select is
// `{"select": {"name": "Done"}}`, a date is `{"date": {"start": "..."}}`, and
// a number is `{"number": 3}`. A caller working from the page it just read will
// naturally write `{"Status": "Done"}`, which the API rejects with a bare
// validation_error naming no property.
//
// So these tools fetch the parent data source's schema and coerce plain values
// into the shape each property actually needs. A value that is already wrapped
// is passed through untouched, which keeps the full API reachable for anything
// the coercion does not cover.
// ---------------------------------------------------------------------------

/// Property name -> Notion property type, from a data source's schema.
pub type Schema = BTreeMap<String, String>;

/// Reads a data source's property schema.
pub fn fetch_schema(client: &Notion, data_source_id: &str) -> Result<Schema, String> {
    let source = client.get(&format!("/v1/data_sources/{data_source_id}"), &[])?;
    Ok(schema_of(&source))
}

/// Extracts the name -> type map from a data source or database object.
pub fn schema_of(object: &Value) -> Schema {
    let mut schema = Schema::new();
    if let Some(props) = object.get("properties").and_then(Value::as_object) {
        for (name, spec) in props {
            if let Some(kind) = spec.get("type").and_then(Value::as_str) {
                schema.insert(name.clone(), kind.to_string());
            }
        }
    }
    schema
}

/// Property types the API refuses to accept on write, because Notion computes
/// them. Naming them explicitly turns a puzzling 400 into a clear refusal.
const COMPUTED: &[&str] = &[
    "formula",
    "rollup",
    "created_by",
    "created_time",
    "last_edited_by",
    "last_edited_time",
    "unique_id",
];

/// Coerces a map of plain values into Notion's tagged write format.
///
/// `schema` may be empty, in which case only values that are already wrapped,
/// plus bare strings (treated as title text), can be handled — that is the
/// situation for a page whose parent is another page, where `title` is the only
/// writable property anyway.
pub fn coerce_properties(input: &Value, schema: &Schema) -> Result<Value, String> {
    let Some(fields) = input.as_object() else {
        return Err("'properties' must be a JSON object of property name to value".to_string());
    };

    let mut out = serde_json::Map::new();
    let mut unknown: Vec<String> = Vec::new();

    for (name, value) in fields {
        let declared = schema.get(name).map(String::as_str);

        if let Some(kind) = declared {
            if COMPUTED.contains(&kind) {
                return Err(format!(
                    "'{name}' is a {kind} property, which Notion computes and the API cannot \
                     write. Remove it from 'properties'."
                ));
            }
        }

        // Already in write format: `{"select": {...}}` or an explicit
        // `{"type": "select", ...}`. Pass it through rather than second-
        // guessing a caller who knows the API.
        if let Some(object) = value.as_object() {
            let tagged = object
                .keys()
                .any(|k| k == "type" || Some(k.as_str()) == declared || is_property_key(k));
            if tagged {
                out.insert(name.clone(), value.clone());
                continue;
            }
        }

        match declared {
            Some(kind) => {
                out.insert(name.clone(), wrap(kind, value, name)?);
            }
            None => {
                // No schema entry. If the caller gave a string and we have no
                // schema at all, it is almost certainly the title.
                if schema.is_empty() && value.is_string() {
                    out.insert(name.clone(), wrap("title", value, name)?);
                } else {
                    unknown.push(name.clone());
                }
            }
        }
    }

    if !unknown.is_empty() {
        let mut known: Vec<String> = schema
            .iter()
            .filter(|(_, kind)| !COMPUTED.contains(&kind.as_str()))
            .map(|(name, kind)| format!("{name} ({kind})"))
            .collect();
        known.sort();
        return Err(format!(
            "this data source has no propert{} named {}. Writable properties are: {}.\n\n\
             Property names are case- and space-sensitive. Use notion-database-schema to see \
             the exact schema.",
            if unknown.len() == 1 { "y" } else { "ies" },
            unknown
                .iter()
                .map(|u| format!("{u:?}"))
                .collect::<Vec<_>>()
                .join(", "),
            if known.is_empty() {
                "(none)".to_string()
            } else {
                known.join(", ")
            }
        ));
    }

    Ok(Value::Object(out))
}

/// Whether a key is one of Notion's property type tags, which is how an
/// already-wrapped value is recognised without a schema.
fn is_property_key(key: &str) -> bool {
    matches!(
        key,
        "title"
            | "rich_text"
            | "number"
            | "select"
            | "multi_select"
            | "status"
            | "date"
            | "people"
            | "files"
            | "checkbox"
            | "url"
            | "email"
            | "phone_number"
            | "relation"
    )
}

/// Wraps one plain value for one property type.
fn wrap(kind: &str, value: &Value, name: &str) -> Result<Value, String> {
    // An explicit null clears a property, whatever its type. Notion has no
    // empty string, so this is the documented way to unset a value.
    if value.is_null() {
        return Ok(json!({ kind: Value::Null }));
    }

    let text = || value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string());

    Ok(match kind {
        "title" | "rich_text" => json!({ kind: [{ "text": { "content": text() } }] }),
        "number" => {
            let number = value
                .as_f64()
                // A number that arrived as a string is a very common shape from
                // a model; parse it rather than failing on a formality.
                .or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
                .ok_or_else(|| format!("'{name}' is a number property; {value} is not a number"))?;
            json!({ "number": number })
        }
        "checkbox" => {
            let flag = value
                .as_bool()
                .or_else(|| match value.as_str().map(str::trim) {
                    Some("true") | Some("yes") => Some(true),
                    Some("false") | Some("no") => Some(false),
                    _ => None,
                })
                .ok_or_else(|| {
                    format!("'{name}' is a checkbox property; {value} is not true or false")
                })?;
            json!({ "checkbox": flag })
        }
        "select" | "status" => json!({ kind: { "name": text() } }),
        "multi_select" => {
            let options = string_list(value, name, kind)?;
            json!({ "multi_select": options.iter().map(|o| json!({ "name": o })).collect::<Vec<_>>() })
        }
        "date" => match value.as_str() {
            Some(single) => json!({ "date": { "start": single } }),
            None => json!({ "date": value.clone() }),
        },
        "people" => {
            let ids = string_list(value, name, kind)?;
            json!({ "people": ids.iter().map(|id| json!({ "object": "user", "id": id })).collect::<Vec<_>>() })
        }
        "relation" => {
            let ids = string_list(value, name, kind)?;
            let mut wrapped = Vec::new();
            for id in &ids {
                wrapped.push(json!({ "id": normalize_id(id, "related page id")? }));
            }
            json!({ "relation": wrapped })
        }
        "url" | "email" | "phone_number" => json!({ kind: text() }),
        "files" => json!({ "files": value.clone() }),
        // Unrecognised type: hand it to Notion as-is under its own tag and let
        // the API be the authority. Better than refusing something valid.
        other => json!({ other: value.clone() }),
    })
}

/// A list of strings from either an array or a single value, so a caller may
/// write one tag without wrapping it in a list.
fn string_list(value: &Value, name: &str, kind: &str) -> Result<Vec<String>, String> {
    match value {
        Value::String(s) => Ok(vec![s.clone()]),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .or_else(|| item.get("name").and_then(Value::as_str).map(str::to_string))
                    .or_else(|| item.get("id").and_then(Value::as_str).map(str::to_string))
                    .ok_or_else(|| {
                        format!("'{name}' is a {kind} property; {item} is not a name or id")
                    })
            })
            .collect(),
        other => Err(format!(
            "'{name}' is a {kind} property; expected a string or a list of strings, got {other}"
        )),
    }
}

/// Builds an icon object from an emoji character or an image URL.
pub fn icon_value(raw: &str) -> Value {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        json!({ "type": "external", "external": { "url": raw } })
    } else {
        json!({ "type": "emoji", "emoji": raw })
    }
}

#[cfg(test)]
mod url_tests {
    use super::shorten_signed_urls;

    // A real signed URL from the workspace, cut down but structurally intact.
    const SIGNED: &str = "https://prod-files-secure.s3.us-west-2.amazonaws.com/1387c3a9/c73cc784/image.png?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=ASIAZI2LB4666FHT6IY6%2F20260826%2Fus-west-2%2Fs3%2Faws4_request&X-Amz-Signature=0a399cdebf247ea5cb3249020ba906650b0fc7af4431fa2a66dfa83360e6dc2b&X-Amz-SignedHeaders=host";

    #[test]
    fn a_signed_notion_file_url_loses_its_signature_but_keeps_its_name() {
        let out = shorten_signed_urls(&format!("![]({SIGNED})"));

        assert!(out.contains("image.png"), "the filename should survive: {out}");
        assert!(!out.contains("X-Amz-Signature"), "the signature should be gone: {out}");
        assert!(out.ends_with(")"), "the markdown should still close: {out}");
        // The point is the saving, not an absolute size: the signature is the
        // bulk of the URL, so the result should be a fraction of the input.
        let before = format!("![]({SIGNED})").len();
        assert!(
            out.len() * 2 < before,
            "expected at least half saved, went from {before} to {}",
            out.len()
        );
    }

    #[test]
    fn an_ordinary_url_keeps_its_query_string() {
        // A query string elsewhere may be load-bearing, so it is left alone.
        let text = "see https://example.com/search?q=notion&page=2 for more";
        assert_eq!(shorten_signed_urls(text), text);
    }

    #[test]
    fn a_short_query_on_a_notion_host_is_left_alone() {
        let text = "https://prod-files-secure.s3.us-west-2.amazonaws.com/a/b.png?v=2";
        assert_eq!(shorten_signed_urls(text), text);
    }

    #[test]
    fn text_with_no_urls_is_unchanged() {
        let text = "# Heading\n\nSome prose with (parens) and \"quotes\".";
        assert_eq!(shorten_signed_urls(text), text);
    }

    #[test]
    fn several_signed_urls_are_all_shortened() {
        let out = shorten_signed_urls(&format!("![one]({SIGNED})\n\n![two]({SIGNED})"));
        assert_eq!(out.matches("[signature removed]").count(), 2, "{out}");
    }
}
