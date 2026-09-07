//! Search a Notion workspace by title.
//!
//! `POST /v1/search`. This is the discovery tool for the group: nothing else
//! here can find an id on its own, so a session usually starts with this.
//!
//! The search matches *titles only*, not page bodies — a fact worth stating in
//! the output, because a model that assumes full-text search will conclude that
//! content does not exist when it simply was not searched.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod notion;

use notion::Notion;
use serde_json::{json, Value};

const DEFAULT_LIMIT: u64 = 20;
const MAX_LIMIT: u64 = 100;

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "notion-search".to_string(),
            description: "Search a Notion workspace for pages and databases whose TITLE matches a \
                          query. Titles only — this does not search page content. Omit the query \
                          to list everything the connection can see. Start here when you need an \
                          id: every other notion tool takes ids, and this is how you find them."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Text to match against titles. Omit to list everything \
                                        shared with the connection."
                    },
                    "filter": {
                        "type": "string",
                        "enum": ["page", "data_source"],
                        "description": "Return only pages, or only data sources (the queryable \
                                        tables inside a database). Omit for both."
                    },
                    "sort": {
                        "type": "string",
                        "enum": ["relevance", "last_edited_ascending", "last_edited_descending"],
                        "description": "Result order. Defaults to relevance."
                    },
                    "in_trash": {
                        "type": "boolean",
                        "description": "Search the trash instead of live content. Defaults to false."
                    },
                    "limit": {
                        "type": "integer",
                        "description": format!("Maximum results to gather, 1-{MAX_LIMIT}. \
                                                Defaults to {DEFAULT_LIMIT}.")
                    },
                    "start_cursor": {
                        "type": "string",
                        "description": "Resume from a cursor returned by an earlier call."
                    }
                },
                "additionalProperties": false
            })
            .to_string(),
            // Searching only reads, so this survives a read-only mode.
            capabilities: vec!["http".to_string(), "read-only".to_string()],
        }
    }

    fn invoke(_session: String, args_json: String, config_json: String) -> Result<String, String> {
        let args = notion::args_of(&args_json)?;
        let client = Notion::from_config(&config_json)?;

        let query = notion::optional_str(&args, "query");
        let want = notion::limit(&args, DEFAULT_LIMIT, MAX_LIMIT);

        let mut body = json!({});
        if let Some(query) = &query {
            body["query"] = json!(query);
        }
        if let Some(cursor) = notion::optional_str(&args, "start_cursor") {
            body["start_cursor"] = json!(cursor);
        }

        // Notion takes the object filter and the trash flag in one object, and
        // rejects unknown keys, so it is assembled rather than merged.
        let object = notion::optional_str(&args, "filter");
        let in_trash = args.get("in_trash").and_then(Value::as_bool);
        match (&object, in_trash) {
            (Some(value), trash) => {
                let mut filter = json!({ "property": "object", "value": value });
                if let Some(trash) = trash {
                    filter["in_trash"] = json!(trash);
                }
                body["filter"] = filter;
            }
            (None, Some(trash)) => body["filter"] = json!({ "in_trash": trash }),
            (None, None) => {}
        }

        match notion::optional_str(&args, "sort").as_deref() {
            Some("last_edited_ascending") => {
                body["sort"] = json!({ "timestamp": "last_edited_time", "direction": "ascending" });
            }
            Some("last_edited_descending") => {
                body["sort"] =
                    json!({ "timestamp": "last_edited_time", "direction": "descending" });
            }
            Some("relevance") => body["sort"] = json!({ "property": "relevance" }),
            _ => {}
        }

        let (results, next) = client.paginate("POST", "/v1/search", &body, want)?;

        Ok(format(&results, next.as_ref(), query.as_deref(), &object))
    }
}

fn format(
    results: &[Value],
    next: Option<&String>,
    query: Option<&str>,
    filter: &Option<String>,
) -> String {
    let subject = match query {
        Some(q) => format!("{q:?}"),
        None => "everything shared with this connection".to_string(),
    };

    if results.is_empty() {
        return format!(
            "No Notion pages or databases match {subject}.\n\n\
             Search matches titles only, not page content, so try a shorter or different title \
             fragment. If you expected a result, the page may not be shared with this \
             connection: open it in Notion, use the ••• menu -> Connections, and add yours."
        );
    }

    let scope = match filter.as_deref() {
        Some("page") => " (pages only)",
        Some("data_source") => " (data sources only)",
        _ => "",
    };

    let mut out = format!(
        "{} result(s) for {subject}{scope}. Titles matched; page content was not searched.\n",
        results.len()
    );

    for (i, object) in results.iter().enumerate() {
        out.push_str(&format!("\n{}. {}\n", i + 1, notion::object_line(object)));

        // A data source id is what notion-database-query needs, and it is not
        // the same as the database id it belongs to — so name both.
        if object.get("object").and_then(Value::as_str) == Some("data_source") {
            if let Some(line) = notion::parent_line(object) {
                out.push_str(&format!("   {line}\n"));
                out.push_str("   query this id with notion-database-query\n");
            }
        }
    }

    out.push_str(&notion::pagination_note(results.len(), next));
    out
}

export!(Component);
