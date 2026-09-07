//! Profile a table's columns in one pass.
//!
//! ## Why this tool exists
//!
//! This is the "draw conclusions from raw data" tool. Before you can trust a
//! column you need to know: how often is it null, how many distinct values does
//! it take, what is its range, and what are the common values. Asked by hand
//! that is one query per column per statistic — ten round trips, each a full
//! scan of the column, each billed.
//!
//! BigQuery will compute all of it in a **single scan**, because the aggregates
//! are independent and the engine reads each column once regardless of how many
//! functions are applied to it. So the profile of a fifty-column table costs
//! roughly what one `SELECT *` would, and answers questions that `SELECT *`
//! cannot.
//!
//! Three deliberate choices:
//!
//! * **`APPROX_QUANTILES` and `APPROX_COUNT_DISTINCT`**, not exact. Both are
//!   sketch-based, need no sort and no shuffle, and are within a percent or so
//!   — the right trade when the question is "what does this look like".
//! * **Top values in a second query, not the first.** Top-K needs a GROUP BY per
//!   column, which does not compose into one row. It is issued only for columns
//!   whose cardinality the first pass showed to be low enough to be interesting.
//! * **`TABLESAMPLE` for big tables.** Past a threshold the profile samples
//!   rather than scanning, because a shape is visible in 1% of a terabyte and
//!   costs a hundredth as much.

wit_bindgen::generate!({
    world: "tool",
    path: "../../wit",
    generate_all,
});

mod bq;

use bq::{
    as_f64_loose, as_u64_loose, bool_arg, clip, commas, decode_rows, estimate_cost, human_bytes,
    parse_schema, qualify_table, render_cost, str_arg, u64_arg, Bq, Field,
};
use serde_json::{json, Value};
use thetis::grip::types::LogLevel;

const POLL_TIMEOUT_MS: u64 = 60_000;
/// Columns profiled unless told otherwise. The SQL grows with this, and past a
/// few dozen the output stops being readable before the query stops being
/// affordable.
const DEFAULT_MAX_COLUMNS: usize = 40;
/// Distinct values at or below which a column is worth a top-K breakdown.
const CATEGORICAL_LIMIT: u64 = 50;
/// Above this size, sample rather than scan unless overridden.
const SAMPLE_THRESHOLD_BYTES: u64 = 20 * 1024 * 1024 * 1024;

struct Component;

impl Guest for Component {
    fn describe() -> ToolManifest {
        ToolManifest {
            name: "bq-profile".to_string(),
            description: "Profile the columns of a BigQuery table in one scan: null rate, \
                distinct count, min, max, mean and approximate quantiles for numerics, and \
                the most common values for low-cardinality columns. This is the tool for \
                understanding what data actually contains — one call replaces a dozen \
                hand-written aggregate queries, and costs a single scan rather than one per \
                column. Samples automatically on tables over 20 GiB. Run `bq-describe` \
                first if you only need the schema, which is free. Reaches \
                bigquery.googleapis.com."
                .to_string(),
            args_schema_json: json!({
                "type": "object",
                "properties": {
                    "table": {
                        "type": "string",
                        "description": "The table, as 'project.dataset.table' or 'dataset.table'. A wildcard like 'events_*' works."
                    },
                    "dataset": {
                        "type": "string",
                        "description": "Dataset, if not included in `table`."
                    },
                    "project": {
                        "type": "string",
                        "description": "GCP project. Defaults to the configured project."
                    },
                    "columns": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Only profile these columns. Strongly recommended on a wide table: fewer columns means a smaller scan and a much shorter answer."
                    },
                    "where": {
                        "type": "string",
                        "description": "A SQL predicate to restrict the rows profiled, e.g. \"_TABLE_SUFFIX BETWEEN '20240301' AND '20240307'\". On a partitioned or sharded table this is what keeps the scan small."
                    },
                    "sample_percent": {
                        "type": "number",
                        "description": "Profile this percentage of rows via TABLESAMPLE, e.g. 1 for 1%. Set 100 to force a full scan. Defaults to sampling only if the table is large."
                    },
                    "top_values": {
                        "type": "boolean",
                        "description": "Include the most common values for low-cardinality columns. Default true; costs a second query."
                    },
                    "max_columns": {
                        "type": "integer",
                        "description": "Cap on how many columns to profile. Default 40."
                    },
                    "location": {
                        "type": "string",
                        "description": "Dataset location. Only needed outside the US/EU multi-regions."
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
            // Runs SELECTs only, but they are generated, so the description is
            // explicit that this one spends money.
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
        // Identifiers go into generated SQL, so they are validated rather than
        // trusted. This is the chokepoint.
        let reference = qualify_table(&project, &dataset, &table)?;

        // --- Schema, free, and needed to know what to aggregate -------------
        let meta = fetch_schema(&client, &project, &dataset, &table)?;
        let all_fields = parse_schema(meta.get("schema").unwrap_or(&Value::Null));
        if all_fields.is_empty() {
            return Err(format!("{reference} has no readable schema."));
        }

        let requested: Vec<String> = args
            .get("columns")
            .and_then(Value::as_array)
            .map(|columns| {
                columns
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|c| c.trim().to_string())
                    .filter(|c| !c.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let max_columns = u64_arg(&args, "max_columns")
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_COLUMNS)
            .max(1);

        // Profile leaf scalars: a STRUCT has no aggregate of its own, and an
        // ARRAY needs unnesting, which changes the row count and so cannot be
        // mixed into a single-row aggregate.
        let mut targets: Vec<Target> = Vec::new();
        collect_targets(&all_fields, "", &mut targets);
        let skipped_repeated: Vec<String> = targets
            .iter()
            .filter(|t| t.repeated)
            .map(|t| t.path.clone())
            .collect();
        targets.retain(|t| !t.repeated);

        if !requested.is_empty() {
            let wanted: Vec<String> = requested.iter().map(|c| c.to_ascii_lowercase()).collect();
            targets.retain(|t| wanted.contains(&t.path.to_ascii_lowercase()));
            if targets.is_empty() {
                return Err(format!(
                    "none of {requested:?} are scalar columns on {reference}. \
                     `bq-describe` lists the real names; nested fields are addressed as \
                     'parent.child'."
                ));
            }
        }

        let truncated = targets.len() > max_columns;
        targets.truncate(max_columns);

        // --- Sampling decision ---------------------------------------------
        let table_bytes = meta.get("numBytes").and_then(as_u64_loose).unwrap_or(0);
        let sample = match args.get("sample_percent").and_then(as_f64_loose) {
            Some(percent) if percent > 0.0 && percent < 100.0 => Some(percent),
            Some(_) => None,
            // Automatic: a shape is visible in a fraction of a large table, and
            // the caller did not ask to pay for certainty.
            None if table_bytes > SAMPLE_THRESHOLD_BYTES => Some(1.0),
            None => None,
        };

        let predicate = str_arg(&args, "where");
        let from = build_from(&reference, sample, predicate.as_deref());

        // --- Pass 1: every aggregate, one row, one scan ----------------------
        let mut projections = vec!["COUNT(*) AS __rows".to_string()];
        for (index, target) in targets.iter().enumerate() {
            projections.extend(target.projections(index));
        }
        let sql = format!("SELECT\n  {}\n{from}", projections.join(",\n  "));

        let response = run(&client, &project, &sql, &args)?;
        let fields = parse_schema(response.get("schema").unwrap_or(&Value::Null));
        let rows = decode_rows(
            &fields,
            response
                .get("rows")
                .and_then(Value::as_array)
                .unwrap_or(&vec![]),
        );
        let stats = rows
            .first()
            .cloned()
            .ok_or_else(|| "the profile query returned no rows".to_string())?;

        let total = as_u64_loose(stats.get("__rows").unwrap_or(&Value::Null)).unwrap_or(0);
        let mut scanned = response
            .get("totalBytesProcessed")
            .and_then(as_u64_loose)
            .unwrap_or(0);

        // --- Pass 2: top values, only where it makes sense ------------------
        let want_top = bool_arg(&args, "top_values").unwrap_or(true);
        let mut top_values: Vec<(String, Vec<(String, u64)>)> = Vec::new();
        if want_top && total > 0 {
            let categorical: Vec<&Target> = targets
                .iter()
                .enumerate()
                .filter(|(index, target)| {
                    if !target.top_worthy() {
                        return false;
                    }
                    let distinct =
                        as_u64_loose(stats.get(&format!("c{index}_distinct")).unwrap_or(&Value::Null))
                            .unwrap_or(u64::MAX);
                    // Only worth showing when the values repeat: a column with
                    // one distinct value per row has no "common values".
                    distinct > 0 && distinct <= CATEGORICAL_LIMIT
                })
                .map(|(_, target)| target)
                .collect();

            if !categorical.is_empty() {
                match top_query(&client, &project, &from, &categorical, &args) {
                    Ok((values, bytes)) => {
                        top_values = values;
                        scanned += bytes;
                    }
                    // A failed second pass should not lose the first pass's
                    // results, which are the bulk of the answer.
                    Err(error) => {
                        thetis::grip::sys::log(
                            LogLevel::Warn,
                            &format!("bq-profile: top values failed: {error}"),
                        );
                    }
                }
            }
        }

        // --- Render ---------------------------------------------------------
        let mut out = String::new();
        out.push_str(&format!("{reference}\n{}\n\n", "=".repeat(50)));
        out.push_str(&format!("rows profiled: {}\n", commas(total)));
        if let Some(percent) = sample {
            let table_rows = meta.get("numRows").and_then(as_u64_loose);
            out.push_str(&format!(
                "sampled:       {percent}% of the table{}\n",
                table_rows
                    .map(|r| format!(" (~{} rows)", commas(r)))
                    .unwrap_or_default()
            ));
        }
        if let Some(predicate) = &predicate {
            out.push_str(&format!("filtered:      WHERE {}\n", clip(predicate, 120)));
        }
        out.push_str(&format!(
            "scanned:       {} · {}\n\n",
            human_bytes(scanned),
            render_cost(estimate_cost(scanned, client.price_per_tib))
        ));

        if total == 0 {
            out.push_str("No rows matched, so there is nothing to profile.\n");
            return Ok(out);
        }

        for (index, target) in targets.iter().enumerate() {
            out.push_str(&target.render(index, &stats, total, &top_values));
        }

        if truncated {
            out.push_str(&format!(
                "\nStopped at {max_columns} columns. Pass `columns` to choose which, or \
                 raise `max_columns`.\n"
            ));
        }
        if !skipped_repeated.is_empty() {
            out.push_str(&format!(
                "\nSkipped {} repeated field(s), which need UNNEST and change the row \
                 count: {}\n",
                skipped_repeated.len(),
                clip(&skipped_repeated.join(", "), 200)
            ));
        }

        Ok(out)
    }
}

/// One scalar column to profile.
struct Target {
    /// Dotted path, which is also valid SQL for reaching the field.
    path: String,
    ty: String,
    repeated: bool,
}

impl Target {
    fn numeric(&self) -> bool {
        matches!(
            self.ty.as_str(),
            "INTEGER" | "INT64" | "FLOAT" | "FLOAT64" | "NUMERIC" | "BIGNUMERIC"
        )
    }
    fn temporal(&self) -> bool {
        matches!(
            self.ty.as_str(),
            "TIMESTAMP" | "DATE" | "DATETIME" | "TIME"
        )
    }
    fn orderable(&self) -> bool {
        self.numeric() || self.temporal() || matches!(self.ty.as_str(), "STRING" | "BOOLEAN" | "BOOL")
    }
    fn top_worthy(&self) -> bool {
        matches!(
            self.ty.as_str(),
            "STRING" | "BOOLEAN" | "BOOL" | "INTEGER" | "INT64" | "DATE"
        )
    }

    /// Backtick-quoted path, so a column named `order` or `select` still works.
    fn sql(&self) -> String {
        self.path
            .split('.')
            .map(|part| format!("`{part}`"))
            .collect::<Vec<_>>()
            .join(".")
    }

    /// The aggregates for this column, aliased by index so the results can be
    /// found again without depending on name mangling.
    fn projections(&self, index: usize) -> Vec<String> {
        let column = self.sql();
        let mut out = vec![
            format!("COUNTIF({column} IS NULL) AS c{index}_nulls"),
            format!("APPROX_COUNT_DISTINCT({column}) AS c{index}_distinct"),
        ];

        if self.numeric() {
            out.push(format!("MIN({column}) AS c{index}_min"));
            out.push(format!("MAX({column}) AS c{index}_max"));
            out.push(format!("AVG(CAST({column} AS FLOAT64)) AS c{index}_mean"));
            out.push(format!(
                "STDDEV(CAST({column} AS FLOAT64)) AS c{index}_stddev"
            ));
            // Quartiles: enough to see skew and outliers without a wall of
            // numbers. IGNORE NULLS or the whole array comes back null.
            out.push(format!(
                "APPROX_QUANTILES(CAST({column} AS FLOAT64), 4 IGNORE NULLS) AS c{index}_quartiles"
            ));
        } else if self.temporal() {
            out.push(format!("CAST(MIN({column}) AS STRING) AS c{index}_min"));
            out.push(format!("CAST(MAX({column}) AS STRING) AS c{index}_max"));
        } else if self.ty == "STRING" {
            out.push(format!("MIN({column}) AS c{index}_min"));
            out.push(format!("MAX({column}) AS c{index}_max"));
            // Length distribution catches padding, truncation and junk values
            // that a min/max of the text itself hides.
            out.push(format!(
                "MIN(LENGTH({column})) AS c{index}_minlen"
            ));
            out.push(format!(
                "MAX(LENGTH({column})) AS c{index}_maxlen"
            ));
            out.push(format!(
                "COUNTIF({column} = '') AS c{index}_empty"
            ));
        } else if matches!(self.ty.as_str(), "BOOLEAN" | "BOOL") {
            out.push(format!("COUNTIF({column}) AS c{index}_true"));
        }

        out
    }

    fn render(
        &self,
        index: usize,
        stats: &Value,
        total: u64,
        top_values: &[(String, Vec<(String, u64)>)],
    ) -> String {
        let get = |suffix: &str| stats.get(&format!("c{index}_{suffix}")).cloned().unwrap_or(Value::Null);
        let nulls = as_u64_loose(&get("nulls")).unwrap_or(0);
        let distinct = as_u64_loose(&get("distinct")).unwrap_or(0);
        let null_percent = if total > 0 {
            nulls as f64 / total as f64 * 100.0
        } else {
            0.0
        };

        let mut out = format!("{}  ({})\n", self.path, self.ty);
        out.push_str(&format!(
            "  nulls    {} ({:.1}%)\n",
            commas(nulls),
            null_percent
        ));
        out.push_str(&format!("  distinct ~{}\n", commas(distinct)));

        // A column that is entirely null has no range or values worth printing,
        // and saying so plainly is more useful than five lines of NULL.
        if nulls == total {
            out.push_str("  entirely null\n\n");
            return out;
        }

        if self.orderable() {
            let min = render_scalar(&get("min"));
            let max = render_scalar(&get("max"));
            if min != "NULL" || max != "NULL" {
                out.push_str(&format!("  range    {min} .. {max}\n"));
            }
        }

        if self.numeric() {
            if let Some(mean) = as_f64_loose(&get("mean")) {
                let stddev = as_f64_loose(&get("stddev"));
                out.push_str(&format!(
                    "  mean     {}{}\n",
                    trim_float(mean),
                    stddev
                        .map(|s| format!("  (sd {})", trim_float(s)))
                        .unwrap_or_default()
                ));
            }
            if let Some(quartiles) = get("quartiles").as_array() {
                let values: Vec<String> = quartiles
                    .iter()
                    .filter_map(as_f64_loose)
                    .map(trim_float)
                    .collect();
                if values.len() == 5 {
                    out.push_str(&format!(
                        "  quartiles p25 {}  median {}  p75 {}\n",
                        values[1], values[2], values[3]
                    ));
                }
            }
        }

        if self.ty == "STRING" {
            let empty = as_u64_loose(&get("empty")).unwrap_or(0);
            let minlen = as_u64_loose(&get("minlen"));
            let maxlen = as_u64_loose(&get("maxlen"));
            if let (Some(min), Some(max)) = (minlen, maxlen) {
                out.push_str(&format!("  length   {min} .. {max}\n"));
            }
            if empty > 0 {
                // An empty string is not a null, and conflating the two is a
                // classic source of wrong conclusions.
                out.push_str(&format!(
                    "  empty    {} ({:.1}%) — empty strings, distinct from NULL\n",
                    commas(empty),
                    empty as f64 / total as f64 * 100.0
                ));
            }
        }

        if matches!(self.ty.as_str(), "BOOLEAN" | "BOOL") {
            if let Some(trues) = as_u64_loose(&get("true")) {
                let non_null = total.saturating_sub(nulls);
                let percent = if non_null > 0 {
                    trues as f64 / non_null as f64 * 100.0
                } else {
                    0.0
                };
                out.push_str(&format!(
                    "  true     {} of {} ({:.1}%)\n",
                    commas(trues),
                    commas(non_null),
                    percent
                ));
            }
        }

        if let Some((_, values)) = top_values.iter().find(|(path, _)| path == &self.path) {
            if !values.is_empty() {
                out.push_str("  top values\n");
                for (value, count) in values.iter().take(10) {
                    out.push_str(&format!(
                        "    {:<40} {:>12}  {:>5.1}%\n",
                        clip(value, 40),
                        commas(*count),
                        *count as f64 / total as f64 * 100.0
                    ));
                }
            }
        }

        out.push('\n');
        out
    }
}

/// Walks the schema for leaf scalars, recording the dotted path to each.
fn collect_targets(fields: &[Field], prefix: &str, out: &mut Vec<Target>) {
    for field in fields {
        let path = if prefix.is_empty() {
            field.name.clone()
        } else {
            format!("{prefix}.{}", field.name)
        };

        if field.is_repeated() {
            // Recorded so the report can say what it skipped, rather than
            // silently omitting columns.
            out.push(Target {
                path,
                ty: field.ty.clone(),
                repeated: true,
            });
            continue;
        }
        if field.is_record() {
            collect_targets(&field.fields, &path, out);
            continue;
        }
        out.push(Target {
            path,
            ty: field.ty.clone(),
            repeated: false,
        });
    }
}

/// The FROM clause, with sampling and filtering applied.
///
/// TABLESAMPLE must precede WHERE: it selects blocks before the predicate is
/// evaluated, which is exactly why it is cheap.
fn build_from(reference: &str, sample: Option<f64>, predicate: Option<&str>) -> String {
    let mut from = format!("FROM {reference}");
    if let Some(percent) = sample {
        from.push_str(&format!(" TABLESAMPLE SYSTEM ({percent} PERCENT)"));
    }
    if let Some(predicate) = predicate {
        from.push_str(&format!("\nWHERE {predicate}"));
    }
    from
}

/// Top-K per categorical column, as one query of UNION ALL branches.
///
/// Each branch is its own GROUP BY, which cannot be folded into the
/// single-row aggregate of pass one. Unioning them keeps it to one job.
fn top_query(
    client: &Bq,
    project: &str,
    from: &str,
    targets: &[&Target],
    args: &Value,
) -> Result<(Vec<(String, Vec<(String, u64)>)>, u64), String> {
    let branches: Vec<String> = targets
        .iter()
        .map(|target| {
            format!(
                "SELECT '{}' AS column_name, CAST({} AS STRING) AS value, COUNT(*) AS n\n  {from}\n  GROUP BY value",
                target.path.replace('\'', "\\'"),
                target.sql()
            )
        })
        .collect();

    // Rank inside each column and keep the top ten, so one high-cardinality
    // column cannot crowd out the rest.
    let sql = format!(
        "WITH counts AS (\n{}\n),\nranked AS (\n  SELECT column_name, value, n,\n         ROW_NUMBER() OVER (PARTITION BY column_name ORDER BY n DESC) AS rank\n  FROM counts\n)\nSELECT column_name, value, n FROM ranked WHERE rank <= 10\nORDER BY column_name, n DESC",
        branches.join("\n  UNION ALL\n")
    );

    let response = run(client, project, &sql, args)?;
    let fields = parse_schema(response.get("schema").unwrap_or(&Value::Null));
    let rows = decode_rows(
        &fields,
        response
            .get("rows")
            .and_then(Value::as_array)
            .unwrap_or(&vec![]),
    );

    let mut grouped: Vec<(String, Vec<(String, u64)>)> = Vec::new();
    for row in &rows {
        let column = row
            .get("column_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let value = match row.get("value") {
            Some(Value::Null) | None => "NULL".to_string(),
            Some(Value::String(text)) if text.is_empty() => "(empty string)".to_string(),
            Some(other) => render_scalar(other),
        };
        let count = as_u64_loose(row.get("n").unwrap_or(&Value::Null)).unwrap_or(0);

        match grouped.iter_mut().find(|(name, _)| name == &column) {
            Some((_, values)) => values.push((value, count)),
            None => grouped.push((column, vec![(value, count)])),
        }
    }

    let bytes = response
        .get("totalBytesProcessed")
        .and_then(as_u64_loose)
        .unwrap_or(0);
    Ok((grouped, bytes))
}

/// Runs a generated query and waits for it.
fn run(client: &Bq, project: &str, sql: &str, args: &Value) -> Result<Value, String> {
    let mut request = json!({
        "query": sql,
        "useLegacySql": false,
        "timeoutMs": POLL_TIMEOUT_MS,
        "maxResults": 1000,
        "useQueryCache": true,
    });
    if let Some(location) = str_arg(args, "location").or_else(|| client.location.clone()) {
        request["location"] = json!(location);
    }
    if let Some(cap) = client.max_bytes_billed {
        request["maximumBytesBilled"] = json!(cap.to_string());
    }

    let mut response = client.post(&format!("/projects/{project}/queries"), &request)?;

    let job_id = response
        .get("jobReference")
        .and_then(|r| r.get("jobId"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let location = response
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
        if job_id.is_empty() || polls >= 10 {
            return Err(format!(
                "the profile query is taking a long time. It is still running as job \
                 {job_id}; collect it with `bq-results`, or narrow the profile with \
                 `columns`, `where` or `sample_percent`."
            ));
        }
        polls += 1;
        let mut query = vec![
            ("maxResults".to_string(), "1000".to_string()),
            ("timeoutMs".to_string(), POLL_TIMEOUT_MS.to_string()),
        ];
        if let Some(location) = &location {
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
        return Err(format!("the profile query failed: {message}"));
    }

    Ok(response)
}

/// Schema for a table, tolerating a wildcard reference by resolving one shard.
fn fetch_schema(
    client: &Bq,
    project: &str,
    dataset: &str,
    table: &str,
) -> Result<Value, String> {
    if !table.contains('*') {
        return client.get(
            &format!("/projects/{project}/datasets/{dataset}/tables/{table}"),
            &[],
        );
    }

    // tables.get cannot resolve a wildcard, so find a real shard that matches
    // the prefix and read its schema — shards of a set share one schema.
    let prefix = table.trim_end_matches('*');
    let listing = client.get(
        &format!("/projects/{project}/datasets/{dataset}/tables"),
        &[("maxResults".to_string(), "5000".to_string())],
    )?;
    let candidate = listing
        .get("tables")
        .and_then(Value::as_array)
        .and_then(|tables| {
            tables
                .iter()
                .filter_map(|entry| {
                    entry
                        .get("tableReference")
                        .and_then(|r| r.get("tableId"))
                        .and_then(Value::as_str)
                })
                .filter(|id| id.starts_with(prefix))
                // The last shard is the most likely to have the current schema.
                .max()
                .map(str::to_string)
        })
        .ok_or_else(|| {
            format!(
                "no tables in {project}.{dataset} start with {prefix:?}, so the wildcard \
                 matches nothing."
            )
        })?;

    let mut meta = client.get(
        &format!("/projects/{project}/datasets/{dataset}/tables/{candidate}"),
        &[],
    )?;
    // The size of one shard would badly misinform the sampling decision, so
    // drop it rather than let it be read as the size of the set.
    if let Some(object) = meta.as_object_mut() {
        object.remove("numBytes");
        object.remove("numRows");
    }
    Ok(meta)
}

fn render_scalar(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::String(text) => text.clone(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

/// Formats a float without a long tail of noise digits.
fn trim_float(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    let formatted = format!("{value:.4}");
    formatted.trim_end_matches('0').trim_end_matches('.').to_string()
}

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
