//! Read sample rows straight out of a BigQuery table, free of query charges.
//!
//! ## The point of this tool
//!
//! `SELECT * FROM t LIMIT 10` is what everyone reaches for to see what a table
//! holds, and on a columnar store it is a trap: BigQuery bills by bytes read
//! from the columns referenced, and `*` references all of them. On a wide table
//! that is a full scan, charged in full, to look at ten rows.
//!
//! `tabledata.list` reads the stored rows directly, does not go through the
//! query engine, and is **not billed**. It is strictly better for the "what
//! does this data look like" question, which is the question you ask first and
//! most often. `selected_fields` narrows it further.
//!
//! The limits are real and worth knowing: no ordering (you get rows in storage
//! order from `start_index`), no filtering, and no aggregation. For any of
//! those you need `bq-query` and you should expect to pay.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod bq;

use bq::{
    as_u64_loose, commas, decode_rows, parse_schema, render_rows, str_arg, u64_arg, Bq, Field,
};
use serde_json::{json, Value};

const DEFAULT_ROWS: u64 = 10;
const MAX_CELL: usize = 100;

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "bq-peek".to_string(),
            description: "Read sample rows straight out of a BigQuery table with zero query \
                cost, using tabledata.list instead of SELECT. Prefer this over \
                `SELECT * LIMIT 10` for finding out what data looks like: BigQuery bills \
                for every column a query touches, so `SELECT *` on a wide table is a full \
                scan charged in full, while this is free. It cannot filter, order or \
                aggregate — rows come in storage order — so use `bq-query` when you need \
                any of that. Reaches bigquery.googleapis.com."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "table": {
                        "type": "string",
                        "description": "The table, as 'project.dataset.table', 'dataset.table', or a bare name with `dataset` given separately."
                    },
                    "dataset": {
                        "type": "string",
                        "description": "Dataset, if not included in `table`."
                    },
                    "project": {
                        "type": "string",
                        "description": "GCP project. Defaults to the configured project."
                    },
                    "rows": {
                        "type": "integer",
                        "description": "How many rows to read. Default 10."
                    },
                    "start_index": {
                        "type": "integer",
                        "description": "Zero-based row to start at, for looking further into the table."
                    },
                    "selected_fields": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Only these columns, e.g. [\"user_id\", \"geo.country\"]. Strongly recommended on a wide table: it is the difference between a readable sample and a screen of noise."
                    },
                    "format": {
                        "type": "string",
                        "enum": ["table", "json"],
                        "description": "Output shape. 'table' (default) is compact; 'json' preserves nested structure exactly."
                    },
                    "access_token": {
                        "type": "string",
                        "description": "OAuth access token, overriding any configured credential."
                    }
                },
                "required": ["table"],
                "additionalProperties": false
            })
            .to_string(),
            capabilities: vec!["http".to_string(), "read-only".to_string()],
        }
    }

    fn invoke(
        _session_id: String,
        args_json: String,
        config_json: String,
    ) -> Result<String, String> {
        let args: Value = serde_json::from_str(&args_json)
            .map_err(|e| format!("arguments were not valid JSON: {e}"))?;
        let config: Value = serde_json::from_str(&config_json).unwrap_or(json!({}));

        let client = Bq::new(&config, &args);
        let raw = str_arg(&args, "table").ok_or("missing required argument 'table'")?;
        let (project, dataset, table) = resolve_reference(&raw, &args, &client)?;

        if table.contains('*') {
            // Worth an explicit error: a wildcard is a query-engine feature and
            // tabledata.list would just 404 on the literal name.
            return Err(format!(
                "{table:?} is a wildcard reference, which only the query engine \
                 understands — this tool reads one table's stored rows. Name a single \
                 shard, or use `bq-query` with the wildcard."
            ));
        }

        let rows_wanted = u64_arg(&args, "rows").unwrap_or(DEFAULT_ROWS).max(1);
        let selected: Vec<String> = args
            .get("selected_fields")
            .and_then(Value::as_array)
            .map(|fields| {
                fields
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|f| f.trim().to_string())
                    .filter(|f| !f.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let mut query = vec![("maxResults".to_string(), rows_wanted.min(1000).to_string())];
        if let Some(start) = u64_arg(&args, "start_index") {
            query.push(("startIndex".to_string(), start.to_string()));
        }
        if !selected.is_empty() {
            query.push(("selectedFields".to_string(), selected.join(",")));
        }
        // Keep timestamps as readable strings rather than raw int64 micros.
        query.push(("formatOptions.useInt64Timestamp".to_string(), "false".to_string()));

        let response = client.get(
            &format!("/projects/{project}/datasets/{dataset}/tables/{table}/data"),
            &query,
        )?;

        // tabledata.list returns no schema, so fetch it separately — also
        // free — because without it the rows cannot be decoded at all.
        let table_meta = client.get(
            &format!("/projects/{project}/datasets/{dataset}/tables/{table}"),
            &[],
        )?;
        let mut fields = parse_schema(table_meta.get("schema").unwrap_or(&Value::Null));

        // With selectedFields the response carries only those columns, in
        // schema order, so the schema must be narrowed the same way or the
        // positional join lands on the wrong columns.
        if !selected.is_empty() {
            fields = narrow(&fields, &selected);
            if fields.is_empty() {
                return Err(format!(
                    "none of the requested fields exist on {project}.{dataset}.{table}. \
                     `bq-describe` lists the real column names."
                ));
            }
        }

        let raw_rows = response
            .get("rows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let decoded = decode_rows(&fields, &raw_rows);
        let total = response.get("totalRows").and_then(as_u64_loose);

        let mut out = String::new();
        out.push_str(&format!("{project}.{dataset}.{table}\n\n"));

        if decoded.is_empty() {
            out.push_str("(no rows)\n");
            if u64_arg(&args, "start_index").is_some() {
                out.push_str("start_index may be past the end of the table.\n");
            }
            return Ok(out);
        }

        if str_arg(&args, "format").as_deref() == Some("json") {
            out.push_str(
                &serde_json::to_string_pretty(&decoded)
                    .map_err(|e| format!("could not serialise the rows: {e}"))?,
            );
            out.push('\n');
        } else {
            out.push_str(&render_rows(&fields, &decoded, MAX_CELL));
        }

        out.push('\n');
        match total {
            Some(total) => out.push_str(&format!(
                "{} row(s) shown of {} in the table — free, no query charge\n",
                decoded.len(),
                commas(total)
            )),
            None => out.push_str(&format!(
                "{} row(s) shown — free, no query charge\n",
                decoded.len()
            )),
        }

        // A wide table sampled without `selected_fields` is unreadable, and the
        // fix is not obvious unless stated.
        if selected.is_empty() && fields.len() > 12 {
            out.push_str(&format!(
                "\nThis table has {} columns. Pass `selected_fields` to narrow the sample \
                 to the ones you care about.\n",
                fields.len()
            ));
        }

        Ok(out)
    }
}

/// Keeps only the requested top-level fields, in schema order.
///
/// `selectedFields` accepts dotted paths for nested fields, but the response
/// still carries the whole top-level column, so matching on the first path
/// segment is what keeps the schema and the rows aligned.
fn narrow(fields: &[Field], selected: &[String]) -> Vec<Field> {
    let wanted: Vec<String> = selected
        .iter()
        .map(|field| {
            field
                .split('.')
                .next()
                .unwrap_or(field)
                .to_ascii_lowercase()
        })
        .collect();
    fields
        .iter()
        .filter(|field| wanted.contains(&field.name.to_ascii_lowercase()))
        .cloned()
        .collect()
}

/// Splits a table reference into its three parts, accepting the forms people
/// actually type.
fn resolve_reference(
    raw: &str,
    args: &Value,
    client: &Bq,
) -> Result<(String, String, String), String> {
    let cleaned = raw.trim().trim_matches('`').trim();
    let parts: Vec<&str> = cleaned.split('.').collect();

    match parts.len() {
        3 => Ok((
            parts[0].to_string(),
            parts[1].to_string(),
            parts[2].to_string(),
        )),
        2 => Ok((client.project()?, parts[0].to_string(), parts[1].to_string())),
        1 => {
            let dataset = str_arg(args, "dataset").ok_or_else(|| {
                format!(
                    "{cleaned:?} has no dataset. Pass `dataset`, or give the table as \
                     'dataset.table' or 'project.dataset.table'."
                )
            })?;
            Ok((client.project()?, dataset, parts[0].to_string()))
        }
        _ => Err(format!(
            "{cleaned:?} is not a table reference. Expected 'project.dataset.table', \
             'dataset.table' or a bare table name."
        )),
    }
}

export!(Component);
