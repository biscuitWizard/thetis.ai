//! Run a read-only SQL query against BigQuery and return the rows.
//!
//! ## What this tool has to get right
//!
//! Three things, none of which the REST API does for you:
//!
//! 1. **Completion.** `jobs.query` waits `timeoutMs` (10s by default) and then
//!    returns `jobComplete: false` with a job reference and *no rows*. A tool
//!    that returned at that point would look broken on every query slower than
//!    ten seconds. So we poll `jobs.getQueryResults` until the job finishes.
//!
//! 2. **Decoding.** Rows come back as positional `{"f":[{"v":...}]}` pairs with
//!    every scalar stringified. Joining them to the schema and coercing types
//!    is done in `bq.rs`, once.
//!
//! 3. **Cost visibility.** Every result states what it scanned and what that
//!    cost. The number is invisible otherwise, and invisible costs are the ones
//!    that surprise people at the end of the month.
//!
//! Mutations are refused here and live in `bq-execute`. The split is the point:
//! an agent reaching for "run some SQL" gets the tool that cannot drop a table.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod bq;

use bq::{
    as_u64_loose, bool_arg, commas, decode_rows, estimate_cost, human_bytes, parse_schema,
    render_cost, render_rows, require_read_only, str_arg, u64_arg, Bq,
};
use serde_json::{json, Value};
use thetis::grip::sys;

/// Rows fetched per page. Responses are also capped at 10 MB by the API, so a
/// page can come back short; that is handled by looping on the page token.
const DEFAULT_MAX_ROWS: u64 = 200;
/// How long each poll waits server-side. The API blocks up to this long before
/// answering, so it doubles as our sleep — there is no timer in a wasm guest.
const POLL_TIMEOUT_MS: u64 = 10_000;
/// Cap on polls, so a genuinely long query returns a job id to resume from
/// rather than sitting until the tool's own budget kills it.
const MAX_POLLS: u32 = 12;
/// Characters per cell before truncation, so one JSON blob cannot swamp the
/// table.
const MAX_CELL: usize = 120;

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "bq-query".to_string(),
            description: "Run a read-only SQL query against BigQuery and get the rows back \
                as an aligned table, with the bytes scanned and the cost it incurred. Waits \
                for the job to finish, and pages through large results. Supports named or \
                positional query parameters — use them rather than interpolating values \
                into SQL. Only SELECT and WITH are accepted; DML and DDL are refused, and \
                live in `bq-execute`. If you are unsure what a query will cost, run \
                `bq-query-cost` first — it is free. Reaches bigquery.googleapis.com."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": "The GoogleSQL query. Legacy SQL is not supported. Use @named parameters instead of interpolating literals."
                    },
                    "parameters": {
                        "type": "object",
                        "description": "Named query parameters, e.g. {\"since\": \"2024-01-01\", \"limit\": 10}. Referenced in SQL as @since. Types are inferred; pass {\"value\": ..., \"type\": \"DATE\"} to be explicit."
                    },
                    "positional_parameters": {
                        "type": "array",
                        "description": "Positional parameters for ? placeholders, in order. Use instead of `parameters`, not alongside.",
                        "items": {}
                    },
                    "max_rows": {
                        "type": "integer",
                        "description": "Rows to return. Default 200. Raise for a full extract, but mind that everything returned lands in the transcript."
                    },
                    "project": {
                        "type": "string",
                        "description": "GCP project to bill and run in. Defaults to the configured project."
                    },
                    "location": {
                        "type": "string",
                        "description": "Dataset location, e.g. 'US' or 'europe-west2'. Only needed outside the US/EU multi-regions."
                    },
                    "default_dataset": {
                        "type": "string",
                        "description": "Dataset assumed for unqualified table names, as 'dataset' or 'project.dataset'."
                    },
                    "maximum_bytes_billed": {
                        "type": "integer",
                        "description": "Fail the query rather than scan more than this many bytes. Nothing is billed when it trips."
                    },
                    "use_cache": {
                        "type": "boolean",
                        "description": "Allow BigQuery's result cache, which is free and instant on a repeated query. Default true."
                    },
                    "format": {
                        "type": "string",
                        "enum": ["table", "json"],
                        "description": "Output shape. 'table' (default) is far more compact; 'json' is exact, for nested data or further processing."
                    },
                    "access_token": {
                        "type": "string",
                        "description": "OAuth access token, overriding any configured credential. `gcloud auth print-access-token` prints one."
                    }
                },
                "required": ["sql"],
                "additionalProperties": false
            })
            .to_string(),
            // Refuses anything but SELECT/WITH, so it cannot change data. It
            // does spend money, which the description states plainly.
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

        let sql = str_arg(&args, "sql")
            .ok_or("missing required argument 'sql' — the query to run")?;
        // The guard that makes this tool safe to hand out. Checked before any
        // network call, so a refused statement costs nothing.
        require_read_only(&sql)?;

        let client = Bq::new(&config, &args);
        let project = client.project()?;
        let max_rows = u64_arg(&args, "max_rows").unwrap_or(DEFAULT_MAX_ROWS).max(1);

        let mut request = json!({
            "query": sql,
            // The API defaults this to true, which would parse a modern query
            // as legacy SQL and fail confusingly.
            "useLegacySql": false,
            "maxResults": max_rows.min(1000),
            "timeoutMs": POLL_TIMEOUT_MS,
            "useQueryCache": bool_arg(&args, "use_cache").unwrap_or(true),
            // Ask for int64s as numbers rather than strings where the API will
            // do it, which loses less in the round trip.
            "formatOptions": { "useInt64Timestamp": false },
        });

        if let Some(location) = client.location.clone() {
            request["location"] = json!(location);
        }
        if let Some(cap) = client.max_bytes_billed {
            // A string: the field is int64, and JSON numbers lose precision
            // above 2^53.
            request["maximumBytesBilled"] = json!(cap.to_string());
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
        apply_parameters(&args, &mut request)?;

        let mut response = client.post(&format!("/projects/{project}/queries"), &request)?;

        // ------------------------------------------------------------------
        // Wait for completion
        // ------------------------------------------------------------------
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
            .map(str::to_string)
            .or_else(|| client.location.clone());

        let mut polls = 0;
        while !response
            .get("jobComplete")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            if job_id.is_empty() {
                return Err("BigQuery did not complete the query and returned no job id to \
                            poll, which should not happen."
                    .to_string());
            }
            if polls >= MAX_POLLS {
                return Ok(format!(
                    "the query is still running after about {}s.\n\n\
                     job id: {job_id}\n\n\
                     It was not cancelled and is still going. Fetch the results when it \
                     finishes with `bq-results` and this job id.",
                    MAX_POLLS as u64 * POLL_TIMEOUT_MS / 1000
                ));
            }
            polls += 1;
            sys::log(
                thetis::grip::types::LogLevel::Debug,
                &format!("bq-query: polling job {job_id} ({polls})"),
            );
            // getQueryResults blocks server-side for timeoutMs, so this is the
            // wait as well as the check.
            response = fetch_page(&client, &project, &job_id, &job_location, None, max_rows)?;
        }

        // A completed job can still carry errors; the first is the real cause.
        if let Some(errors) = response.get("errors").and_then(Value::as_array) {
            if let Some(first) = errors.first() {
                let message = first
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                return Err(format!("the query failed: {message}"));
            }
        }

        // ------------------------------------------------------------------
        // Collect rows
        // ------------------------------------------------------------------
        let fields = parse_schema(response.get("schema").unwrap_or(&Value::Null));
        let mut rows: Vec<Value> = Vec::new();
        let mut page = response.clone();

        loop {
            let raw = page.get("rows").and_then(Value::as_array).cloned().unwrap_or_default();
            rows.extend(decode_rows(&fields, &raw));

            if rows.len() as u64 >= max_rows {
                break;
            }
            let Some(token) = page.get("pageToken").and_then(Value::as_str).filter(|t| !t.is_empty())
            else {
                break;
            };
            if job_id.is_empty() {
                break;
            }
            let remaining = max_rows - rows.len() as u64;
            page = fetch_page(
                &client,
                &project,
                &job_id,
                &job_location,
                Some(token),
                remaining,
            )?;
        }
        rows.truncate(max_rows as usize);

        // ------------------------------------------------------------------
        // Report
        // ------------------------------------------------------------------
        let total_rows = response.get("totalRows").and_then(as_u64_loose).unwrap_or(rows.len() as u64);
        let scanned = response
            .get("totalBytesProcessed")
            .and_then(as_u64_loose)
            .unwrap_or(0);
        let billed = response.get("totalBytesBilled").and_then(as_u64_loose);
        let cache_hit = response.get("cacheHit").and_then(Value::as_bool).unwrap_or(false);

        let mut out = String::new();

        if str_arg(&args, "format").as_deref() == Some("json") {
            out.push_str(
                &serde_json::to_string_pretty(&rows)
                    .map_err(|e| format!("could not serialise the rows: {e}"))?,
            );
            out.push('\n');
        } else {
            out.push_str(&render_rows(&fields, &rows, MAX_CELL));
        }

        out.push('\n');
        out.push_str(&format!("{} of {} row(s)", commas(rows.len() as u64), commas(total_rows)));
        if total_rows > rows.len() as u64 {
            out.push_str(&format!(
                " — raise `max_rows`, or page the rest with `bq-results` and job id {job_id}"
            ));
        }
        out.push('\n');

        if cache_hit {
            // Worth saying: a cache hit is free, and knowing that stops someone
            // "optimising" a query that already costs nothing.
            out.push_str("cached result — no bytes scanned, nothing billed\n");
        } else {
            let effective = billed.unwrap_or(scanned);
            out.push_str(&format!(
                "scanned {} · cost {}\n",
                human_bytes(scanned),
                render_cost(estimate_cost(effective, client.price_per_tib))
            ));
        }

        Ok(out)
    }
}

/// One page of results for a job that already exists.
fn fetch_page(
    client: &Bq,
    project: &str,
    job_id: &str,
    location: &Option<String>,
    page_token: Option<&str>,
    max_rows: u64,
) -> Result<Value, String> {
    let mut query = vec![
        ("maxResults".to_string(), max_rows.min(1000).to_string()),
        ("timeoutMs".to_string(), POLL_TIMEOUT_MS.to_string()),
    ];
    if let Some(token) = page_token {
        query.push(("pageToken".to_string(), token.to_string()));
    }
    if let Some(location) = location {
        // Required for jobs outside the US/EU multi-regions, and harmless
        // inside them.
        query.push(("location".to_string(), location.clone()));
    }
    client.get(&format!("/projects/{project}/queries/{job_id}"), &query)
}

/// Translates the friendly parameter forms into BigQuery's verbose shape.
///
/// Parameters exist so values never have to be interpolated into SQL, which is
/// both an injection risk and a cache-defeating habit — a parameterised query
/// hits the result cache across different values. But the wire format is deeply
/// nested, so accept a plain map and infer the types.
fn apply_parameters(args: &Value, request: &mut Value) -> Result<(), String> {
    if let Some(named) = args.get("parameters").and_then(Value::as_object) {
        if !named.is_empty() {
            let mut parameters = Vec::new();
            for (name, value) in named {
                let (ty, text) = parameter_type_and_value(value)?;
                parameters.push(json!({
                    "name": name,
                    "parameterType": { "type": ty },
                    "parameterValue": { "value": text },
                }));
            }
            request["parameterMode"] = json!("NAMED");
            request["queryParameters"] = json!(parameters);
            return Ok(());
        }
    }

    if let Some(positional) = args.get("positional_parameters").and_then(Value::as_array) {
        if !positional.is_empty() {
            let mut parameters = Vec::new();
            for value in positional {
                let (ty, text) = parameter_type_and_value(value)?;
                parameters.push(json!({
                    "parameterType": { "type": ty },
                    "parameterValue": { "value": text },
                }));
            }
            request["parameterMode"] = json!("POSITIONAL");
            request["queryParameters"] = json!(parameters);
        }
    }

    Ok(())
}

/// Infers a BigQuery parameter type, or takes an explicit one.
///
/// Inference covers the common cases. It cannot distinguish a DATE from a
/// STRING that looks like one, which matters because comparing a STRING
/// parameter to a DATE column is an error — hence the explicit
/// `{"value": ..., "type": "DATE"}` form.
fn parameter_type_and_value(value: &Value) -> Result<(String, Value), String> {
    // Explicit form: {"value": ..., "type": "DATE"}
    if let Some(object) = value.as_object() {
        if let (Some(inner), Some(ty)) = (object.get("value"), object.get("type").and_then(Value::as_str)) {
            let text = match inner {
                Value::String(text) => Value::String(text.clone()),
                Value::Null => Value::Null,
                other => Value::String(other.to_string()),
            };
            return Ok((ty.to_uppercase(), text));
        }
    }

    Ok(match value {
        Value::Null => ("STRING".to_string(), Value::Null),
        Value::Bool(flag) => ("BOOL".to_string(), Value::String(flag.to_string())),
        Value::String(text) => ("STRING".to_string(), Value::String(text.clone())),
        Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                ("INT64".to_string(), Value::String(number.to_string()))
            } else {
                ("FLOAT64".to_string(), Value::String(number.to_string()))
            }
        }
        Value::Array(_) | Value::Object(_) => {
            return Err(
                "array and struct query parameters are not supported here. Inline the \
                 values into the SQL as a literal list, or pass them as separate scalar \
                 parameters."
                    .to_string(),
            )
        }
    })
}

export!(Component);
