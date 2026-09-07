//! List BigQuery datasets, tables, views and routines.
//!
//! Pure discovery, and free: `datasets.list` and `tables.list` read metadata
//! and are not billed. This is the entry point to an unfamiliar project —
//! without it the only way to find out what exists is to guess names, or to
//! query `INFORMATION_SCHEMA`, which *is* billed at a 10 MB minimum per call.
//!
//! One nicety worth the code: sharded tables. A GA4 export leaves a project
//! holding `events_20240301`, `events_20240302` and hundreds more, which would
//! bury every other table in the listing. They are collapsed into a single
//! `events_*` line with the shard count and date range, because that is the
//! form you would actually query them in.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod bq;

use bq::{as_u64_loose, commas, human_bytes, str_arg, u64_arg, Bq};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Tables fetched by default. Higher than it looks like it needs to be because
/// sharded sets collapse: a GA4 dataset here holds 1614 tables that render as
/// one line, and a limit of 200 would report the shard range as ending in 2022
/// when the data runs to the present. Pagination is free, so the only cost of a
/// high default is latency.
const DEFAULT_LIMIT: u64 = 5000;

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "bq-list".to_string(),
            description: "List what exists in BigQuery: datasets in a project, or tables, \
                views and routines in a dataset. Free — reads metadata only, with no query \
                charges — so this is the right way to explore an unfamiliar project rather \
                than guessing names or querying INFORMATION_SCHEMA, which is billed. Date-\
                sharded tables such as a GA4 export are collapsed into one `prefix_*` entry \
                with their shard count and range. Reaches bigquery.googleapis.com."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "dataset": {
                        "type": "string",
                        "description": "Dataset to list the contents of. Omit to list the datasets in the project instead."
                    },
                    "project": {
                        "type": "string",
                        "description": "GCP project. Defaults to the configured project."
                    },
                    "kind": {
                        "type": "string",
                        "enum": ["tables", "routines"],
                        "description": "What to list inside a dataset. Default 'tables', which includes views."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum entries to return. Default 200."
                    },
                    "expand_shards": {
                        "type": "boolean",
                        "description": "List every date shard separately instead of collapsing them. Default false."
                    },
                    "access_token": {
                        "type": "string",
                        "description": "OAuth access token, overriding any configured credential."
                    }
                },
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
        let project = client.project()?;
        let limit = u64_arg(&args, "limit").unwrap_or(DEFAULT_LIMIT).max(1);

        match str_arg(&args, "dataset") {
            None => list_datasets(&client, &project, limit),
            Some(dataset) => {
                let dataset = dataset.trim().trim_matches('`').to_string();
                match str_arg(&args, "kind").as_deref() {
                    Some("routines") => list_routines(&client, &project, &dataset, limit),
                    _ => list_tables(&client, &project, &dataset, limit, &args),
                }
            }
        }
    }
}

fn list_datasets(client: &Bq, project: &str, limit: u64) -> Result<String, String> {
    let items = paginate(
        client,
        &format!("/projects/{project}/datasets"),
        "datasets",
        limit,
        &[],
    )?;

    if items.is_empty() {
        return Ok(format!(
            "{project} has no datasets you can see. Check the project id, and that the \
             credential has bigquery.datasets.get on it."
        ));
    }

    let mut out = format!("{} dataset(s) in {project}:\n\n", items.len());
    for item in &items {
        let id = item
            .get("datasetReference")
            .and_then(|r| r.get("datasetId"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let location = item.get("location").and_then(Value::as_str).unwrap_or("");
        // Labels are how teams mark dev/prod, so they earn their place.
        let labels = item
            .get("labels")
            .and_then(Value::as_object)
            .map(|labels| {
                labels
                    .iter()
                    .map(|(key, value)| format!("{key}={}", value.as_str().unwrap_or("")))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|labels| !labels.is_empty())
            .map(|labels| format!("  [{labels}]"))
            .unwrap_or_default();
        out.push_str(&format!("  {id:<44} {location}{labels}\n"));
    }
    out.push_str("\nPass `dataset` to list the tables in one.\n");
    Ok(out)
}

fn list_routines(
    client: &Bq,
    project: &str,
    dataset: &str,
    limit: u64,
) -> Result<String, String> {
    let items = paginate(
        client,
        &format!("/projects/{project}/datasets/{dataset}/routines"),
        "routines",
        limit,
        &[],
    )?;

    if items.is_empty() {
        return Ok(format!("{project}.{dataset} has no routines."));
    }

    let mut out = format!("{} routine(s) in {project}.{dataset}:\n\n", items.len());
    for item in &items {
        let id = item
            .get("routineReference")
            .and_then(|r| r.get("routineId"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let kind = item
            .get("routineType")
            .and_then(Value::as_str)
            .unwrap_or("");
        let language = item.get("language").and_then(Value::as_str).unwrap_or("");
        out.push_str(&format!("  {id:<44} {kind} {language}\n"));
    }
    Ok(out)
}

fn list_tables(
    client: &Bq,
    project: &str,
    dataset: &str,
    limit: u64,
    args: &Value,
) -> Result<String, String> {
    let items = paginate(
        client,
        &format!("/projects/{project}/datasets/{dataset}/tables"),
        "tables",
        limit,
        &[],
    )?;

    if items.is_empty() {
        return Ok(format!(
            "{project}.{dataset} has no tables you can see, or the dataset does not exist. \
             Run `bq-list` without `dataset` to see what does."
        ));
    }

    let expand = bq::bool_arg(args, "expand_shards").unwrap_or(false);
    let mut plain: Vec<Row> = Vec::new();
    // prefix -> (count, min suffix, max suffix, total rows, total bytes)
    let mut shards: BTreeMap<String, Shard> = BTreeMap::new();

    for item in &items {
        let id = item
            .get("tableReference")
            .and_then(|r| r.get("tableId"))
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let kind = match item.get("type").and_then(Value::as_str).unwrap_or("TABLE") {
            "VIEW" => "view",
            "MATERIALIZED_VIEW" => "matview",
            "EXTERNAL" => "external",
            "SNAPSHOT" => "snapshot",
            _ => "table",
        }
        .to_string();
        let rows = item.get("numRows").and_then(as_u64_loose);
        let bytes = item.get("numBytes").and_then(as_u64_loose);

        match (expand, split_shard(&id)) {
            (false, Some((prefix, suffix))) => {
                let entry = shards.entry(prefix).or_insert_with(|| Shard {
                    count: 0,
                    first: suffix.clone(),
                    last: suffix.clone(),
                    rows: 0,
                    bytes: 0,
                    known: true,
                });
                entry.count += 1;
                if suffix < entry.first {
                    entry.first = suffix.clone();
                }
                if suffix > entry.last {
                    entry.last = suffix;
                }
                // Sizes are absent from a bare listing for some table kinds;
                // track whether the total is trustworthy rather than implying
                // a shard set is empty.
                match (rows, bytes) {
                    (Some(r), Some(b)) => {
                        entry.rows += r;
                        entry.bytes += b;
                    }
                    _ => entry.known = false,
                }
            }
            _ => plain.push(Row { id, kind, rows, bytes }),
        }
    }

    let mut out = String::new();
    let total = plain.len() + shards.len();
    out.push_str(&format!("{total} entr(ies) in {project}.{dataset}:\n\n"));

    // `tables.list` does not return sizes — verified against the API, which
    // omits numRows/numBytes entirely here. So only print those columns when
    // something actually filled them in, rather than a wall of placeholders.
    let any_sizes = plain.iter().any(|r| r.bytes.is_some() || r.rows.is_some())
        || shards.values().any(|s| s.known);

    for (prefix, shard) in &shards {
        let size = if any_sizes && shard.known {
            format!(
                " {:>10}  {:>12} rows",
                human_bytes(shard.bytes),
                commas(shard.rows)
            )
        } else {
            String::new()
        };
        out.push_str(&format!(
            "  {:<40} {:<9}{size}   {} shards, {}..{}\n",
            format!("{prefix}_*"),
            "sharded",
            shard.count,
            shard.first,
            shard.last
        ));
    }

    for row in &plain {
        let size = match (any_sizes, row.bytes, row.rows) {
            (true, Some(bytes), Some(rows)) => {
                format!(" {:>10}  {:>12} rows", human_bytes(bytes), commas(rows))
            }
            // A view has no size of its own, and saying "0 B" would be a lie.
            (true, _, _) => format!(" {:>10}  {:>12}", "—", "—"),
            (false, _, _) => String::new(),
        };
        out.push_str(&format!("  {:<40} {:<9}{size}\n", row.id, row.kind));
    }

    if !any_sizes {
        out.push_str(
            "\nRow counts and sizes are not part of a listing; `bq-describe` reports them \
             per table, also for free.\n",
        );
    }

    if !shards.is_empty() {
        out.push_str(
            "\nSharded sets are shown collapsed. Query them with a wildcard and filter on \
             _TABLE_SUFFIX to limit the scan — `expand_shards` lists each one.\n",
        );
    }
    if items.len() as u64 >= limit {
        out.push_str(&format!(
            "\nStopped at the limit of {limit}; raise `limit` for more.\n"
        ));
    }

    Ok(out)
}

struct Row {
    id: String,
    kind: String,
    rows: Option<u64>,
    bytes: Option<u64>,
}

struct Shard {
    count: u64,
    first: String,
    last: String,
    rows: u64,
    bytes: u64,
    known: bool,
}

/// Recognises `name_YYYYMMDD`, the date-sharding convention BigQuery's own
/// wildcard tables and the GA4 export use.
///
/// Deliberately strict: only an 8-digit suffix that plausibly parses as a date
/// counts, so a table genuinely called `revenue_2024` or `model_v2` is left
/// alone.
fn split_shard(id: &str) -> Option<(String, String)> {
    let (prefix, suffix) = id.rsplit_once('_')?;
    if prefix.is_empty() || suffix.len() != 8 || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let month: u32 = suffix[4..6].parse().ok()?;
    let day: u32 = suffix[6..8].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((prefix.to_string(), suffix.to_string()))
}

/// Walks a paginated list endpoint until it runs dry or `limit` is reached.
fn paginate(
    client: &Bq,
    path: &str,
    key: &str,
    limit: u64,
    extra: &[(String, String)],
) -> Result<Vec<Value>, String> {
    let mut collected: Vec<Value> = Vec::new();
    let mut token: Option<String> = None;

    while (collected.len() as u64) < limit {
        let want = (limit - collected.len() as u64).min(1000);
        let mut query = extra.to_vec();
        query.push(("maxResults".to_string(), want.to_string()));
        if let Some(token) = &token {
            query.push(("pageToken".to_string(), token.clone()));
        }

        let response = client.get(path, &query)?;
        let items = response
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let count = items.len();
        collected.extend(items);

        token = response
            .get("nextPageToken")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        if token.is_none() || count == 0 {
            break;
        }
    }

    collected.truncate(limit as usize);
    Ok(collected)
}

export!(Component);
