//! List the databases a Notion connection can see, with their data source ids.
//!
//! There is no "list all databases" endpoint; this is `POST /v1/search` with an
//! object filter. The reason it exists as its own tool is the id problem: since
//! API version 2025-09-03 a *database* holds one or more *data sources*, and
//! almost every useful call — query, create a row, read the schema — needs the
//! data source id, not the database id. They look identical, so the pairing has
//! to be shown explicitly or it gets guessed wrong.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod notion;

use notion::Notion;
use serde_json::{json, Value};

const DEFAULT_LIMIT: u64 = 25;
const MAX_LIMIT: u64 = 100;

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "notion-database-list".to_string(),
            description: "List the Notion databases this connection can see, each with the data \
                          source id needed to query it or add rows to it. Optionally filter by \
                          title. Use this to find what is available before querying."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Only databases whose title matches this text. Omit for all."
                    },
                    "limit": {
                        "type": "integer",
                        "description": format!("Maximum databases to list, 1-{MAX_LIMIT}. \
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
            capabilities: vec!["http".to_string(), "read-only".to_string()],
        }
    }

    fn invoke(_session: String, args_json: String, config_json: String) -> Result<String, String> {
        let args = notion::args_of(&args_json)?;
        let client = Notion::from_config(&config_json)?;

        let query = notion::optional_str(&args, "query");
        let want = notion::limit(&args, DEFAULT_LIMIT, MAX_LIMIT);

        // Searching for data sources rather than databases: the data source is
        // the queryable thing, and each one names its parent database.
        let mut body = json!({ "filter": { "property": "object", "value": "data_source" } });
        if let Some(query) = &query {
            body["query"] = json!(query);
        }
        if let Some(cursor) = notion::optional_str(&args, "start_cursor") {
            body["start_cursor"] = json!(cursor);
        }

        let (results, next) = client.paginate("POST", "/v1/search", &body, want)?;

        if results.is_empty() {
            return Ok(match &query {
                Some(q) => format!(
                    "No databases match {q:?}.\n\n\
                     This matches database titles only. If you expected one, it may not be \
                     shared with this connection: open the database in Notion, use the ••• menu \
                     -> Connections, and add yours."
                ),
                None => "This connection can see no databases.\n\n\
                     Notion shares nothing by default. Open a database in Notion, use the ••• \
                     menu -> Connections, and add this connection to it."
                    .to_string(),
            });
        }

        let mut out = format!("{} data source(s) available.\n", results.len());
        out.push_str(
            "\nQuery a data source with notion-database-query, and add rows to it with \
             notion-page-create (parent_data_source_id). A database id is not interchangeable \
             with a data source id.\n",
        );

        for (i, source) in results.iter().enumerate() {
            let id = source.get("id").and_then(Value::as_str).unwrap_or("?");
            out.push_str(&format!(
                "\n{}. {}\n   data_source_id: {id}\n",
                i + 1,
                notion::title_of(source)
            ));

            if let Some(parent) = source.get("parent") {
                if let Some(database_id) = parent.get("database_id").and_then(Value::as_str) {
                    out.push_str(&format!("   in database: {database_id}\n"));
                }
            }
            if let Some(url) = source.get("url").and_then(Value::as_str) {
                out.push_str(&format!("   {url}\n"));
            }

            // The column names, so a caller can write a filter without a
            // second call for the schema.
            let schema = notion::schema_of(source);
            if !schema.is_empty() {
                let columns: Vec<String> = schema
                    .iter()
                    .map(|(name, kind)| format!("{name} ({kind})"))
                    .collect();
                out.push_str(&format!(
                    "   properties: {}\n",
                    notion::clip(&columns.join(", "), 400)
                ));
            }
        }

        out.push_str(&notion::pagination_note(results.len(), next.as_ref()));
        Ok(out)
    }
}

export!(Component);
