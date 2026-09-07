//! Mutate a Notion page's properties, icon, cover, or trash state.
//!
//! `PATCH /v1/pages/{id}`. Properties are the hard part: the API wants every
//! value wrapped in its own type tag, so this tool reads the parent data
//! source's schema first and coerces plain values into the right shape. That
//! means `{"Status": "Done", "Priority": 2}` works, and so does the fully
//! wrapped form for anything the coercion does not cover.
//!
//! Content is deliberately not here — see notion-page-content, which edits the
//! body as markdown. Properties and prose are different operations with
//! different failure modes, and one tool doing both invites a caller to replace
//! a page when it meant to tick a box.

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
            name: "notion-page-update".to_string(),
            description: "Change a Notion page's property values, icon, cover, lock or trash \
                          state. Property values may be given plainly — {\"Status\": \"Done\", \
                          \"Tags\": [\"a\",\"b\"]} — and are coerced to the schema of the \
                          database the page belongs to. Use notion-page-content to change the \
                          page body instead."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "page_id": {
                        "type": "string",
                        "description": "Page id, or the page URL copied from Notion."
                    },
                    "properties": {
                        "type": "object",
                        "description": "Property name to new value. Plain values are accepted \
                                        and coerced: a string for text/select/url, a number for \
                                        number, true/false for checkbox, an ISO date or \
                                        {start,end} for date, a list of names for multi_select, \
                                        a list of page ids for relation. null clears a property. \
                                        Notion's fully-wrapped form is passed through untouched. \
                                        Names are case-sensitive; formula, rollup and other \
                                        computed properties cannot be written.",
                        "additionalProperties": true
                    },
                    "title": {
                        "type": "string",
                        "description": "Convenience for setting the page title without knowing \
                                        which property holds it."
                    },
                    "icon": {
                        "type": "string",
                        "description": "An emoji character, or an image URL. Pass an empty string \
                                        to remove the icon."
                    },
                    "cover": {
                        "type": "string",
                        "description": "Cover image URL. Pass an empty string to remove it."
                    },
                    "in_trash": {
                        "type": "boolean",
                        "description": "true moves the page to the trash; false restores it. \
                                        Notion's trash is recoverable, so this is not a \
                                        permanent delete."
                    },
                    "is_locked": {
                        "type": "boolean",
                        "description": "Lock or unlock the page against edits in the Notion UI. \
                                        Does not affect API edits."
                    }
                },
                "required": ["page_id"],
                "additionalProperties": false
            })
            .to_string(),
            // No read-only capability: this writes, so a read-only mode
            // withholds it. That omission is the safety property, not an
            // oversight.
            capabilities: vec!["http".to_string()],
        }
    }

    fn invoke(_session: String, args_json: String, config_json: String) -> Result<String, String> {
        let args = notion::args_of(&args_json)?;
        let client = Notion::from_config(&config_json)?;
        let page_id = notion::required_id(&args, "page_id")?;

        let mut body = json!({});
        let mut changed: Vec<String> = Vec::new();

        // Properties need the parent's schema, which costs one extra request —
        // only made when there are properties to coerce.
        let mut properties = serde_json::Map::new();

        if let Some(input) = args.get("properties") {
            if !input.is_null() {
                let page = client.get(&format!("/v1/pages/{page_id}"), &[])?;
                let schema = schema_for(&client, &page)?;
                let coerced = notion::coerce_properties(input, &schema)?;
                if let Some(object) = coerced.as_object() {
                    for (k, v) in object {
                        properties.insert(k.clone(), v.clone());
                        changed.push(k.clone());
                    }
                }
            }
        }

        // `title` is a shortcut: find whichever property is the title rather
        // than making the caller look it up.
        if let Some(title) = notion::optional_str(&args, "title") {
            let page = client.get(&format!("/v1/pages/{page_id}"), &[])?;
            let key = title_property(&page).unwrap_or_else(|| "title".to_string());
            properties.insert(
                key.clone(),
                json!({ "title": [{ "text": { "content": title } }] }),
            );
            changed.push(format!("{key} (title)"));
        }

        if !properties.is_empty() {
            body["properties"] = Value::Object(properties);
        }

        if let Some(icon) = args.get("icon").and_then(Value::as_str) {
            body["icon"] = if icon.trim().is_empty() {
                Value::Null
            } else {
                notion::icon_value(icon.trim())
            };
            changed.push("icon".to_string());
        }
        if let Some(cover) = args.get("cover").and_then(Value::as_str) {
            body["cover"] = if cover.trim().is_empty() {
                Value::Null
            } else {
                json!({ "type": "external", "external": { "url": cover.trim() } })
            };
            changed.push("cover".to_string());
        }
        if let Some(trash) = args.get("in_trash").and_then(Value::as_bool) {
            body["in_trash"] = json!(trash);
            changed.push(if trash {
                "moved to trash".to_string()
            } else {
                "restored from trash".to_string()
            });
        }
        if let Some(locked) = args.get("is_locked").and_then(Value::as_bool) {
            body["is_locked"] = json!(locked);
            changed.push(if locked { "locked" } else { "unlocked" }.to_string());
        }

        if changed.is_empty() {
            return Err(
                "nothing to change. Give at least one of: properties, title, icon, cover, \
                 in_trash, is_locked."
                    .to_string(),
            );
        }

        let updated = client.patch(&format!("/v1/pages/{page_id}"), &body)?;

        let mut out = format!(
            "Updated \"{}\".\nchanged: {}\n",
            notion::title_of(&updated),
            changed.join(", ")
        );
        if let Some(url) = updated.get("url").and_then(Value::as_str) {
            out.push_str(&format!("url: {url}\n"));
        }
        let props = notion::describe_properties(&updated, "  ");
        if !props.trim().is_empty() {
            out.push_str("\nproperties now (non-empty only):\n");
            out.push_str(&props);
        }
        Ok(out)
    }
}

/// The schema of the data source a page belongs to, or an empty schema when the
/// page's parent is an ordinary page (where only the title is writable).
fn schema_for(client: &Notion, page: &Value) -> Result<notion::Schema, String> {
    let parent = page.get("parent").cloned().unwrap_or(Value::Null);
    let kind = parent.get("type").and_then(Value::as_str).unwrap_or("");

    match kind {
        "data_source_id" => {
            let id = parent
                .get("data_source_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            notion::fetch_schema(client, id)
        }
        // Older parents still report database_id. A database's first data
        // source carries the schema.
        "database_id" => {
            let id = parent
                .get("database_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let database = client.get(&format!("/v1/databases/{id}"), &[])?;
            match database
                .get("data_sources")
                .and_then(Value::as_array)
                .and_then(|sources| sources.first())
                .and_then(|source| source.get("id"))
                .and_then(Value::as_str)
            {
                Some(source_id) => notion::fetch_schema(client, source_id),
                None => Ok(notion::Schema::new()),
            }
        }
        _ => Ok(notion::Schema::new()),
    }
}

/// The name of whichever property holds the page title.
fn title_property(page: &Value) -> Option<String> {
    page.get("properties")?
        .as_object()?
        .iter()
        .find(|(_, value)| value.get("type").and_then(Value::as_str) == Some("title"))
        .map(|(name, _)| name.clone())
}

export!(Component);
