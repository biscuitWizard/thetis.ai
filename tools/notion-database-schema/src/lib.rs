//! Read a Notion data source's schema: its columns, their types, and their
//! allowed values.
//!
//! `GET /v1/data_sources/{id}`. This is the tool that makes writing to a
//! database work. Notion rejects a property whose name differs by a character
//! or whose value is not one of the configured select options, and the error
//! does not say what the right ones were — so knowing the schema before writing
//! turns a guess into a lookup.
//!
//! Select, status and multi-select options are listed in full, because those are
//! exactly the values a caller has to match and cannot infer.

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
            name: "notion-database-schema".to_string(),
            description: "Show a Notion database's schema: every property, its type, and the \
                          allowed options for select, status and multi-select properties. Read \
                          this before writing to a database or building a query filter — \
                          property names and option values must match exactly."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "data_source_id": {
                        "type": "string",
                        "description": "The data source whose schema to read. A database id is \
                                        accepted and its data sources are listed."
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
        let id = notion::required_id(&args, "data_source_id")?;

        match client.get(&format!("/v1/data_sources/{id}"), &[]) {
            Ok(source) => Ok(render(&source)),
            Err(first) => {
                // Perhaps it is a database id. Listing its data sources is more
                // use than repeating a 404.
                match client.get(&format!("/v1/databases/{id}"), &[]) {
                    Ok(database) => Ok(render_database(&database, &id)),
                    Err(_) => Err(first),
                }
            }
        }
    }
}

fn render(source: &Value) -> String {
    let mut out = format!("# {} (data source)\n", notion::title_of(source));

    if let Some(id) = source.get("id").and_then(Value::as_str) {
        out.push_str(&format!("data_source_id: {id}\n"));
    }
    if let Some(parent) = source.get("parent") {
        if let Some(database_id) = parent.get("database_id").and_then(Value::as_str) {
            out.push_str(&format!("in database: {database_id}\n"));
        }
    }
    if let Some(description) = source.get("description") {
        let text = notion::rich_text(description);
        if !text.trim().is_empty() {
            out.push_str(&format!("description: {text}\n"));
        }
    }

    let Some(properties) = source.get("properties").and_then(Value::as_object) else {
        out.push_str("\nThis data source reports no properties.\n");
        return out;
    };

    let mut names: Vec<&String> = properties.keys().collect();
    names.sort();

    out.push_str(&format!("\n{} propert(ies):\n", names.len()));

    for name in names {
        let spec = &properties[name];
        let kind = spec.get("type").and_then(Value::as_str).unwrap_or("?");
        out.push_str(&format!("\n- {name} — {kind}"));
        if let Some(id) = spec.get("id").and_then(Value::as_str) {
            out.push_str(&format!(" (id {id})"));
        }
        out.push('\n');

        match kind {
            "select" | "status" | "multi_select" => {
                let options = options_of(spec, kind);
                if options.is_empty() {
                    out.push_str("  no options configured yet\n");
                } else {
                    out.push_str(&format!("  options: {}\n", options.join(" | ")));
                }
            }
            "number" => {
                if let Some(format) = spec
                    .get("number")
                    .and_then(|n| n.get("format"))
                    .and_then(Value::as_str)
                {
                    out.push_str(&format!("  format: {format}\n"));
                }
            }
            "formula" => {
                if let Some(expression) = spec
                    .get("formula")
                    .and_then(|f| f.get("expression"))
                    .and_then(Value::as_str)
                {
                    out.push_str(&format!(
                        "  expression: {}\n",
                        notion::clip(expression, 200)
                    ));
                }
                out.push_str("  computed by Notion; cannot be written\n");
            }
            "relation" => {
                let relation = spec.get("relation");
                if let Some(target) = relation
                    .and_then(|r| r.get("data_source_id"))
                    .and_then(Value::as_str)
                {
                    out.push_str(&format!("  points at data source: {target}\n"));
                }
                out.push_str("  write with a list of page ids\n");
            }
            "rollup" => {
                out.push_str("  computed by Notion; cannot be written\n");
            }
            "title" => out.push_str("  this is the page title\n"),
            "created_time" | "created_by" | "last_edited_time" | "last_edited_by"
            | "unique_id" => {
                out.push_str("  maintained by Notion; cannot be written\n");
            }
            _ => {}
        }
    }

    out.push_str(
        "\nWhen writing these with notion-page-create or notion-page-update, plain values are \
         accepted: a string for text/select/url, a number for number, true/false for checkbox, \
         an ISO date for date, a list of option names for multi_select, a list of page ids for \
         relation.\n",
    );
    out
}

fn options_of(spec: &Value, kind: &str) -> Vec<String> {
    spec.get(kind)
        .and_then(|inner| inner.get(if kind == "status" { "options" } else { "options" }))
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|o| o.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// A database is a container: it has no schema of its own, its data sources do.
fn render_database(database: &Value, id: &str) -> String {
    let sources = database
        .get("data_sources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut out = format!(
        "{id} is a database, not a data source: \"{}\".\n\n\
         A database holds one or more data sources, and the schema belongs to the data source. \
         Call this tool again with one of these ids:\n",
        notion::title_of(database)
    );

    if sources.is_empty() {
        out.push_str("  (this database reports no data sources)\n");
        return out;
    }
    for source in &sources {
        out.push_str(&format!(
            "  {} — {}\n",
            source.get("id").and_then(Value::as_str).unwrap_or("?"),
            source
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("(unnamed)")
        ));
    }
    out
}

export!(Component);
