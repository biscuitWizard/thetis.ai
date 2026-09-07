//! The one BigQuery tool that can change things.
//!
//! ## Why mutation is a separate tool
//!
//! Every other `bq-*` tool declares `read-only` and refuses anything but SELECT.
//! This one is the deliberate exception, and it is a separate component rather
//! than a flag on `bq-query` for two reasons:
//!
//! * A read-only conversation mode filters on the declared capability. If
//!   mutation lived behind an argument on `bq-query`, the whole tool would have
//!   to be withheld or the whole tool trusted. Split, the reads stay available
//!   and only this is withdrawn.
//! * It cannot be reached by accident. Writing DELETE into the tool you were
//!   using to explore is a slip; choosing `bq-execute` is not.
//!
//! It is the mirror image of the others: it *requires* a mutating statement and
//! refuses a bare SELECT, pointing at `bq-query` instead.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod bq;

use bq::{
    as_u64_loose, commas, decode_rows, estimate_cost, human_bytes, parse_schema, render_cost,
    render_rows, statement_kind, str_arg, u64_arg, Bq,
};
use serde_json::{json, Value};

const POLL_TIMEOUT_MS: u64 = 30_000;
const MAX_POLLS: u32 = 20;
const MAX_CELL: usize = 100;

/// Statements this tool exists to run.
const MUTATING: [&str; 10] = [
    "INSERT", "UPDATE", "DELETE", "MERGE", "TRUNCATE", "CREATE", "DROP", "ALTER", "GRANT", "REVOKE",
];

/// Statements that are neither plain reads nor simple DML — scripts, procedure
/// calls, transactions. Allowed, because a script is the normal way to do
/// anything non-trivial, but they are named so the classification is explicit
/// rather than a fallthrough.
const SCRIPTING: [&str; 8] = [
    "DECLARE", "SET", "BEGIN", "CALL", "EXECUTE", "COMMIT", "ROLLBACK", "LOAD",
];

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "bq-execute".to_string(),
            description: "Run a mutating BigQuery statement — INSERT, UPDATE, DELETE, MERGE, \
                CREATE, DROP, ALTER, or a script. This is the only bq-* tool that can change \
                data or schema; it is deliberately separate from bq-query so that mutation \
                is never accidental, and it refuses a plain SELECT. Effects are real and not \
                undoable by this tool. Reaches bigquery.googleapis.com."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": "The GoogleSQL statement or script to run. Must be a mutating statement; use `bq-query` to read."
                    },
                    "confirm": {
                        "type": "boolean",
                        "description": "Must be true for a statement that destroys data or schema (DELETE, DROP, TRUNCATE, ALTER, or an UPDATE/DELETE without a WHERE clause). A guard against a slip, not against intent."
                    },
                    "parameters": {
                        "type": "object",
                        "description": "Named query parameters, e.g. {\"id\": 42}. Referenced in SQL as @id. Always prefer these over interpolating values into the statement."
                    },
                    "project": {
                        "type": "string",
                        "description": "GCP project to bill and run in. Defaults to the configured project."
                    },
                    "default_dataset": {
                        "type": "string",
                        "description": "Dataset assumed for unqualified table names, as 'dataset' or 'project.dataset'."
                    },
                    "location": {
                        "type": "string",
                        "description": "Dataset location, e.g. 'US' or 'europe-west2'. Only needed outside the US/EU multi-regions."
                    },
                    "maximum_bytes_billed": {
                        "type": "integer",
                        "description": "Fail rather than scan more than this many bytes. Nothing is billed when it trips."
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "Validate the statement and price it without running it. Free, and changes nothing — worth doing first for anything irreversible."
                    },
                    "access_token": {
                        "type": "string",
                        "description": "OAuth access token, overriding any configured credential."
                    }
                },
                "required": ["sql"],
                "additionalProperties": false
            })
            .to_string(),
            // Deliberately NOT read-only: this is the tool a read-only mode
            // must withhold.
            capabilities: vec!["http".to_string()],
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

        let sql = str_arg(&args, "sql").ok_or("missing required argument 'sql'")?;
        if sql.trim().is_empty() {
            return Err("'sql' is empty".to_string());
        }

        // --- Classify before touching the network ---------------------------
        let kind = statement_kind(&sql);
        let mutating = MUTATING.contains(&kind.as_str());
        let scripting = SCRIPTING.contains(&kind.as_str());

        if !mutating && !scripting {
            return Err(format!(
                "{} is a read, and this tool is for mutation. Use `bq-query` — it \
                 paginates, reports cost, and cannot change anything.",
                if kind.is_empty() { "that" } else { &kind }
            ));
        }

        let dry_run = args
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // --- The confirmation gate ------------------------------------------
        // Only for statements that destroy. Requiring it for INSERT and CREATE
        // too would make it a reflex to pass, which is how a guard stops
        // working.
        if !dry_run {
            if let Some(danger) = destructive(&kind, &sql) {
                let confirmed = args
                    .get("confirm")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !confirmed {
                    return Err(format!(
                        "refusing to run this without confirmation: {danger}\n\n\
                         Check the statement, then pass confirm=true. Or pass \
                         dry_run=true to validate it for free without running it."
                    ));
                }
            }
        }

        let client = Bq::new(&config, &args);
        let project = client.project()?;

        let mut request = json!({
            "query": sql,
            "useLegacySql": false,
            "timeoutMs": POLL_TIMEOUT_MS,
            "maxResults": 200,
        });
        if dry_run {
            request["dryRun"] = json!(true);
        }
        if let Some(dataset) = str_arg(&args, "default_dataset") {
            let (dataset_project, dataset_id) = match dataset.split_once('.') {
                Some((p, d)) => (p.to_string(), d.to_string()),
                None => (project.clone(), dataset),
            };
            request["defaultDataset"] = json!({
                "projectId": dataset_project,
                "datasetId": dataset_id,
            });
        }
        if let Some(location) = str_arg(&args, "location").or_else(|| client.location.clone()) {
            request["location"] = json!(location);
        }
        match u64_arg(&args, "maximum_bytes_billed").or(client.max_bytes_billed) {
            Some(cap) => request["maximumBytesBilled"] = json!(cap.to_string()),
            None => {}
        }
        if let Some(map) = args.get("parameters").and_then(Value::as_object) {
            if !map.is_empty() {
                let bound: Vec<Value> = map
                    .iter()
                    .map(|(name, value)| bind(name, value))
                    .collect::<Result<Vec<_>, String>>()?;
                request["parameterMode"] = json!("NAMED");
                request["queryParameters"] = json!(bound);
            }
        }

        let mut response = client.post(&format!("/projects/{project}/queries"), &request)?;

        // --- Dry run: report and stop ---------------------------------------
        if dry_run {
            let bytes = response
                .get("totalBytesProcessed")
                .and_then(as_u64_loose)
                .unwrap_or(0);
            let mut out = String::new();
            out.push_str(&format!("{kind} statement is valid. Nothing was run.\n\n"));
            out.push_str(&format!(
                "would scan: {} · {}\n",
                human_bytes(bytes),
                render_cost(estimate_cost(bytes, client.price_per_tib))
            ));
            if let Some(danger) = destructive(&kind, &sql) {
                out.push_str(&format!(
                    "\nWhen run for real this will need confirm=true: {danger}\n"
                ));
            }
            return Ok(out);
        }

        // --- Wait for it ----------------------------------------------------
        let job_id = response
            .get("jobReference")
            .and_then(|r| r.get("jobId"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let job_location = response
            .get("jobReference")
            .and_then(|r| r.get("location"))
            .and_then(Value::as_str)
            .map(str::to_string);

        let mut polls = 0;
        while !response
            .get("jobComplete")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            if job_id.is_empty() || polls >= MAX_POLLS {
                // Crucially not an error about the statement: it was submitted
                // and is very likely still going to take effect.
                return Ok(format!(
                    "the statement is still running as job {job_id}.\n\n\
                     It has NOT been cancelled and will most likely complete. Check it \
                     with `bq-jobs`, or read any results with `bq-results`."
                ));
            }
            polls += 1;
            let mut query = vec![
                ("timeoutMs".to_string(), POLL_TIMEOUT_MS.to_string()),
                ("maxResults".to_string(), "200".to_string()),
            ];
            if let Some(location) = &job_location {
                query.push(("location".to_string(), location.clone()));
            }
            response = client.get(&format!("/projects/{project}/queries/{job_id}"), &query)?;
        }

        if let Some(first) = response
            .get("errors")
            .and_then(Value::as_array)
            .and_then(|errors| errors.first())
        {
            let message = first
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(format!("the statement failed: {message}"));
        }

        // --- Report what changed --------------------------------------------
        let mut out = String::new();
        out.push_str(&format!("{kind} completed.\n\n"));

        match response.get("numDmlAffectedRows").and_then(as_u64_loose) {
            Some(rows) => {
                out.push_str(&format!("rows affected: {}\n", commas(rows)));
                // The breakdown is what tells a MERGE apart from a no-op.
                if let Some(stats) = response.get("dmlStats") {
                    let part = |key: &str| stats.get(key).and_then(as_u64_loose).unwrap_or(0);
                    let (inserted, deleted, updated) = (
                        part("insertedRowCount"),
                        part("deletedRowCount"),
                        part("updatedRowCount"),
                    );
                    if inserted + deleted + updated > 0 {
                        out.push_str(&format!(
                            "  inserted {}  ·  updated {}  ·  deleted {}\n",
                            commas(inserted),
                            commas(updated),
                            commas(deleted)
                        ));
                    }
                }
            }
            None => {
                // DDL and scripts report no row count; say so rather than
                // leaving the caller wondering.
                out.push_str("no row count reported (normal for DDL and scripts)\n");
            }
        }

        // A script's final statement may return rows; show them.
        let fields = parse_schema(response.get("schema").unwrap_or(&Value::Null));
        let raw_rows = response
            .get("rows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !fields.is_empty() && !raw_rows.is_empty() {
            let decoded = decode_rows(&fields, &raw_rows);
            out.push('\n');
            out.push_str(&render_rows(&fields, &decoded, MAX_CELL));
        }

        let bytes = response
            .get("totalBytesBilled")
            .and_then(as_u64_loose)
            .or_else(|| response.get("totalBytesProcessed").and_then(as_u64_loose))
            .unwrap_or(0);
        out.push_str(&format!(
            "\nscanned {} · {}\n",
            human_bytes(bytes),
            render_cost(estimate_cost(bytes, client.price_per_tib))
        ));
        if !job_id.is_empty() {
            out.push_str(&format!("job: {job_id}\n"));
        }

        Ok(out)
    }
}

/// Describes why a statement is destructive, or None if it only adds.
///
/// The unqualified-DML check is the valuable one: `DELETE FROM t` without a
/// WHERE is legal, silent, and empties the table.
fn destructive(kind: &str, sql: &str) -> Option<String> {
    let upper = sql.to_uppercase();
    let has_where = upper.contains(" WHERE ") || upper.contains("\nWHERE");

    match kind {
        "DROP" => Some(format!(
            "DROP removes an object permanently — {}",
            first_line(sql)
        )),
        "TRUNCATE" => Some("TRUNCATE empties the table completely".to_string()),
        "ALTER" => Some(format!(
            "ALTER changes schema, which can drop columns and their data — {}",
            first_line(sql)
        )),
        "DELETE" if !has_where => {
            Some("DELETE with no WHERE clause removes every row".to_string())
        }
        "DELETE" => Some("DELETE removes rows".to_string()),
        "UPDATE" if !has_where => {
            Some("UPDATE with no WHERE clause rewrites every row".to_string())
        }
        "CREATE" if upper.contains("OR REPLACE") => {
            Some(format!(
                "CREATE OR REPLACE overwrites whatever is already there — {}",
                first_line(sql)
            ))
        }
        "MERGE" if upper.contains("DELETE") => {
            Some("this MERGE contains a DELETE branch".to_string())
        }
        "GRANT" | "REVOKE" => Some("this changes access control".to_string()),
        _ => None,
    }
}

fn first_line(sql: &str) -> String {
    let flat = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 120 {
        format!("{}…", flat.chars().take(120).collect::<String>())
    } else {
        flat
    }
}

/// Turns a plain JSON value into a typed BigQuery query parameter.
fn bind(name: &str, value: &Value) -> Result<Value, String> {
    // The explicit form, for when inference would guess wrong — a DATE looks
    // like a STRING otherwise.
    if let Some(object) = value.as_object() {
        if let (Some(inner), Some(ty)) = (object.get("value"), object.get("type")) {
            let ty = ty
                .as_str()
                .ok_or_else(|| format!("parameter {name:?} has a non-string 'type'"))?;
            return Ok(json!({
                "name": name,
                "parameterType": { "type": ty.to_uppercase() },
                "parameterValue": { "value": scalar_text(inner) },
            }));
        }
    }

    let ty = match value {
        Value::Bool(_) => "BOOL",
        Value::Number(number) if number.is_i64() || number.is_u64() => "INT64",
        Value::Number(_) => "FLOAT64",
        Value::String(_) => "STRING",
        Value::Null => "STRING",
        Value::Array(_) | Value::Object(_) => {
            return Err(format!(
                "parameter {name:?} is an array or struct, which this tool does not bind. \
                 Pass scalars, or write the literal into the statement."
            ))
        }
    };

    Ok(json!({
        "name": name,
        "parameterType": { "type": ty },
        "parameterValue": { "value": scalar_text(value) },
    }))
}

/// Parameter values go over the wire as strings, whatever their declared type.
fn scalar_text(value: &Value) -> Value {
    match value {
        Value::Null => Value::Null,
        Value::String(text) => Value::String(text.clone()),
        other => Value::String(other.to_string()),
    }
}

export!(Component);
