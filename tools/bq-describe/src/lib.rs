//! Show a BigQuery table's schema, size, partitioning and clustering.
//!
//! ## Why this is free, and why that matters
//!
//! Everything here comes from `tables.get`, which reads metadata and is not
//! billed. That makes it the right first call against an unfamiliar table:
//! `SELECT *` to "see what's in there" scans the whole thing and charges for
//! it, whereas this returns the column list, the exact row count and the byte
//! size for nothing.
//!
//! The partitioning and clustering sections are the ones that actually save
//! money. A query against a partitioned table that omits a filter on the
//! partition column scans every partition — often thousands of times more data
//! than intended. Knowing the partition column *before* writing the query is
//! the difference between scanning a day and scanning a year, so this tool says
//! so explicitly rather than leaving it in a JSON field.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod bq;

use bq::{
    as_u64_loose, bool_arg, clip, commas, estimate_cost, human_bytes, parse_schema, render_cost,
    str_arg, Bq, Field,
};
use serde_json::{json, Value};

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "bq-describe".to_string(),
            description: "Show a BigQuery table's full schema with nested fields, its exact \
                row count, byte size, partitioning and clustering — everything needed to \
                write a correct and cheap query against it. Free: reads metadata only, no \
                query charges, so prefer this over `SELECT *` for finding out what a table \
                contains. Pay attention to the partitioning section: querying a partitioned \
                table without filtering the partition column scans all of it. Accepts \
                'project.dataset.table' or separate arguments. Reaches \
                bigquery.googleapis.com."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "table": {
                        "type": "string",
                        "description": "The table, as 'project.dataset.table', 'dataset.table', or just the table name with `dataset` given separately."
                    },
                    "dataset": {
                        "type": "string",
                        "description": "Dataset, if not included in `table`."
                    },
                    "project": {
                        "type": "string",
                        "description": "GCP project. Defaults to the configured project."
                    },
                    "show_description": {
                        "type": "boolean",
                        "description": "Include column descriptions where the table has them. Default true."
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

        let response = client.get(
            &format!("/projects/{project}/datasets/{dataset}/tables/{table}"),
            &[],
        )?;

        let kind = response
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("TABLE");
        let rows = response.get("numRows").and_then(as_u64_loose);
        let bytes = response.get("numBytes").and_then(as_u64_loose);

        let mut out = String::new();
        out.push_str(&format!("{project}.{dataset}.{table}\n"));
        out.push_str(&format!("{}\n\n", "=".repeat(40)));
        out.push_str(&format!("type:        {kind}\n"));

        if let Some(rows) = rows {
            out.push_str(&format!("rows:        {}\n", commas(rows)));
        }
        if let Some(bytes) = bytes {
            out.push_str(&format!(
                "size:        {} — a full scan would cost {}\n",
                human_bytes(bytes),
                render_cost(estimate_cost(bytes, client.price_per_tib))
            ));
        }
        if let Some(description) = response
            .get("description")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
        {
            out.push_str(&format!("description: {}\n", clip(description, 400)));
        }
        if let Some(location) = response.get("location").and_then(Value::as_str) {
            out.push_str(&format!("location:    {location}\n"));
        }

        // --- Partitioning: the section that decides what a query costs ------
        let mut partition_note = None;
        if let Some(time) = response.get("timePartitioning") {
            let ty = time.get("type").and_then(Value::as_str).unwrap_or("DAY");
            let column = time
                .get("field")
                .and_then(Value::as_str)
                .unwrap_or("_PARTITIONTIME");
            out.push_str(&format!("partitioned: by {column} ({ty})\n"));
            if let Some(days) = time.get("expirationMs").and_then(as_u64_loose) {
                out.push_str(&format!(
                    "  partitions expire after {} day(s)\n",
                    days / 86_400_000
                ));
            }
            let required = time
                .get("requirePartitionFilter")
                .and_then(Value::as_bool)
                .or_else(|| {
                    response
                        .get("requirePartitionFilter")
                        .and_then(Value::as_bool)
                })
                .unwrap_or(false);
            partition_note = Some(if required {
                format!(
                    "This table REQUIRES a filter on {column} — a query without one is \
                     rejected outright."
                )
            } else {
                format!(
                    "Filter on {column} in every query. Without it BigQuery scans every \
                     partition, which is the single most common way to run up a large bill \
                     here."
                )
            });
        } else if let Some(range) = response.get("rangePartitioning") {
            let column = range.get("field").and_then(Value::as_str).unwrap_or("?");
            out.push_str(&format!("partitioned: by {column} (integer range)\n"));
            partition_note = Some(format!("Filter on {column} to limit the scan."));
        }

        if let Some(clustering) = response
            .get("clustering")
            .and_then(|c| c.get("fields"))
            .and_then(Value::as_array)
        {
            let columns: Vec<String> = clustering
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            out.push_str(&format!("clustered:   by {}\n", columns.join(", ")));
            out.push_str(
                "  filtering or grouping on the clustering columns, leftmost first, \
                 reduces the scan\n",
            );
        }

        // A wildcard table name is a strong hint this is a sharded set, where
        // _TABLE_SUFFIX is the equivalent of a partition filter.
        if table.contains('*') {
            out.push_str(
                "\nThis is a wildcard reference over sharded tables. Filter on \
                 _TABLE_SUFFIX to limit which shards are read.\n",
            );
        }

        // --- Schema ---------------------------------------------------------
        let fields = parse_schema(response.get("schema").unwrap_or(&Value::Null));
        let show_description = bool_arg(&args, "show_description").unwrap_or(true);
        out.push_str(&format!("\n{} column(s):\n", count_fields(&fields)));
        for field in &fields {
            write_field(&mut out, field, 0, show_description);
        }

        if let Some(view) = response
            .get("view")
            .and_then(|v| v.get("query"))
            .and_then(Value::as_str)
        {
            out.push_str(&format!("\nview definition:\n{}\n", clip(view, 2000)));
        }

        if let Some(note) = partition_note {
            out.push_str(&format!("\nNOTE: {note}\n"));
        }

        Ok(out)
    }
}

/// Renders one field, indenting nested records so structure is visible.
fn write_field(out: &mut String, field: &Field, depth: usize, show_description: bool) {
    let indent = "  ".repeat(depth + 1);
    let name_width = 34usize.saturating_sub(depth * 2);
    // For a record, print the head and recurse rather than inlining the whole
    // STRUCT<...> on one line, which becomes unreadable past two levels.
    let type_text = if field.is_record() {
        if field.is_repeated() {
            "ARRAY<STRUCT>".to_string()
        } else {
            "STRUCT".to_string()
        }
    } else {
        field.display_type()
    };

    let required = if field.mode.eq_ignore_ascii_case("REQUIRED") {
        " NOT NULL"
    } else {
        ""
    };
    let description = if show_description {
        field
            .description
            .as_ref()
            .map(|d| format!("  — {}", clip(d, 100)))
            .unwrap_or_default()
    } else {
        String::new()
    };

    out.push_str(&format!(
        "{indent}{:<width$} {}{}{}\n",
        field.name,
        type_text,
        required,
        description,
        width = name_width
    ));

    if field.is_record() {
        for child in &field.fields {
            write_field(out, child, depth + 1, show_description);
        }
    }
}

fn count_fields(fields: &[Field]) -> usize {
    fields
        .iter()
        .map(|field| 1 + count_fields(&field.fields))
        .sum()
}

/// Splits a table reference into its three parts.
///
/// Accepts the forms people actually type: fully qualified, dataset-qualified,
/// or a bare name with `dataset` passed separately. Backticks are stripped
/// because they are how the reference appears in SQL.
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
            "{cleaned:?} is not a table reference. Expected \
             'project.dataset.table', 'dataset.table' or a bare table name."
        )),
    }
}

export!(Component);
