//! Query a Notion data source: the rows of a database, filtered and sorted.
//!
//! `POST /v1/data_sources/{id}/query`. Results come back as a table of
//! flattened property values rather than raw JSON — a query of thirty rows in
//! Notion's wire format is tens of thousands of tokens of type wrappers, and
//! the same information fits in a few hundred as lines.
//!
//! Filters are passed through as Notion's own filter objects. They are a small
//! language of their own and reproducing it in a flat schema would only limit
//! what can be asked, so the schema documents the shape and the API validates it.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod notion;

use notion::Notion;
use serde_json::{json, Value};

const DEFAULT_LIMIT: u64 = 25;
const MAX_LIMIT: u64 = 200;

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "notion-database-query".to_string(),
            description: "Query the rows of a Notion database, with optional filters and sorts. \
                          Takes a data source id (from notion-database-list). Returns each row's \
                          id, title and property values. Check the schema first with \
                          notion-database-schema so filter property names and types are right."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "data_source_id": {
                        "type": "string",
                        "description": "The data source to query. A database id is accepted and \
                                        resolved when the database has just one data source."
                    },
                    "filter": {
                        "type": "object",
                        "description": "A Notion filter object, e.g. \
                                        {\"property\":\"Status\",\"status\":{\"equals\":\"Done\"}}. \
                                        Combine with {\"and\":[...]} or {\"or\":[...]}. The inner \
                                        key must match the property's type. Omit for all rows.",
                        "additionalProperties": true
                    },
                    "sorts": {
                        "type": "array",
                        "description": "Sort order, most significant first, e.g. \
                                        [{\"property\":\"Due\",\"direction\":\"ascending\"}] or \
                                        [{\"timestamp\":\"last_edited_time\",\"direction\":\"descending\"}].",
                        "items": { "type": "object", "additionalProperties": true }
                    },
                    "properties": {
                        "type": "array",
                        "description": "Only return these properties. Worth setting on a wide \
                                        database: it makes the query faster and the output much \
                                        shorter.",
                        "items": { "type": "string" }
                    },
                    "is_archived": {
                        "type": "boolean",
                        "description": "Query archived rows instead of live ones. Defaults to false."
                    },
                    "limit": {
                        "type": "integer",
                        "description": format!("Maximum rows to gather across pages, 1-{MAX_LIMIT}. \
                                                Defaults to {DEFAULT_LIMIT}.")
                    },
                    "start_cursor": {
                        "type": "string",
                        "description": "Resume from a cursor returned by an earlier call."
                    }
                },
                "required": ["data_source_id"],
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["http".to_string(), "read-only".to_string()],
        }
    }

    fn invoke(_session: String, args_json: String, config_json: String) -> Result<String, String> {
        let args = notion::args_of(&args_json)?;
        let client = Notion::from_config(&config_json)?;
        let given = notion::required_id(&args, "data_source_id")?;
        let want = notion::limit(&args, DEFAULT_LIMIT, MAX_LIMIT);

        let mut body = json!({});
        if let Some(filter) = args.get("filter") {
            if !filter.is_null() {
                body["filter"] = filter.clone();
            }
        }
        if let Some(sorts) = args.get("sorts").and_then(Value::as_array) {
            if !sorts.is_empty() {
                body["sorts"] = json!(sorts);
            }
        }
        if args.get("is_archived").and_then(Value::as_bool) == Some(true) {
            body["is_archived"] = json!(true);
        }
        if let Some(cursor) = notion::optional_str(&args, "start_cursor") {
            body["start_cursor"] = json!(cursor);
        }

        // filter_properties is a repeated query parameter, not a body field, so
        // it is appended to the path.
        let wanted: Vec<String> = args
            .get("properties")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        let mut path = format!("/v1/data_sources/{given}/query");
        if !wanted.is_empty() {
            let encoded: Vec<String> = wanted
                .iter()
                .map(|p| format!("filter_properties[]={}", urlencode(p)))
                .collect();
            path.push('?');
            path.push_str(&encoded.join("&"));
        }

        let (rows, next) = match client.paginate("POST", &path, &body, want) {
            Ok(found) => found,
            Err(first) => {
                // Being handed a database id instead of a data source id is the
                // most common failure here, so try to recover from it before
                // reporting anything.
                match resolve(&client, &given) {
                    Some(source_id) if source_id != given => {
                        let retry = path.replace(&given, &source_id);
                        client.paginate("POST", &retry, &body, want)?
                    }
                    _ => return Err(first),
                }
            }
        };

        Ok(format(&rows, next.as_ref(), &given))
    }
}

fn format(rows: &[Value], next: Option<&String>, source: &str) -> String {
    if rows.is_empty() {
        return format!(
            "No rows in data source {source} match this query.\n\n\
             An empty result is not an error: the filter may simply match nothing. If a filter \
             was given, check the property names and type keys against notion-database-schema — \
             a filter naming a property that does not exist is rejected, but one with the wrong \
             expectation quietly matches nothing."
        );
    }

    let mut out = format!("{} row(s) from data source {source}.\n", rows.len());

    for (i, row) in rows.iter().enumerate() {
        out.push_str(&format!(
            "\n{}. {}\n",
            i + 1,
            notion::title_of(row)
        ));
        if let Some(id) = row.get("id").and_then(Value::as_str) {
            out.push_str(&format!("   id: {id}\n"));
        }

        let properties = notion::describe_properties(row, "   ");
        // The title is already the heading; repeating it as a property doubles
        // every entry for no gain.
        for line in properties.lines() {
            if line.contains("(title):") {
                continue;
            }
            out.push_str(&format!("{line}\n"));
        }
    }

    out.push_str(&notion::pagination_note(rows.len(), next));
    out
}

/// A data source id for something that might be a database id.
fn resolve(client: &Notion, id: &str) -> Option<String> {
    let database = client.get(&format!("/v1/databases/{id}"), &[]).ok()?;
    let sources = database.get("data_sources")?.as_array()?;
    if sources.len() == 1 {
        return sources[0]
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    None
}

/// Percent-encodes a query parameter value. Property names hold spaces and
/// punctuation often enough that this is not optional.
fn urlencode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

export!(Component);
