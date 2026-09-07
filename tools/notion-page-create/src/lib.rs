//! Create a Notion page, under another page or as a row in a database.
//!
//! `POST /v1/pages`. Content is given as markdown, which the API accepts
//! directly — far easier to produce correctly than a block tree.
//!
//! The parent decides what `properties` may contain. Under a page, only the
//! title is valid. Under a data source, the keys must match that source's
//! schema, so this tool fetches the schema and coerces plain values, the same
//! way notion-page-update does.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod notion;

use notion::Notion;
use serde_json::{json, Value};

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "notion-page-create".to_string(),
            description: "Create a Notion page — either a sub-page of an existing page, or a new \
                          row in a database (give its data source id). Body content is written as \
                          markdown. Property values may be given plainly and are coerced to the \
                          parent's schema."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "parent_page_id": {
                        "type": "string",
                        "description": "Create the page as a child of this page. Give this or \
                                        parent_data_source_id, not both."
                    },
                    "parent_data_source_id": {
                        "type": "string",
                        "description": "Create the page as a row in this data source (a database \
                                        table). Find it with notion-database-list or \
                                        notion-search. A database id is accepted too and its \
                                        first data source is used."
                    },
                    "title": {
                        "type": "string",
                        "description": "The page title. Under a data source this fills whichever \
                                        property is the title property."
                    },
                    "content": {
                        "type": "string",
                        "description": "Page body as markdown. Headings, lists, tables, code \
                                        fences and to-dos all convert to real Notion blocks."
                    },
                    "properties": {
                        "type": "object",
                        "description": "Property name to value, for a page in a database. Plain \
                                        values are coerced to the schema; see \
                                        notion-database-schema for the exact property names.",
                        "additionalProperties": true
                    },
                    "icon": {
                        "type": "string",
                        "description": "An emoji character, or an image URL."
                    }
                },
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["http".to_string()],
        }
    }

    fn invoke(_session: String, args_json: String, config_json: String) -> Result<String, String> {
        let args = notion::args_of(&args_json)?;
        let client = Notion::from_config(&config_json)?;

        let parent_page = notion::optional_id(&args, "parent_page_id")?;
        let parent_source = notion::optional_id(&args, "parent_data_source_id")?;

        if parent_page.is_some() && parent_source.is_some() {
            return Err(
                "give either parent_page_id or parent_data_source_id, not both: a page has one \
                 parent."
                    .to_string(),
            );
        }

        let mut body = json!({});
        let mut schema = notion::Schema::new();
        let mut where_to = String::from("the workspace root (a private page)");

        match (&parent_page, &parent_source) {
            (Some(page_id), _) => {
                body["parent"] = json!({ "type": "page_id", "page_id": page_id });
                where_to = format!("page {page_id}");
            }
            (_, Some(given)) => {
                // An id that is really a database, not a data source, is the
                // most common mix-up since 2025-09-03 split the two. Resolve it
                // rather than returning a validation error.
                let source_id = resolve_data_source(&client, given)?;
                body["parent"] =
                    json!({ "type": "data_source_id", "data_source_id": source_id });
                schema = notion::fetch_schema(&client, &source_id)?;
                where_to = format!("data source {source_id}");
            }
            // No parent at all creates a private top-level page, which a
            // personal access token is allowed to do.
            (None, None) => {}
        }

        let mut properties = serde_json::Map::new();
        if let Some(input) = args.get("properties") {
            if !input.is_null() {
                if let Some(object) = notion::coerce_properties(input, &schema)?.as_object() {
                    for (k, v) in object {
                        properties.insert(k.clone(), v.clone());
                    }
                }
            }
        }

        if let Some(title) = notion::optional_str(&args, "title") {
            let key = title_key(&schema);
            properties.insert(key, json!({ "title": [{ "text": { "content": title } }] }));
        } else if properties.is_empty() {
            return Err(
                "give a 'title', or 'properties' including the title property: a page with no \
                 title at all is almost never intended."
                    .to_string(),
            );
        }

        body["properties"] = Value::Object(properties);

        if let Some(content) = notion::optional_str(&args, "content") {
            body["markdown"] = json!(content);
        }
        if let Some(icon) = notion::optional_str(&args, "icon") {
            body["icon"] = notion::icon_value(&icon);
        }

        let created = client.post("/v1/pages", &body)?;

        let mut out = format!(
            "Created \"{}\" in {where_to}.\n",
            notion::title_of(&created)
        );
        if let Some(id) = created.get("id").and_then(Value::as_str) {
            out.push_str(&format!("id: {id}\n"));
        }
        if let Some(url) = created.get("url").and_then(Value::as_str) {
            out.push_str(&format!("url: {url}\n"));
        }
        let props = notion::describe_properties(&created, "  ");
        if !props.trim().is_empty() {
            out.push_str("\nproperties:\n");
            out.push_str(&props);
        }
        Ok(out)
    }
}

/// Accepts either a data source id or a database id, returning a data source id.
///
/// Since API version 2025-09-03 a database holds one or more data sources, and
/// only a data source can parent a page or answer a query. The two ids look
/// identical, so being handed the wrong one is routine.
fn resolve_data_source(client: &Notion, id: &str) -> Result<String, String> {
    if client.get(&format!("/v1/data_sources/{id}"), &[]).is_ok() {
        return Ok(id.to_string());
    }

    let database = client.get(&format!("/v1/databases/{id}"), &[]).map_err(|e| {
        format!("{id} is neither a data source nor a database this connection can see.\n\n{e}")
    })?;

    let sources = database
        .get("data_sources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    match sources.len() {
        0 => Err(format!("database {id} has no data sources to add a page to.")),
        1 => Ok(sources[0]
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()),
        // Choosing for the caller here would silently put the row in the wrong
        // table, so ask.
        _ => Err(format!(
            "{id} is a database with {} data sources; say which one to use:\n{}",
            sources.len(),
            sources
                .iter()
                .map(|s| format!(
                    "  {} — {}",
                    s.get("id").and_then(Value::as_str).unwrap_or("?"),
                    s.get("name").and_then(Value::as_str).unwrap_or("(unnamed)")
                ))
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
}

/// The schema's title property name, or plain "title" when there is no schema
/// (a page parented by another page).
fn title_key(schema: &notion::Schema) -> String {
    schema
        .iter()
        .find(|(_, kind)| kind.as_str() == "title")
        .map(|(name, _)| name.clone())
        .unwrap_or_else(|| "title".to_string())
}

export!(Component);
